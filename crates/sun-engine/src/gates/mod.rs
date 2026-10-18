//! Gates: the engine's verdict on finished work.
//!
//! **Pattern:** **Strategy**. Each `gates:` entry resolves to a [`Gate`]; every
//! variant checks differently and all yield a [`GateResult`] `{pass, output}`:
//! - code gates (`diff_not_empty`, `test`, `lint`, `diagnostics_clean`) run a
//!   shell command in the task worktree through [`HostLink::exec`];
//! - `pr_exists`, `ci_green` and dotted extension gates (`vendor.check`) ask a
//!   mod through [`ModLink`], which answers `{pass, output}`;
//! - content gates (`sections_present`, `length`, `format`, `links_resolve`,
//!   `terms`, `image_size`, `cites_sources`) read a worktree file from the
//!   host and check it in the engine;
//! - `visual_diff` compares preview screenshots of the base and task
//!   branches outside the task's scope (`visual`);
//! - `merge_order` checks a multi-repo task's PR merges against its order
//!   (`multi_repo`) and keeps the step waiting until all merged; a PR
//!   closed without merging fails it.
//!
//! In a multi-repo task code gates run in every repo's worktree
//! (`diff_not_empty` passes when any repo changed); `integration` runs its
//! `command` once in the session dir that holds them all.
//!
//! Lifecycle of a `gate` step attempt (the executor drives it):
//! `queued → running` (gates run off the engine task, as a [`Job`]) →
//! `verifying` → `succeeded` | `failed` | `running` (sent back). A send-back
//! queues a new attempt of `on_fail.send_back_to` (and of every step between
//! it and the gate) carrying the failure output; the gate step stays `running`
//! and runs again once its `needs` succeed again. At most `on_fail.max`
//! (default 2) send-backs, then the step journals `escalated` (announced by
//! `attention`), fails, and the run ends for a human.
//!
//! A run that ends `done (unverified)` gets a suggested `test` gate when its
//! repo has one ([`suggest`]).

pub(crate) mod content;
#[cfg(test)]
pub(crate) mod tests;
mod visual;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sun_core::host_protocol::{ExecRequest, Worktree};
use sun_core::model::Step;
use sun_core::{
    HostId, HostLink, ModCall, ModLink, Result, RunId, StepId, StepKey, StepStatus, TaskId,
};
use sun_store::{EventLog, EventQuery, RunRepo, Tx};
use sun_workflow::{CODE_GATES, GateSpec, OnFail, StepKind, Workflow};

use crate::data_touched;
use crate::executor::{append, status_payload};
use crate::multi_repo::{self, Order, RepoPr};

/// Prefix of a `merge_order` outcome that is not decided yet.
const WAITING: &str = "\u{0}waiting:";

/// Event kind: one gate's result, on the gate step.
const GATE_RESULT: &str = "gate_result";
/// Event kind: work sent back, on the target step; carries the feedback for
/// its next attempt.
const SENT_BACK: &str = "sent_back";
/// Event kind: a verification step used up its send-backs and fails for a
/// person; payload `{send_backs, reason}`, on that step.
pub(crate) const ESCALATED: &str = "escalated";

/// Seconds a gate command or mod call may take; override with `timeout_s`.
const DEFAULT_TIMEOUT_S: u64 = 600;
/// Bytes of each failing gate's output sent back to the agent.
const FEEDBACK_TAIL: usize = 8 * 1024;

/// One gate's outcome. Only the engine builds one
/// (`non_exhaustive`); nothing converts into it.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[non_exhaustive]
pub struct GateResult {
    /// Gate name as written in the workflow.
    pub name: String,
    /// Passed.
    pub pass: bool,
    /// What the gate printed or found (redacted).
    pub output: String,
    /// The file a content gate read, as `(name, text)`: saved as a task
    /// artifact version.
    #[serde(skip)]
    pub file: Option<(String, String)>,
    /// What a `data_touched` gate detected (journalled as its own event).
    #[serde(skip)]
    pub touched: Vec<crate::data_touched::Touch>,
    /// Not decided yet (`merge_order` before every PR merged): the step
    /// keeps running and checks again on the next merge.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub waiting: bool,
}

