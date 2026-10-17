//! Context delivery before every agent session.
//!
//! A child module of `executor`. Order, with no transaction open:
//! `retrieve_for_step` (P4) → `compile` (with the hand-off as P1 when the
//! session continues another's work) → `ContextPack::redact` → `store_pack`
//! (blob) → `HostLink::write_files` (`.sun/context/*`, `.sun/progress.md`).
//! The `context_pack` row and its timeline event are written in the
//! transaction that records the session start.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Serialize;
use serde_json::json;
use sun_context::knowledge::{KnowledgeFlag, Scope, retrieve, unique_flags};
use sun_context::{DesignFile, HandoffPack, LedgerView, McpSessions, compile, store_pack};
use sun_core::host_protocol::{PREVIEW_FILE, SessionFile, Worktree};
use sun_core::model::{self, Step};
use sun_core::{HostId, HostLink, McpGrant, Result, TaskId, render_mcp_config};
use sun_store::{SecretRepo, SettingRepo, TaskRepo};

use super::repo_source::{cat, repo_dir};
use super::{Executor, step_brief};
use crate::gates::quote;
use crate::knowledge::{
    CONTEXT_WINDOW, KNOWLEDGE_DOCS, over_budget, retrieve_scoped, task_sources, workspace_sources,
};

/// Where the session's MCP config is written, relative to its worktree
/// (`.sun/context/` is git-ignored and rewritten for every session).
const MCP_CONFIG: &str = ".sun/context/mcp.json";

/// The `sun` MCP server every agent session gets: the engine
/// issues a token per session from `sessions` and writes an MCP config that
/// runs `command args…` (the `sun mcp` stdio shim, which dials the kernel)
/// with the token in `SUN_MCP_TOKEN`. The token is revoked when the session
/// ends. Remote hosts run `sun` from their own PATH (the kernel's path means
/// nothing there); only the host inside the kernel's process gets `local`.
#[derive(Clone)]
pub struct SessionMcp {
    /// The kernel's token registry, shared with the transport that opens them.
    pub sessions: Arc<McpSessions>,
    /// The shim program on remote hosts, normally `sun` (looked up on the
    /// host's PATH, where `sun host` is installed).
    pub command: String,
    /// The in-process host (`serve --with-host`) and the kernel's own binary,
    /// which it runs instead of `command`.
    pub local: Option<(HostId, String)>,
    /// Its arguments, e.g. `["mcp", "--kernel", "http://127.0.0.1:7700"]`.
    /// `args… relay <grant>` reaches a granted MCP server through the
    /// kernel's relay (`GET /mcp/{server}`), so grants work on any host.
    pub args: Vec<String>,
}

impl std::fmt::Debug for SessionMcp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionMcp")
            .field("command", &self.command)
            .field("args", &self.args)
            .finish_non_exhaustive()
    }
}

impl SessionMcp {
    /// The config file for a session holding `token` (Claude's format; the
    /// adapters render it for Codex and OpenCode): the `sun` server plus one
    /// `sun mcp relay` entry per grant, each with the session's token.
    pub(super) fn file(
        &self,
        token: &str,
        host: HostId,
        grants: &[McpGrant],
    ) -> Result<SessionFile> {
        let command = match &self.local {
            Some((local, exe)) if *local == host => exe,
            _ => &self.command,
        };
        let mut relay: Vec<&str> = self.args.iter().map(String::as_str).collect();
        relay.push("relay");
        let mut config = render_mcp_config(grants, command, &relay)?;
        for grant in grants {
            config["mcpServers"][&grant.server]["env"] = json!({ "SUN_MCP_TOKEN": token });
        }
        config["mcpServers"]["sun"] = json!({
            "command": command,
            "args": self.args,
            "env": { "SUN_MCP_TOKEN": token },
        });
        Ok(SessionFile {
            path: MCP_CONFIG.into(),
            contents: config.to_string(),
            overwrite: true,
        })
    }
}

/// Most screens taken from `.sun-preview`.
const MAX_SCREENS: usize = 12;
/// Token files read besides `DESIGN.md`.
const MAX_TOKEN_FILES: usize = 4;
/// Bytes read per design file; host exec keeps a 64 KiB output tail.
const MAX_DESIGN_BYTES: usize = 60 * 1024;
/// Token files git tracks, as pathspecs (any depth).
const TOKEN_FILES: &str = "'*tokens*.css' '*tokens*.scss' '*tokens*.json'";

/// A canvas screen: a page path of the preview.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ScreenPath {
    /// Label, e.g. `Pricing` (the path when `.sun-preview` gives none).
    pub name: String,
    /// Path and query under the preview base, starting with `/`.
    pub path: String,
}

/// The repo's canvas screens and design system, read from the task
/// branch: [`PREVIEW_FILE`]'s screen lines, `DESIGN.md` and token files.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Design {
    /// At least one; `/` when the repo lists none.
    pub screens: Vec<ScreenPath>,
    /// `DESIGN.md` first, then up to [`MAX_TOKEN_FILES`] token files.
    pub files: Vec<DesignFile>,
}

