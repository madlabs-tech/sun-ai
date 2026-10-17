//! OpenCode **Adapter** over ACP v1:
//! `opencode acp`, JSON-RPC 2.0 over stdio, one message per line.
//!
//! A small hand-written client on the shared JSONL driver, not the
//! `agent-client-protocol` crate: Sun sends three requests (`initialize`,
//! `session/new` or `session/load` to resume, `session/prompt`) and answers
//! permission requests; the
//! crate would bring its own process and connection runtime instead.
//! Runs on whatever login or provider config OpenCode has (BYO runtime),
//! or on a [`ByoKey`] endpoint rendered per session (BYO key).
//!
//! Permission modes: every session's config sets `edit`, `bash` and
//! `webfetch` to `ask` (OpenCode 1.18.34 allows all three by default), and
//! Sun answers each request by the mode ([`AcpStream::with_permissions`]).
//!
//! | Sun mode | Allowed request kinds | Effect |
//! |---|---|---|
//! | `plan` | read, search, think | never edits, runs or fetches, even with `allow_shell` |
//! | `default` | read, search, think (+ execute with `allow_shell`) | edits refused |
//! | `accept_edits` | + edit, delete, move | edits freely |
//! | `bypass_permissions` | every kind | everything |
//!
//! `auto` is not reported (the four [`PermissionMode::LEGACY`] modes are):
//! OpenCode has no reviewer to approve "safe" commands. A session can still
//! get it from a host's own `runtimes.permission_mode` default; it then
//! answers as `accept_edits`, never more.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use chrono::Utc;
use serde_json::{Value, json};
use sun_core::{
    BoxStream, Error, HostCtx, LimitKind, PermissionMode, Readiness, Result, Role, RuntimeAdapter,
    SessionHandle, SessionId, SessionSpec, Steer, StopMode, SunEvent, Transcript,
};
use tokio::process::Command;
use tokio::sync::watch;

use crate::byo_key::{self, ByoKey};
use crate::claude::todo;
use crate::process::{
    self, Normalizer, Probe, Process, Sessions, args_hash, native_id, readiness, relative, stream,
    truncate,
};
use crate::{envelope, limit};

/// Adapter id.
pub const ID: &str = "opencode";

/// The env var OpenCode merges over its own config.
const CONFIG_ENV: &str = "OPENCODE_CONFIG_CONTENT";

/// JSON-RPC ids of the handshake; the first prompt is 3, steers count on from 4.
const INITIALIZE: u64 = 1;
const SESSION_NEW: u64 = 2;
const FIRST_PROMPT: u64 = 3;

/// OpenCode runtime adapter.
pub struct OpenCode {
    /// The `opencode` binary (a bare name is looked up on `PATH`).
    pub binary: PathBuf,
    /// The `byo_key` source; `None` runs on OpenCode's own config (BYO runtime).
    pub byo_key: Option<ByoKey>,
    /// Which agent permission requests are allowed when the spec names no
    /// mode (`accept_edits` by default).
    pub permission_mode: PermissionMode,
    /// Also allow shell commands (`execute`), e.g. to run the gate commands.
    pub allow_shell: bool,
    /// Where a user's `opencode.md` envelope template overrides the built-in
    /// one (sun.yaml `runtimes.template_dir`).
    pub template_dir: Option<PathBuf>,
    sessions: Sessions<AcpSession>,
}

impl Default for OpenCode {
    fn default() -> Self {
        Self::new("opencode")
    }
}

impl OpenCode {
    /// An adapter that runs `binary`.
    pub fn new(binary: impl Into<PathBuf>) -> Self {
        Self {
            binary: binary.into(),
            byo_key: None,
            permission_mode: PermissionMode::AcceptEdits,
            allow_shell: false,
            template_dir: None,
            sessions: Sessions::default(),
        }
    }

    fn session(&self, id: &SessionId) -> Result<Arc<AcpSession>> {
        self.sessions.get(ID, id)
    }
}

struct AcpSession {
    process: Process,
    /// The ACP `sessionId`, once `session/new` answered.
    session_id: watch::Receiver<Option<String>>,
    next_id: AtomicU64,
}

#[async_trait]
impl RuntimeAdapter for OpenCode {
    fn id(&self) -> &str {
        ID
    }