/// How one gate checks (**Strategy**).
#[derive(Debug, Clone)]
enum Gate {
    /// A shell command in the task worktree; passes on exit 0.
    Exec {
        command: String,
        timeout_s: u64,
        scope: Scope,
    },
    /// A multi-repo task's PRs merge in order (`multi_repo`).
    MergeOrder { mod_name: String, timeout_s: u64 },
    /// A mod method that answers `{pass, output}`.
    Mod {
        mod_name: String,
        method: String,
        params: Option<Value>,
        timeout_s: u64,
    },
    /// A check over a worktree file.
    Content(content::Check),
    /// Data-touched detection over the task diff (always passes).
    DataTouched(data_touched::Params),
    /// Out-of-scope screens unchanged between the preview sides.
    Visual(visual::Params),
}

/// Where an exec gate runs in a multi-repo worktree.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Scope {
    /// In every repo; passes when all pass.
    Each,
    /// In every repo; passes when any passes (`diff_not_empty`).
    Any,
    /// Once, in the session dir (`integration`).
    Once,
}

/// `test` / `lint` / `diagnostics_clean` params.
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct CommandParams {
    command: Option<String>,
    timeout_s: Option<u64>,
}

/// Has the task changed anything? Uncommitted changes, or commits that no
/// `origin/*` branch has (the host cuts task branches from `origin/<base>`)
/// but a task's own `origin/sun/*` (a mid-run move pushed it).
const DIFF_NOT_EMPTY: &str = r#"git rev-parse --git-dir >/dev/null || exit 1
changes=$(git status --porcelain)
ahead=$(git rev-list --count HEAD --not --exclude='origin/sun/*' --remotes=origin 2>/dev/null || echo 0)
if [ -n "$changes" ] || [ "$ahead" -gt 0 ]; then
  echo "$ahead commit(s) ahead of origin"; printf '%s\n' "$changes"
else
  echo 'no changes: the working tree is clean and no commit is ahead of origin'; exit 1
fi"#;

/// Per ecosystem, in detection order: the file that marks it and its
/// `test`, `lint` and `diagnostics_clean` commands.
// ponytail: three ecosystems; set `command:` for anything else.
const ECOSYSTEMS: [(&str, [&str; 3]); 3] = [
    (
        "Cargo.toml",
        [
            "cargo test",
            "cargo clippy --all-targets -- -D warnings",
            "cargo check --all-targets",
        ],
    ),
    (
        "package.json",
        ["npm test", "npm run lint", "npx --no-install tsc --noEmit"],
    ),
    (
        "go.mod",
        ["go test ./...", "go vet ./...", "go build ./..."],
    ),
];

/// Default command of a code gate, picked from the worktree's files.
fn default_command(name: &str) -> String {
    let which = match name {
        "test" => 0,
        "lint" => 1,
        _ => 2,
    };
    let mut script = String::new();
    for (i, (file, commands)) in ECOSYSTEMS.iter().enumerate() {
        let keyword = if i == 0 { "if" } else { "elif" };
        script.push_str(&format!(
            "{keyword} [ -f {file} ]; then {}; ",
            commands[which]
        ));
    }
    format!(
        "{script}else echo 'no {name} command found: set `{name}: {{command: ...}}` on the \
         gate'; exit 1; fi"
    )
}

/// Event kind: the `test` gate Sun suggests for a run that ended `done
/// (unverified)`; payload `{gate: "test", command}`.
pub(crate) const GATE_SUGGESTED: &str = "gate_suggested";

/// The `test` command the repo in `cwd` suggests: the first ecosystem
/// found, and for npm only a real `scripts.test` (not `npm init`'s stub).
async fn suggest_test(
    hosts: &dyn HostLink,
    host: HostId,
    cwd: &std::path::Path,
) -> Option<&'static str> {
    let files = crate::executor::repo_source::cat(hosts, host, cwd, "ls -A".into()).await?;
    let (file, commands) = ECOSYSTEMS
        .iter()
        .find(|(file, _)| files.lines().any(|line| line == *file))?;
    if *file == "package.json" {
        let text =
            crate::executor::repo_source::cat(hosts, host, cwd, "cat package.json".into()).await?;
        let package: Value = serde_json::from_str(&text).ok()?;
        let script = package["scripts"]["test"].as_str()?;
        if script.contains("no test specified") {
            return None;
        }
    }
    Some(commands[0])
}