/// The screens listed after the command line of a `.sun-preview` file:
/// `<path> [name]` per line, blank and `#` lines skipped, invalid paths
/// dropped, duplicates once. `/` when none is left.
pub(crate) fn screens(preview: &str) -> Vec<ScreenPath> {
    let mut out: Vec<ScreenPath> = Vec::new();
    for line in preview.lines().skip(1).map(str::trim) {
        let (path, name) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
        let ok = path.starts_with('/') && path.len() <= 1000 && !path.contains(char::is_control);
        if !ok || out.iter().any(|s| s.path == path) || out.len() == MAX_SCREENS {
            continue;
        }
        let name = name.trim();
        out.push(ScreenPath {
            name: if name.is_empty() { path } else { name }
                .chars()
                .take(80)
                .collect(),
            path: path.to_owned(),
        });
    }
    if out.is_empty() {
        out.push(ScreenPath {
            name: "/".into(),
            path: "/".into(),
        });
    }
    out
}

/// Reads the task's [`Design`] on `host`. A task without a repo, or a host
/// that can't be asked, has the default screen and no design files.
pub(crate) async fn read_design(hosts: &dyn HostLink, host: HostId, worktree: &Worktree) -> Design {
    let mut design = Design {
        screens: screens(""),
        files: Vec::new(),
    };
    let Some(cwd) = repo_dir(hosts, host, worktree).await else {
        return design;
    };
    // `.sun-preview`'s text, a NUL, then the design files (one per line).
    let list = format!(
        "head -c 4096 {PREVIEW_FILE} 2>/dev/null; printf '\\0'; \
         git ls-files -- DESIGN.md 2>/dev/null; \
         git ls-files -- {TOKEN_FILES} 2>/dev/null | head -n {MAX_TOKEN_FILES}; true"
    );
    let Some(list) = cat(hosts, host, &cwd, list).await else {
        return design;
    };
    let (preview, files) = list.split_once('\0').unwrap_or(("", &list));
    design.screens = screens(preview);
    // ponytail: one exec per file (at most 5); batch them if it shows in session start-up.
    for path in files.lines().filter(|p| !p.is_empty()) {
        let read = format!(
            "head -c {MAX_DESIGN_BYTES} -- {}",
            quote(&format!("./{path}"))
        );
        if let Some(text) = cat(hosts, host, &cwd, read).await {
            design.files.push(DesignFile {
                path: path.to_owned(),
                text,
            });
        }
    }
    design
}

impl crate::Engine {
    /// The task's canvas screens and design system, from its branch.
    pub async fn design(&self, id: TaskId) -> Result<Design> {
        let worktree = self.worktree(id).await?;
        Ok(read_design(&*self.hosts, self.host, &worktree).await)
    }

    /// Stale and conflicting sources among the knowledge the task's text
    /// retrieves, shown on the task before and during its run. Steps
    /// get the same rules for their own query in their pack's warnings.
    pub async fn knowledge_flags(&self, id: TaskId) -> Result<Vec<KnowledgeFlag>> {
        let mut tx = self.store.read().await?;
        let task = tx.get_task(id).await?;
        let sources = workspace_sources(&mut tx, self.workspace_of_task(id).await?, None).await?;
        drop(tx);
        let query = format!("{} {}", task.title, task.body);
        let scope = Scope {
            sources: Some(&sources),
            embedder: None,
        };
        let hits = retrieve(&self.store, &query, scope, KNOWLEDGE_DOCS).await?;
        Ok(unique_flags(&hits))
    }
}

/// What a session is given: the first prompt and the stored pack row.
pub(super) struct Delivered {
    pub envelope: String,
    /// The envelope split for the runtime's template; `None` when the whole
    /// pack rides inline in `envelope`.
    pub envelope_parts: Option<sun_core::Envelope>,
    pub row: model::ContextPack,
    pub summary: String,
    /// The MCP config's host path, when one was written.
    pub mcp_config: Option<PathBuf>,
}