    // OpenCode's free models need no login; provider keys come from its own
    // config or `SessionSpec::env`, so only the install and version are checked.
    async fn readiness(&self, _host: &HostCtx) -> Readiness {
        let mut ready = readiness(&Probe {
            name: "OpenCode",
            binary: &self.binary,
            cli: ID,
            install: "npm install -g opencode-ai",
            login_args: &[],
            logged_in: |_| true,
            login: "opencode auth login",
        })
        .await;
        if let Some(Err(e)) = self.byo_key.as_ref().map(ByoKey::validate) {
            ready.logged_in = false;
            ready.detail = Some(e.to_string());
        }
        ready
    }

    async fn start(&self, spec: SessionSpec) -> Result<SessionHandle> {
        let mode = PermissionMode::parse_opt(spec.permission_mode.as_deref())
            .map_err(|why| Error::InvalidInput(format!("session permission_mode: {why}")))?
            .unwrap_or(self.permission_mode);
        let prompt = envelope::first_prompt(ID, self.template_dir.as_deref(), &spec)?;
        let mut cmd = Command::new(process::program(&self.binary, &spec.env));
        cmd.arg("acp").current_dir(&spec.worktree).envs(&spec.env);
        // Merged over the user's own OpenCode config; an env var, so nothing
        // lands in the worktree. A stored `byo_key` runtime comes in the
        // spec (from the engine); the adapter's own field is the fallback.
        let mut config = match spec.byo_key.as_ref().or(self.byo_key.as_ref()) {
            Some(byo) => {
                byo.validate()?;
                if byo.key_secret.is_some() && !spec.env.contains_key(byo_key::KEY_ENV) {
                    return Err(Error::InvalidInput(format!(
                        "BYO key runtime needs the key's value in env `{}`",
                        byo_key::KEY_ENV
                    )));
                }
                byo_key::opencode_config(byo, spec.model.as_deref())
            }
            // A provider config in the session env (Sun credits) wins.
            None => match spec.env.get(CONFIG_ENV) {
                Some(content) => serde_json::from_str(content)
                    .map_err(|e| Error::InvalidInput(format!("{CONFIG_ENV} is not JSON: {e}")))?,
                None => spec
                    .model
                    .as_ref()
                    .map_or_else(|| json!({}), |model| json!({ "model": model })),
            },
        };
        let Some(fields) = config.as_object_mut() else {
            return Err(Error::InvalidInput(format!(
                "{CONFIG_ENV} is not a JSON object"
            )));
        };
        // OpenCode's own default allows every tool without asking, so the
        // requests Sun answers by mode would never come.
        let ask = json!({"edit": "ask", "bash": "ask", "webfetch": "ask"});
        fields.insert("permission".into(), ask);
        cmd.env(CONFIG_ENV, config.to_string());
        let (tx, mut rx) = watch::channel(None);
        let mcp_servers = match &spec.mcp_config {
            Some(path) => crate::mcp::read_mcp_config(path)?
                .iter()
                .map(crate::mcp::McpEntry::acp)
                .collect(),
            None => Vec::new(),
        };
        let mut normalizer = AcpStream::new(prompt, Some(spec.worktree), Some(tx))
            .with_mcp_servers(mcp_servers)
            .with_permissions(mode, self.allow_shell);
        normalizer.resume = spec.resume;
        let initialize = request(
            INITIALIZE,
            "initialize",
            json!({
                "protocolVersion": 1,
                "clientCapabilities": {
                    "fs": {"readTextFile": false, "writeTextFile": false},
                    "terminal": false,
                },
            }),
        );
        let secrets = spec.env.into_values().collect();
        let process = Process::spawn(ID, cmd, normalizer, Some(initialize), secrets).await?;
        let pid = process.pid;
        self.sessions.insert(
            spec.session_id,
            AcpSession {
                process,
                session_id: rx.clone(),
                next_id: AtomicU64::new(FIRST_PROMPT + 1),
            },
        );
        Ok(SessionHandle {
            session_id: spec.session_id,
            native_id: native_id(&mut rx).await,
            pid,
        })
    }