/// Journals a [`GATE_SUGGESTED`] on `run_id` when its repo on `host`
/// suggests a test command. Call it before the task's worktrees go.
pub(crate) async fn suggest(
    hosts: &dyn HostLink,
    store: &sun_store::Store,
    host: HostId,
    run_id: RunId,
    worktree: &Worktree,
) -> Result<()> {
    let Some(cwd) = crate::executor::repo_source::repo_dir(hosts, host, worktree).await else {
        return Ok(());
    };
    let Some(command) = suggest_test(hosts, host, &cwd).await else {
        return Ok(());
    };
    let mut tx = store.begin().await?;
    let payload = json!({ "gate": "test", "command": command });
    tx.append_event(sun_store::NewEvent::new(
        run_id,
        None,
        GATE_SUGGESTED,
        payload,
    ))
    .await?;
    Ok(tx.commit().await?)
}

/// Whether `spec` names a gate Sun can run as configured.
pub(crate) fn resolves(spec: &GateSpec) -> bool {
    resolve(spec).is_ok()
}

fn resolve(spec: &GateSpec) -> std::result::Result<Gate, String> {
    let params = spec.params.as_ref();
    let name = spec.name.as_str();
    let timeout_s = params
        .and_then(|p| p.get("timeout_s"))
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_TIMEOUT_S);
    let mod_gate = |mod_name: &str, method: &str| Gate::Mod {
        mod_name: mod_name.to_owned(),
        method: method.to_owned(),
        params: params.cloned(),
        timeout_s,
    };
    Ok(match name {
        "diff_not_empty" | "test" | "lint" | "diagnostics_clean" | "integration" => {
            let parsed: CommandParams = match params {
                Some(p) => serde_json::from_value(p.clone())
                    .map_err(|e| format!("`{name}` params: {e}"))?,
                None => CommandParams::default(),
            };
            let command = match parsed.command {
                Some(c) if c.trim().is_empty() => return Err(format!("`{name}`: empty command")),
                Some(c) => c,
                None if name == "diff_not_empty" => DIFF_NOT_EMPTY.to_owned(),
                None if name == "integration" => {
                    return Err("`integration` needs `{command: ...}`".into());
                }
                None => default_command(name),
            };
            let scope = match name {
                "diff_not_empty" => Scope::Any,
                "integration" => Scope::Once,
                _ => Scope::Each,
            };
            Gate::Exec {
                command,
                timeout_s: parsed.timeout_s.unwrap_or(timeout_s),
                scope,
            }
        }
        "merge_order" => {
            let via = params.and_then(|p| p.get("mod")).and_then(Value::as_str);
            Gate::MergeOrder {
                mod_name: via.unwrap_or("github").to_owned(),
                timeout_s: timeout_s.min(60),
            }
        }
        "data_touched" => Gate::DataTouched(data_touched::Params::parse(params)?),
        "visual_diff" => Gate::Visual(visual::Params::parse(params)?),
        "pr_exists" | "ci_green" => {
            let via = params.and_then(|p| p.get("mod")).and_then(Value::as_str);
            mod_gate(via.unwrap_or("github"), name)
        }
        _ => match name.split_once('.') {
            Some((mod_name, method)) => mod_gate(mod_name, method),
            None => Gate::Content(content::Check::parse(name, params)?),
        },
    })
}

/// A gate step's checks, ready to run off the engine task (no store access).
#[derive(Debug, Clone)]
pub(crate) struct Job {
    host: HostId,
    task_id: TaskId,
    /// The task worktree the agent sessions used (`executor::worktree_of`).
    worktree: Worktree,
    run_id: RunId,
    gates: Vec<(String, Gate)>,
    /// The task's repos and PRs, for `merge_order` (`Self::with_prs`).
    prs: Vec<RepoPr>,
    /// The knowledge docs the run's packs gave, for `cites_sources`
    /// (`Self::with_sources`).
    sources: Vec<content::Source>,
}

impl Job {
    /// Resolves `specs`. `Err` is a configuration error: the step fails
    /// without a send-back (the agent can't fix the workflow).
    pub(crate) fn new(
        host: HostId,
        worktree: Worktree,
        run_id: RunId,
        specs: &[GateSpec],
    ) -> std::result::Result<Self, String> {
        let task_id = worktree.task_id();
        let gates = specs
            .iter()
            .map(|spec| Ok((spec.name.clone(), resolve(spec)?)))
            .collect::<std::result::Result<_, String>>()?;
        Ok(Self {
            host,
            task_id,
            worktree,
            run_id,
            gates,
            prs: Vec::new(),
            sources: Vec::new(),
        })
    }