impl Executor {
    /// Compiles, stores and writes the step's context pack in `worktree`,
    /// plus the `mcp` config file if given.
    /// `prompt` (the step goal plus any feedback or notice) becomes P0's goal.
    /// When the host can't write files the whole pack rides in the envelope.
    pub(super) async fn deliver_context(
        &self,
        step: &Step,
        adapter: &str,
        worktree: &Path,
        prompt: &str,
        handoff: Option<HandoffPack>,
        mcp: Option<SessionFile>,
    ) -> Result<Delivered> {
        let mut read = self.store.read().await?;
        let mut brief = step_brief(&mut read, step.run_id, step.step_key.clone()).await?;
        let secrets = read.known_secrets().await?;
        // The task's sources and budget cap; the smaller budget wins.
        let selection = read.task_context(brief.task_id).await?;
        let task = read.get_task(brief.task_id).await?;
        let sources = task_sources(&mut read, &task).await?;
        let memory = crate::memory::active_for(&mut read, &task).await?;
        let workspace_context = crate::workspaces::workspace_context(&mut read, task.id).await?;
        let task_worktree = super::worktree_of(&mut read, brief.task_id).await?;
        // A chat answers and never clones, so it isn't told about the repos.
        let repos = if task.kind == model::TaskKind::Chat {
            Vec::new()
        } else {
            crate::vcs::workspace_repos(&mut read, brief.task_id).await?
        };
        drop(read);
        brief.goal = prompt.to_owned();
        // A multi-repo task: which folder is which repo.
        if let Some(repos) = crate::multi_repo::context(&task_worktree) {
            brief.goal = format!("{}\n\n{repos}", brief.goal);
        }
        brief.context_budget_tokens = match (brief.context_budget_tokens, selection.budget_tokens) {
            (Some(step), Some(task)) => Some(step.min(task)),
            (step, task) => step.or(task),
        };
        // Knowledge is optional: a failed search never blocks a session.
        let query = format!("{} {}", brief.goal, brief.title);
        let hits = retrieve_scoped(&self.store, &query, &sources).await;
        // The repo's design system rides along when it has one.
        let design = read_design(&*self.hosts, self.host(step.run_id), &task_worktree).await;
        let ledger = LedgerView {
            handoff,
            step_outputs: Vec::new(),
            design: design.files,
            workspace_context,
            repos,
        };
        // ponytail: no P2 outputs yet.
        // B = min(budget, 10% of the runtime's window).
        let window = self.context_window(adapter).unwrap_or(CONTEXT_WINDOW);
        let mut pack = compile(&brief, &ledger, &memory, &hits, window);
        // Block mode: the start fails (retried, then the step fails).
        if selection.block_over_budget
            && let Some(why) = over_budget(&pack)
        {
            return Err(sun_core::Error::InvalidInput(why));
        }
        let secrets: Vec<&str> = secrets.iter().map(String::as_str).collect();
        pack.redact(&secrets);
        let row = store_pack(self.store.blobs(), step.id, &pack).await?;
        let mut files = pack.files(ledger.handoff.as_ref());
        let mcp_config = mcp.map(|file| {
            let path = worktree.join(&file.path);
            files.push(file);
            path
        });
        let (envelope, envelope_parts, mcp_config) = match self
            .hosts
            .write_files(self.host(step.run_id), worktree.to_path_buf(), files)
            .await
        {
            Ok(()) => (
                pack.envelope(),
                Some(pack.envelope_parts(&brief, &secrets)),
                mcp_config,
            ),
            Err(_) => (pack.render_markdown(), None, None),
        };
        Ok(Delivered {
            envelope,
            envelope_parts,
            row,
            summary: pack.summary(),
            mcp_config,
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn sun_preview_lists_screens_after_the_command() {
        let screen = |path: &str, name: &str| ScreenPath {
            path: path.into(),
            name: name.into(),
        };
        let text =
            "npm run dev\n\n/  Home page\n# comment\npricing Bad\n/pricing?plan=pro\n/  Again\n";
        assert_eq!(
            screens(text),
            [
                screen("/", "Home page"),
                screen("/pricing?plan=pro", "/pricing?plan=pro")
            ]
        );
        assert_eq!(screens("npm run dev\n"), [screen("/", "/")]);
    }

    #[test]
    fn remote_hosts_run_sun_from_their_path_and_the_local_host_the_kernel_binary() {
        let local = HostId::generate();
        let mcp = SessionMcp {
            sessions: Arc::new(McpSessions::new()),
            command: "sun".into(),
            local: Some((local, "/opt/kernel/sun".into())),
            args: vec!["mcp".into()],
        };
        let grants = [McpGrant {
            server: "docs".into(),
            tools: None,
        }];
        let config = |host| {
            let file = mcp.file("t0k", host, &grants).unwrap();
            serde_json::from_str::<serde_json::Value>(&file.contents).unwrap()
        };
        let local_config = config(local);
        assert_eq!(
            local_config["mcpServers"]["sun"]["command"],
            "/opt/kernel/sun"
        );
        assert_eq!(
            local_config["mcpServers"]["docs"]["command"],
            "/opt/kernel/sun"
        );
        assert_eq!(
            local_config["mcpServers"]["docs"]["args"],
            json!(["mcp", "relay", "{\"server\":\"docs\"}"])
        );
        assert_eq!(
            local_config["mcpServers"]["docs"]["env"]["SUN_MCP_TOKEN"],
            "t0k"
        );
        let remote = config(HostId::generate());
        assert_eq!(remote["mcpServers"]["sun"]["command"], "sun");
        assert_eq!(remote["mcpServers"]["docs"]["command"], "sun");
    }
}