    /// Sends the steer as a new `session/prompt`.
    // ponytail: mid-turn it is a second concurrent prompt, which OpenCode
    // queues or rejects (unverified); `session/cancel` first if it rejects.
    async fn send(&self, session: &SessionHandle, msg: Steer) -> Result<()> {
        let session = self.session(&session.session_id)?;
        let Some(session_id) = session.session_id.borrow().clone() else {
            return Err(Error::Unavailable(format!(
                "{ID} session is still starting"
            )));
        };
        let id = session.next_id.fetch_add(1, Ordering::SeqCst);
        session
            .process
            .send_line(&prompt(id, &session_id, &msg.text), ID)
            .await
    }

    /// OpenCode ignores stdin EOF, so a graceful stop sends `session/cancel`;
    /// the process is killed once the cancelled turn ends (or after
    /// `process::GRACE`).
    async fn stop(&self, session: &SessionHandle, how: StopMode) -> Result<()> {
        let session = self.session(&session.session_id)?;
        let session_id = session.session_id.borrow().clone();
        if let (StopMode::Graceful, Some(session_id)) = (how, session_id) {
            let cancel = json!({"jsonrpc": "2.0", "method": "session/cancel", "params": {"sessionId": session_id}});
            // Fails only when stdin is closed: the session is ending anyway.
            let _ = session.process.notify(&cancel.to_string(), ID).await;
        }
        session.process.stop(how);
        Ok(())
    }

    fn events(&self, session: &SessionHandle) -> BoxStream<'static, SunEvent> {
        match self.session(&session.session_id) {
            Ok(session) => session.process.events(),
            Err(_) => stream(None),
        }
    }

    // ponytail: no native bytes; resume on the same host uses ACP
    // `session/load <native_id>`.
    async fn export(&self, session: &SessionHandle) -> Result<Transcript> {
        Ok(Transcript {
            events: self.session(&session.session_id)?.process.log(),
            native: None,
        })
    }

    /// OpenCode models are `provider/model` from any provider; the window is
    /// reported at run time instead (`usage_update.size`).
    fn context_window(&self, _model: &str) -> Option<u32> {
        None
    }
}

/// One JSON-RPC request line.
fn request(id: u64, method: &str, params: Value) -> String {
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string()
}

fn prompt(id: u64, session_id: &str, text: &str) -> String {
    request(
        id,
        "session/prompt",
        json!({"sessionId": session_id, "prompt": [{"type": "text", "text": text}]}),
    )
}

/// A tool call, assembled from `tool_call` and its `tool_call_update`s.
#[derive(Debug, Default)]
struct Call {
    name: String,
    kind: String,
    paths: Vec<String>,
    emitted: bool,
}

/// Normalizes OpenCode's ACP messages to [`SunEvent`]s and drives the
/// handshake: `initialize` → `session/new` → `session/prompt`.
#[derive(Debug, Default)]
pub struct AcpStream {
    envelope: String,
    cwd: Option<PathBuf>,
    session_id: Option<watch::Sender<Option<String>>>,
    replies: Vec<String>,
    calls: HashMap<String, Call>,
    /// The agent message being streamed: its id and text so far.
    message: (Option<String>, String),
    /// The last agent message, the summary of `DoneClaimed`.
    last_text: String,
    /// Session cost so far; `usage_update` reports it cumulatively.
    cost: f64,
    /// The ACP session to `session/load` instead of `session/new`; cleared
    /// when the load answers. The history the agent replays meanwhile is
    /// not new work, so it is not surfaced.
    resume: Option<String>,
    /// ACP `McpServer`s for `session/new` (and `session/load`).
    mcp_servers: Vec<Value>,
    /// Tool kinds whose permission requests are allowed (ACP `ToolKind`).
    allowed_kinds: Vec<&'static str>,
}

impl AcpStream {
    /// A normalizer that prompts with `envelope` in `cwd` and reports the
    /// ACP session id to `session_id`.
    pub fn new(
        envelope: String,
        cwd: Option<PathBuf>,
        session_id: Option<watch::Sender<Option<String>>>,
    ) -> Self {
        Self {
            envelope,
            cwd,
            session_id,
            ..Self::default()
        }
    }

    /// Passes these MCP servers to `session/new`.
    pub fn with_mcp_servers(mut self, servers: Vec<Value>) -> Self {
        self.mcp_servers = servers;
        self
    }