    /// Whether a gate is `cites_sources` ([`Self::with_sources`]).
    pub(crate) fn needs_sources(&self) -> bool {
        self.gates
            .iter()
            .any(|(_, g)| matches!(g, Gate::Content(content::Check::CitesSources { .. })))
    }

    /// The knowledge docs given to the run ([`content::run_sources`]).
    pub(crate) fn with_sources(mut self, sources: Vec<content::Source>) -> Self {
        self.sources = sources;
        self
    }

    /// The git host mod of a `merge_order` gate, which needs the task's PRs
    /// ([`Self::with_prs`]) and sets their statuses.
    pub(crate) fn merge_order_mod(&self) -> Option<&str> {
        self.gates.iter().find_map(|(_, g)| match g {
            Gate::MergeOrder { mod_name, .. } => Some(mod_name.as_str()),
            _ => None,
        })
    }

    /// The task's repos and PRs as journalled so far (`multi_repo::task_repos`).
    pub(crate) fn with_prs(mut self, prs: Vec<RepoPr>) -> Self {
        self.prs = prs;
        self
    }

    /// What the task changed (uncommitted and unpushed, as `--stat`), for the
    /// decision engine's scope question; empty when it can't be read.
    pub(crate) async fn changes(&self, hosts: &dyn HostLink) -> String {
        const STAT: &str = "git status --short 2>/dev/null; git diff --stat HEAD 2>/dev/null; \
                            git log --stat --oneline --not --remotes 2>/dev/null | head -200";
        let Ok(dir) = self.worktree(hosts, &mut None).await else {
            return String::new();
        };
        let mut out = String::new();
        for (_, dir) in self.repo_dirs(&dir) {
            out.push_str(&self.exec(hosts, dir, STAT, 60).await.unwrap_or_default());
        }
        out.chars().take(8000).collect()
    }

    /// Runs the gates in order and stops at the first failure (later gates
    /// are usually slower, and the agent fixes one thing at a time).
    pub(crate) async fn run(self, hosts: &dyn HostLink, mods: &dyn ModLink) -> Vec<GateResult> {
        let mut cwd = None;
        let mut results = Vec::new();
        for (name, gate) in &self.gates {
            let mut file = None;
            let mut touched = Vec::new();
            let mut waiting = false;
            let outcome = self
                .check(gate, hosts, mods, &mut cwd, &mut file, &mut touched)
                .await
                .map_err(|e| {
                    waiting = e.starts_with(WAITING);
                    e.strip_prefix(WAITING).map(str::to_owned).unwrap_or(e)
                });
            let pass = outcome.is_ok();
            let output = outcome.unwrap_or_else(|e| e);
            results.push(GateResult {
                name: name.clone(),
                pass,
                output: sun_core::redact::redact(&output, &[]),
                file,
                touched,
                waiting,
            });
            if !pass {
                break;
            }
        }
        results
    }