    /// Answers permission requests by Sun's `mode` names (the module doc
    /// has the table): reads always, edits on `accept_edits`/`auto`,
    /// everything on `bypass_permissions`, shell commands with `allow_shell`
    /// except in `plan`.
    pub fn with_permissions(mut self, mode: PermissionMode, allow_shell: bool) -> Self {
        let mut kinds = vec!["read", "search", "think"];
        match mode {
            PermissionMode::BypassPermissions => {
                kinds.extend(["edit", "delete", "move", "execute", "fetch", "other"]);
            }
            // Not reported (module doc): only a host default reaches `auto`.
            PermissionMode::AcceptEdits | PermissionMode::Auto => {
                kinds.extend(["edit", "delete", "move"]);
            }
            PermissionMode::Default | PermissionMode::Plan => {}
        }
        if allow_shell && mode != PermissionMode::Plan && !kinds.contains(&"execute") {
            kinds.push("execute");
        }
        self.allowed_kinds = kinds;
        self
    }

    /// Emits the streamed agent message, if any.
    fn flush(&mut self, out: &mut Vec<SunEvent>) {
        let text = std::mem::take(&mut self.message.1);
        if !text.is_empty() {
            self.last_text.clone_from(&text);
            out.push(SunEvent::Text {
                role: Role::Assistant,
                text,
            });
        }
    }

    fn update(&mut self, update: &Value, out: &mut Vec<SunEvent>) {
        let kind = update["sessionUpdate"].as_str().unwrap_or_default();
        if kind == "agent_message_chunk" {
            let id = update["messageId"].as_str().map(str::to_owned);
            if id != self.message.0 {
                self.flush(out);
                self.message.0 = id;
            }
            if let Some(text) = update["content"]["text"].as_str() {
                self.message.1.push_str(text);
            }
            return;
        }
        match kind {
            "tool_call" | "tool_call_update" => {
                self.flush(out);
                self.tool(update, out);
            }
            "usage_update" => {
                self.flush(out);
                let cost = &update["cost"];
                let cost_usd = cost["amount"]
                    .as_f64()
                    .filter(|_| cost["currency"] == "USD")
                    .map(|total| total - std::mem::replace(&mut self.cost, total));
                out.push(SunEvent::Usage {
                    input: 0,
                    output: 0,
                    cost_usd,
                    context_used: update["used"].as_u64().and_then(|n| u32::try_from(n).ok()),
                });
            }
            "plan" => {
                self.flush(out);
                let entries = update["entries"].as_array().map_or(&[][..], Vec::as_slice);
                out.push(SunEvent::Plan {
                    todos: entries.iter().map(todo).collect(),
                });
            }
            _ => {} // thoughts, commands, modes: not surfaced
        }
    }

    fn tool(&mut self, update: &Value, out: &mut Vec<SunEvent>) {
        let id = update["toolCallId"].as_str().unwrap_or_default().to_owned();
        let call = self.calls.entry(id).or_default();
        if call.name.is_empty()
            && let Some(title) = update["title"].as_str()
        {
            title.clone_into(&mut call.name);
        }
        if let Some(kind) = update["kind"].as_str() {
            kind.clone_into(&mut call.kind);
        }
        let paths: Vec<String> = update["locations"]
            .as_array()
            .map_or(&[][..], Vec::as_slice)
            .iter()
            .filter_map(|l| l["path"].as_str())
            .map(|p| relative(self.cwd.as_deref(), p))
            .collect();
        if !paths.is_empty() {
            call.paths = paths;
        }
        let input = &update["rawInput"];
        let status = update["status"].as_str().unwrap_or_default();
        let finished = matches!(status, "completed" | "failed");
        // `tool_call` arrives with empty input; wait for the update that has it.
        if !call.emitted && (input.as_object().is_some_and(|o| !o.is_empty()) || finished) {
            call.emitted = true;
            let arg = call
                .paths
                .first()
                .cloned()
                .unwrap_or_else(|| input.to_string());
            out.push(SunEvent::ToolCall {
                name: call.name.clone(),
                args_hash: args_hash(input),
                summary: truncate(&format!("{} {arg}", call.name), 120),
            });
        }
        if !finished {
            return;
        }
        let ok = status == "completed";
        let content: Vec<&str> = update["content"]
            .as_array()
            .map_or(&[][..], Vec::as_slice)
            .iter()
            .filter_map(|c| c["content"]["text"].as_str())
            .collect();
        let text = if content.is_empty() {
            let raw = &update["rawOutput"];
            raw["error"]
                .as_str()
                .or(raw["output"].as_str())
                .unwrap_or_default()
                .to_owned()
        } else {
            content.join("\n")
        };
        out.push(SunEvent::ToolResult {
            ok,
            summary: truncate(&text, 200),
        });
        if ok && call.kind == "edit" {
            for path in &call.paths {
                out.push(SunEvent::FileChanged { path: path.clone() });
            }
        }
    }