    async fn check(
        &self,
        gate: &Gate,
        hosts: &dyn HostLink,
        mods: &dyn ModLink,
        cwd: &mut Option<PathBuf>,
        file: &mut Option<(String, String)>,
        touched: &mut Vec<data_touched::Touch>,
    ) -> std::result::Result<String, String> {
        match gate {
            Gate::Visual(params) => params.check(hosts, self.host, &self.worktree).await,
            Gate::DataTouched(params) => {
                let dir = self.worktree(hosts, cwd).await?;
                let timeout_s = params.timeout_s.unwrap_or(60);
                let mut listing = String::new();
                for (_, dir) in self.repo_dirs(&dir) {
                    let part = self
                        .exec(hosts, dir, data_touched::script(), timeout_s)
                        .await?;
                    listing.push_str(&part);
                }
                *touched = data_touched::classify(&listing, params);
                Ok(data_touched::summary(touched))
            }
            Gate::Exec {
                command,
                timeout_s,
                scope,
            } => {
                let dir = self.worktree(hosts, cwd).await?;
                let dirs = match scope {
                    Scope::Once => vec![(String::new(), dir)],
                    Scope::Each | Scope::Any => self.repo_dirs(&dir),
                };
                if let [(_, dir)] = dirs.as_slice() {
                    return self.exec(hosts, dir.clone(), command, *timeout_s).await;
                }
                let (mut output, mut passed, mut failed) = (String::new(), 0, 0);
                for (name, dir) in dirs {
                    let outcome = self.exec(hosts, dir, command, *timeout_s).await;
                    let (mark, text) = match &outcome {
                        Ok(text) => ("passed", text),
                        Err(text) => ("failed", text),
                    };
                    (passed, failed) = match outcome.is_ok() {
                        true => (passed + 1, failed),
                        false => (passed, failed + 1),
                    };
                    output.push_str(&format!("## {name}/ ({mark})\n{}\n", text.trim_end()));
                }
                let pass = match scope {
                    Scope::Any => passed > 0,
                    _ => failed == 0,
                };
                if pass { Ok(output) } else { Err(output) }
            }
            Gate::MergeOrder {
                mod_name,
                timeout_s,
            } => self.merge_order(mods, mod_name, *timeout_s).await,
            Gate::Mod {
                mod_name,
                method,
                params,
                timeout_s,
            } => {
                let call = ModCall {
                    mod_name: mod_name.clone(),
                    method: method.clone(),
                    params: json!({
                        "task_id": self.task_id,
                        "run_id": self.run_id,
                        "branch": self.worktree.branch(),
                        "params": params,
                    }),
                    idem_key: None,
                };
                let target = format!("`{mod_name}.{method}`");
                let reply = tokio::time::timeout(Duration::from_secs(*timeout_s), mods.call(call))
                    .await
                    .map_err(|_| format!("{target} gave no answer in {timeout_s}s"))?
                    .map_err(|e| format!("{target}: {e}"))?;
                #[derive(Deserialize)]
                struct Reply {
                    pass: bool,
                    #[serde(default)]
                    output: String,
                }
                let reply: Reply = serde_json::from_value(reply)
                    .map_err(|e| format!("{target} must answer {{pass, output}}: {e}"))?;
                if reply.pass {
                    Ok(reply.output)
                } else {
                    Err(reply.output)
                }
            }
            Gate::Content(check) => {
                let dir = self.worktree(hosts, cwd).await?;
                // ponytail: exec keeps the last 64 KiB, so bigger files are cut;
                // add a host file-read frame when outputs outgrow that.
                let text = self.exec(hosts, dir.clone(), &check.command(), 30).await?;
                // An image's bytes become an artifact through the task-folder
                // snapshot (`snapshot`), not through this text.
                if !matches!(check, content::Check::ImageSize { .. }) {
                    *file = Some((check.file().to_owned(), text.clone()));
                }
                match check {
                    content::Check::LinksResolve { .. } => self.links(hosts, dir, &text).await,
                    content::Check::CitesSources { .. } => content::cites(&text, &self.sources),
                    _ => check.run(&text),
                }
            }
        }
    }

    /// `merge_order`: `Err` starting with [`WAITING`] while PRs are left.
    async fn merge_order(
        &self,
        mods: &dyn ModLink,
        mod_name: &str,
        timeout_s: u64,
    ) -> std::result::Result<String, String> {
        let prs: Vec<&RepoPr> = self.prs.iter().filter(|r| r.pr.is_some()).collect();
        if prs.is_empty() {
            return Err(
                "no PR is open yet: run `merge_order` in a gate step after `open_pr`".into(),
            );
        }
        let name = multi_repo::pr_name;
        // A PR closed without merging never merges: fail, don't wait.
        if let Some(closed) = prs.iter().find(|r| r.status == "closed") {
            let number = closed
                .pr
                .as_ref()
                .map_or(Value::Null, |p| p["number"].clone());
            return Err(format!(
                "PR #{number} in {} was closed without merging. Reopen and merge it, then retry.",
                closed.slug
            ));
        }
        match multi_repo::merge_order(&prs) {
            Order::Done => {
                let list: Vec<String> = prs.iter().map(|r| name(r)).collect();
                Ok(format!("merged in order: {}", list.join(", ")))
            }
            Order::Broken(why) => Err(why),
            Order::Waiting { next } => {
                let call = ModCall {
                    mod_name: mod_name.to_owned(),
                    method: "ci_status".into(),
                    params: json!({ "repo": prs[next].slug, "ref": self.worktree.branch() }),
                    idem_key: None,
                };
                let ci = match tokio::time::timeout(Duration::from_secs(timeout_s), mods.call(call))
                    .await
                {
                    Ok(Ok(reply)) => reply["state"].as_str().unwrap_or("unknown").to_owned(),
                    _ => "unknown".into(),
                };
                let mut text = format!("{WAITING}merge next: {} (CI: {ci})", name(prs[next]));
                let after: Vec<String> = prs[next + 1..].iter().map(|r| name(r)).collect();
                if !after.is_empty() {
                    text.push_str(&format!("; then: {}", after.join(", ")));
                }
                Err(text)
            }
        }
    }