    /// The answer to a prompt ends the turn.
    fn prompt_done(&mut self, line: &Value, out: &mut Vec<SunEvent>) {
        self.flush(out);
        let result = &line["result"];
        if !result.is_object() {
            out.push(prompt_error(&line["error"]));
            return;
        }
        let usage = &result["usage"];
        if usage.is_object() {
            let n = |key: &str| usage[key].as_u64().unwrap_or(0);
            out.push(SunEvent::Usage {
                input: n("inputTokens") + n("cachedReadTokens") + n("cachedWriteTokens"),
                output: n("outputTokens") + n("thoughtTokens"),
                cost_usd: None,
                context_used: None,
            });
        }
        match result["stopReason"].as_str() {
            Some("end_turn") => out.push(SunEvent::DoneClaimed {
                summary: std::mem::take(&mut self.last_text),
            }),
            Some("cancelled") => {} // Sun asked it to stop
            reason => out.push(SunEvent::Error {
                message: format!("{ID} stopped: {}", reason.unwrap_or("no stop reason")),
            }),
        }
    }

    /// Answers a request from the agent. A permission request is allowed
    /// once when its tool kind is in the policy (`with_permissions`), else
    /// rejected; the client offers no fs or terminal methods.
    fn answer(&mut self, method: &str, line: &Value) {
        let id = &line["id"];
        let reply = if method == "session/request_permission" {
            let kind = line["params"]["toolCall"]["kind"].as_str().unwrap_or("");
            let pick = match self.allowed_kinds.contains(&kind) {
                true => "allow_once",
                false => "reject_once",
            };
            let option = line["params"]["options"]
                .as_array()
                .and_then(|options| options.iter().find(|o| o["kind"] == pick))
                .map(|o| o["optionId"].clone());
            let outcome = match option {
                Some(option) => json!({"outcome": "selected", "optionId": option}),
                None => json!({"outcome": "cancelled"}),
            };
            json!({"jsonrpc": "2.0", "id": id, "result": {"outcome": outcome}})
        } else {
            json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "method not found"}})
        };
        self.replies.push(reply.to_string());
    }
}

impl Normalizer for AcpStream {
    fn line(&mut self, line: &Value, out: &mut Vec<SunEvent>) -> bool {
        if let Some(method) = line["method"].as_str() {
            if line.get("id").is_some() {
                self.answer(method, line);
            } else if method == "session/update" && self.resume.is_none() {
                self.update(&line["params"]["update"], out);
            }
            return false;
        }
        let handshake_failed = |out: &mut Vec<SunEvent>| {
            let error = &line["error"];
            out.push(SunEvent::Error {
                message: format!(
                    "{ID} ACP handshake failed: {}",
                    error["message"].as_str().unwrap_or("no session id")
                ),
            });
            true
        };
        match line["id"].as_u64() {
            Some(INITIALIZE) if line["error"].is_null() => {
                let cwd = self.cwd.clone().unwrap_or_default();
                let mcp = &self.mcp_servers;
                let request = match &self.resume {
                    Some(id) => request(
                        SESSION_NEW,
                        "session/load",
                        json!({"sessionId": id, "cwd": cwd, "mcpServers": mcp}),
                    ),
                    None => request(
                        SESSION_NEW,
                        "session/new",
                        json!({"cwd": cwd, "mcpServers": mcp}),
                    ),
                };
                self.replies.push(request);
                false
            }
            Some(SESSION_NEW) => {
                // `session/load` answers `null`; `session/new` the new id.
                let session_id = match self.resume.take() {
                    Some(id) if line["error"].is_null() => id,
                    Some(_) => return handshake_failed(out),
                    None => match line["result"]["sessionId"].as_str() {
                        Some(id) => id.to_owned(),
                        None => return handshake_failed(out),
                    },
                };
                if let Some(tx) = &self.session_id {
                    tx.send_replace(Some(session_id.clone()));
                }
                let envelope = std::mem::take(&mut self.envelope);
                self.replies
                    .push(prompt(FIRST_PROMPT, &session_id, &envelope));
                false
            }
            Some(INITIALIZE) => handshake_failed(out),
            _ => {
                self.prompt_done(line, out);
                true
            }
        }
    }