    /// `(folder, dir)` of every repo worktree in `dir` (a multi-repo
    /// session dir), or just `dir`.
    fn repo_dirs(&self, dir: &std::path::Path) -> Vec<(String, PathBuf)> {
        match &self.worktree {
            Worktree::Repos { repos, .. } => repos
                .iter()
                .map(|r| (r.name.clone(), dir.join(&r.name)))
                .collect(),
            _ => vec![(String::new(), dir.to_path_buf())],
        }
    }

    /// The task's working directory on the host, prepared once per job
    /// (idempotent: the agent's session already made it).
    async fn worktree(
        &self,
        hosts: &dyn HostLink,
        cwd: &mut Option<PathBuf>,
    ) -> std::result::Result<PathBuf, String> {
        if let Some(dir) = cwd {
            return Ok(dir.clone());
        }
        let dir = hosts
            .prepare_worktree(self.host, self.worktree.clone())
            .await
            .map_err(|e| format!("worktree: {e}"))?;
        *cwd = Some(dir.clone());
        Ok(dir)
    }

    async fn exec(
        &self,
        hosts: &dyn HostLink,
        cwd: PathBuf,
        command: &str,
        timeout_s: u64,
    ) -> std::result::Result<String, String> {
        let request = ExecRequest {
            cwd,
            command: command.to_owned(),
            env: Default::default(),
            timeout_s,
        };
        let out = hosts
            .exec(self.host, request)
            .await
            .map_err(|e| format!("host: {e}"))?;
        match out.code {
            Some(0) => Ok(out.output),
            _ if out.timed_out => Err(format!("{}\ntimed out after {timeout_s}s", out.output)),
            code => Err(format!("{}\nexit code {code:?}", out.output)),
        }
    }

    /// Fetches every link from the host; 2xx/3xx (after redirects) pass.
    async fn links(
        &self,
        hosts: &dyn HostLink,
        cwd: PathBuf,
        text: &str,
    ) -> std::result::Result<String, String> {
        let urls = content::links(text);
        if urls.is_empty() {
            return Ok("no links".into());
        }
        let list: Vec<String> = urls.iter().map(|u| quote(u)).collect();
        let script = format!(
            "for u in {}; do printf '%s %s\\n' \"$(curl -sS -o /dev/null -L --max-time 10 \
             -w '%{{http_code}}' \"$u\" 2>/dev/null)\" \"$u\"; done",
            list.join(" ")
        );
        let output = self.exec(hosts, cwd, &script, DEFAULT_TIMEOUT_S).await?;
        let broken: Vec<&str> = output
            .lines()
            .filter(|line| {
                let code = line.split_once(' ').map(|(code, _)| code.parse::<u16>());
                !matches!(code, Some(Ok(200..=399)))
            })
            .collect();
        if broken.is_empty() {
            Ok(format!("{} links resolve", urls.len()))
        } else {
            Err(format!("broken links (status url):\n{}", broken.join("\n")))
        }
    }
}

/// MIME type of a content gate's file, from its extension.
pub(crate) fn mime_of(name: &str) -> &'static str {
    match name
        .rsplit_once('.')
        .map(|(_, ext)| ext.to_ascii_lowercase())
    {
        Some(ext) if ext == "md" || ext == "markdown" => "text/markdown",
        Some(ext) if ext == "csv" => "text/csv",
        Some(ext) if ext == "html" || ext == "htm" => "text/html",
        Some(ext) if ext == "json" => "application/json",
        Some(ext) if ext == "png" => "image/png",
        Some(ext) if ext == "jpg" || ext == "jpeg" => "image/jpeg",
        Some(ext) if ext == "gif" => "image/gif",
        Some(ext) if ext == "webp" => "image/webp",
        Some(ext) if ext == "svg" => "image/svg+xml",
        Some(ext) if ext == "pdf" => "application/pdf",
        _ => "text/plain",
    }
}