    fn replies(&mut self) -> Vec<String> {
        std::mem::take(&mut self.replies)
    }

    fn exits_on_eof(&self) -> bool {
        false
    }
}

/// A JSON-RPC error on `session/prompt`: a `LimitHit` when the text or the
/// provider status says so, else an `Error`.
fn prompt_error(error: &Value) -> SunEvent {
    let data = &error["data"];
    let message = data["message"]
        .as_str()
        .or(error["message"].as_str())
        .unwrap_or("session/prompt failed");
    let kind =
        limit::classify(message).or((data["statusCode"] == 429).then_some(LimitKind::RateLimit));
    let Some(limit) = kind else {
        return SunEvent::Error {
            message: message.to_owned(),
        };
    };
    let now = Utc::now();
    let retry_after = data["responseHeaders"]["retry-after"]
        .as_str()
        .and_then(|secs| secs.trim().parse().ok())
        .map(|secs| now + chrono::Duration::seconds(secs));
    SunEvent::LimitHit {
        limit,
        reset_at: retry_after.or_else(|| limit::parse_reset(message, now)),
        raw: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn reply(stream: &mut AcpStream, kind: &str) -> Value {
        let request = json!({"jsonrpc": "2.0", "id": 9, "method": "session/request_permission",
            "params": {"toolCall": {"kind": kind}, "options": [
                {"optionId": "a", "kind": "allow_once"}, {"optionId": "r", "kind": "reject_once"}]}});
        stream.answer("session/request_permission", &request);
        let reply: Value = serde_json::from_str(&stream.replies.pop().unwrap()).unwrap();
        reply["result"]["outcome"]["optionId"].clone()
    }

    #[test]
    fn permission_requests_follow_the_policy() {
        let mut edits = AcpStream::default().with_permissions(PermissionMode::AcceptEdits, false);
        assert_eq!(reply(&mut edits, "edit"), "a");
        assert_eq!(reply(&mut edits, "execute"), "r");
        assert_eq!(reply(&mut edits, ""), "r", "unknown kinds are refused");
        let mut shell = AcpStream::default().with_permissions(PermissionMode::Default, true);
        assert_eq!(reply(&mut shell, "edit"), "r");
        assert_eq!(reply(&mut shell, "execute"), "a");
        let mut all =
            AcpStream::default().with_permissions(PermissionMode::BypassPermissions, false);
        assert_eq!(reply(&mut all, "fetch"), "a");
    }

    /// BLO-163: the mode table of the module doc, kind by kind, with
    /// `allow_shell` on (the gates' commands), for every mode.
    #[test]
    fn every_mode_answers_by_its_table() {
        let kinds = ["read", "edit", "execute", "fetch"];
        let table = [
            (PermissionMode::Plan, ["a", "r", "r", "r"]),
            (PermissionMode::Default, ["a", "r", "a", "r"]),
            (PermissionMode::AcceptEdits, ["a", "a", "a", "r"]),
            (PermissionMode::Auto, ["a", "a", "a", "r"]),
            (PermissionMode::BypassPermissions, ["a", "a", "a", "a"]),
        ];
        assert_eq!(table.len(), PermissionMode::ALL.len());
        for (mode, answers) in table {
            let mut stream = AcpStream::default().with_permissions(mode, true);
            for (kind, answer) in kinds.into_iter().zip(answers) {
                assert_eq!(reply(&mut stream, kind), answer, "{mode} {kind}");
            }
        }
    }
}