/// `text` as one single-quoted shell word.
pub(crate) fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// What happens to a gate step after its gates ran.
#[derive(Debug, PartialEq)]
pub(crate) enum Verdict {
    /// Every gate passed: `verifying → succeeded`.
    Pass,
    /// Work was sent back: `verifying → running`, with this reason.
    SendBack(String),
    /// `verifying → failed`, with this reason.
    Fail(String),
}

/// Journals `results` on `gate_step` and decides its verdict. On a send-back
/// it also queues the new attempts (in the caller's transaction).
pub(crate) async fn judge(
    tx: &mut Tx,
    workflow: &Workflow,
    latest: &HashMap<StepKey, Step>,
    gate_step: &Step,
    results: &[GateResult],
) -> Result<Verdict> {
    for result in results {
        append(
            tx,
            gate_step,
            GATE_RESULT,
            json!({ "step_id": gate_step.id, "attempt": gate_step.attempt, "result": result }),
        )
        .await?;
    }
    data_touched::journal(tx, gate_step, results).await?;
    let failed: Vec<&GateResult> = results.iter().filter(|r| !r.pass).collect();
    if failed.is_empty() {
        return Ok(Verdict::Pass);
    }
    let names: Vec<&str> = failed.iter().map(|r| r.name.as_str()).collect();
    let names = names.join(", ");
    let on_fail = workflow
        .steps
        .iter()
        .find(|d| d.id == gate_step.step_key)
        .and_then(|d| match &d.kind {
            StepKind::Gate { on_fail, .. } => on_fail.as_ref(),
            _ => None,
        });
    let failure = format!("gate failed: {names}");
    let Some(on_fail) = on_fail else {
        return Ok(Verdict::Fail(failure));
    };
    send_back(
        tx,
        workflow,
        latest,
        gate_step,
        on_fail,
        &failure,
        |n, max| feedback(&gate_step.step_key, n, max, &failed),
    )
    .await
}

/// Sends the work of a failed verification step (`from`: a gate or a device
/// verify) back to `on_fail.send_back_to` with `feedback(n, max)` for the
/// agent, or fails it with `failure` once `max` send-backs were used. Queues
/// the new attempts in the caller's transaction.
pub(crate) async fn send_back(
    tx: &mut Tx,
    workflow: &Workflow,
    latest: &HashMap<StepKey, Step>,
    from: &Step,
    on_fail: &OnFail,
    failure: &str,
    feedback: impl FnOnce(u32, u32) -> String,
) -> Result<Verdict> {
    let Some(target) = latest.get(&on_fail.send_back_to) else {
        return Ok(Verdict::Fail(format!(
            "{failure}; `{}` never ran, nothing to send back",
            on_fail.send_back_to
        )));
    };
    // ponytail: two gates sending back to one step share `max`. Count per
    // gate (`from` in `sent_back`) if that ever matters.
    let sent = send_backs(tx, target, from).await?;
    if sent >= on_fail.max {
        let why = format!("{failure}, after {sent} send-back(s)");
        // The agent had its tries: a person takes over (`attention`).
        append(
            tx,
            from,
            ESCALATED,
            json!({ "send_backs": sent, "reason": why }),
        )
        .await?;
        return Ok(Verdict::Fail(why));
    }
    let feedback = feedback(sent + 1, on_fail.max);
    for key in rerun(workflow, &on_fail.send_back_to, &from.step_key) {
        let Some(previous) = latest.get(&key) else {
            continue;
        };
        let step = Step {
            id: StepId::generate(),
            run_id: from.run_id,
            step_key: key,
            attempt: previous.attempt + 1,
            status: StepStatus::Queued,
            runtime_id: None,
            credential_id: None,
            session_id: None,
            started_at: None,
            ended_at: None,
        };
        tx.insert_step(&step).await?;
        append(tx, &step, "step_status", status_payload(&step, None, None)).await?;
        if step.step_key == on_fail.send_back_to {
            append(
                tx,
                &step,
                SENT_BACK,
                json!({ "attempt": step.attempt, "from": from.step_key, "feedback": feedback }),
            )
            .await?;
        }
    }
    Ok(Verdict::SendBack(format!(
        "sent back to `{}` ({}/{}): {failure}",
        on_fail.send_back_to,
        sent + 1,
        on_fail.max
    )))
}

/// Send-backs from `from` (the gate) to `target` since a person last
/// retried either: a manual retry gives the agent a fresh set; a `retry:`
/// policy retry neither resets nor uses one.
async fn send_backs(tx: &mut Tx, target: &Step, from: &Step) -> Result<u32> {
    use crate::executor::retry::{BY_POLICY, STEP_RETRY};
    let run = target.run_id;
    let retries = tx.search_events(run, Some(STEP_RETRY), None, 0, u32::MAX);
    let reset = retries
        .await?
        .iter()
        .filter(|e| e.payload["by"] != BY_POLICY)
        .filter(|e| {
            [&target.step_key, &from.step_key]
                .map(Some)
                .contains(&e.step_key.as_ref())
        })
        .map(|e| e.seq)
        .max()
        .unwrap_or(0);
    let sent = tx.search_events(run, Some(SENT_BACK), None, reset, u32::MAX);
    let sent = sent.await?;
    let to_target = sent
        .iter()
        .filter(|e| e.step_key.as_ref() == Some(&target.step_key));
    Ok(u32::try_from(to_target.count()).unwrap_or(u32::MAX))
}

/// The message the agent gets with its next attempt.
fn feedback(gate: &StepKey, n: u32, max: u32, failed: &[&GateResult]) -> String {
    let mut text = format!(
        "Verification step `{gate}` failed (send-back {n} of {max}). Fix the problems \
         below, then claim done again.\n"
    );
    for result in failed {
        text.push_str(&format!(
            "\n### Gate `{}` failed\n```\n{}\n```\n",
            result.name,
            tail(&result.output)
        ));
    }
    text
}

/// The last [`FEEDBACK_TAIL`] bytes of a check's output, for the agent.
pub(crate) fn tail(output: &str) -> &str {
    let mut start = output.len().saturating_sub(FEEDBACK_TAIL);
    while !output.is_char_boundary(start) {
        start += 1;
    }
    output[start..].trim_end()
}

/// `target` plus every step that needs it (transitively) and that `gate`
/// needs (transitively): what must run again before the gate re-checks.
fn rerun(workflow: &Workflow, target: &StepKey, gate: &StepKey) -> Vec<StepKey> {
    let upstream = |key: &StepKey| {
        let mut seen = HashSet::new();
        let mut todo = vec![key.clone()];
        while let Some(key) = todo.pop() {
            if let Some(def) = workflow.steps.iter().find(|d| d.id == key) {
                for need in &def.needs {
                    if seen.insert(need.clone()) {
                        todo.push(need.clone());
                    }
                }
            }
        }
        seen
    };
    let before_gate = upstream(gate);
    workflow
        .steps
        .iter()
        .map(|d| &d.id)
        .filter(|key| {
            *key == target || (before_gate.contains(*key) && upstream(key).contains(target))
        })
        .cloned()
        .collect()
}

/// `prompt` plus the send-back feedback journalled for this attempt, if any.
pub(crate) async fn with_feedback(tx: &mut Tx, step: &Step, prompt: &str) -> Result<String> {
    if step.attempt == 1 {
        return Ok(prompt.to_owned());
    }
    let mut found = None;
    let mut after = 0;
    loop {
        let page = tx.events_after(Some(step.run_id), after, 500).await?;
        let Some(last) = page.last() else { break };
        after = last.seq;
        for event in page.iter().filter(|e| {
            e.kind == SENT_BACK
                && e.step_key.as_ref() == Some(&step.step_key)
                && e.payload["attempt"] == step.attempt
        }) {
            found = event.payload["feedback"].as_str().map(str::to_owned);
        }
    }
    Ok(match found {
        Some(feedback) => format!("{prompt}\n\n{feedback}"),
        None => prompt.to_owned(),
    })
}

/// Why a succeeded run is only `done (unverified)`, if it is:
/// no gate and no approval at all, or code gates without `test` or `ci_green`.
pub(crate) fn unverified(workflow: &Workflow) -> Option<&'static str> {
    let mut names = Vec::new();
    let mut approval = false;
    for def in &workflow.steps {
        match &def.kind {
            StepKind::Gate { gates, .. } => names.extend(gates.iter().map(|g| g.name.as_str())),
            StepKind::Approval { .. } => approval = true,
            _ => {}
        }
    }
    let code = names.iter().any(|n| CODE_GATES.contains(n));
    if names.is_empty() && !approval {
        Some("done (unverified): no gate or approval configured")
    } else if code && !names.iter().any(|n| matches!(*n, "test" | "ci_green")) {
        Some("done (unverified): no test or ci_green gate")
    } else {
        None
    }
}
