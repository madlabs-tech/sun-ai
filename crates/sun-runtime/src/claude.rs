//! Claude Code **Adapter** over headless JSON:
//! `claude -p --input-format stream-json --output-format stream-json --verbose`.
//!
//! The envelope and every steer go in as stream-json user messages on stdin;
//! the driver closes stdin once all turns are answered and Claude exits.
//! Runs on whatever login the CLI has (BYO runtime): the user's
//! subscription, or a key/endpoint passed in `SessionSpec::env`.

use std::path::PathBuf;

use async_trait::async_trait;
use chrono::{TimeZone, Utc};
use serde_json::{Value, json};
use sun_core::{
    BoxStream, EffortCatalog, EffortLevel, Error, HostCtx, LimitKind, PermissionMode, Readiness,
    Result, Role, RuntimeAdapter, SessionHandle, SessionId, SessionSpec, Steer, StopMode, SunEvent,
    Todo, TodoStatus, Transcript,
};
use tokio::process::Command;

use crate::process::{
    self, Normalizer, Probe, Process, Sessions, args_hash, readiness, relative, stream, truncate,
};
use crate::{envelope, limit};

/// Adapter id.
pub const ID: &str = "claude-code";

/// Claude Code runtime adapter.
pub struct ClaudeCode {
    /// The `claude` binary (a bare name is looked up on `PATH`).
    pub binary: PathBuf,
    /// The mode of sessions whose spec names none, mapped to
    /// `--permission-mode` by [`cli_permission_mode`]. Defaults to
    /// `accept_edits`, the mode the fixtures were recorded with.
    pub permission_mode: PermissionMode,
    /// `--allowedTools` (e.g. `Bash(cargo test:*)`): tools allowed without a
    /// prompt on top of the mode, such as the commands the gates run.
    pub allowed_tools: Vec<String>,
    /// Where a user's `claude-code.md` envelope template overrides the
    /// built-in one (sun.yaml `runtimes.template_dir`).
    pub template_dir: Option<PathBuf>,
    sessions: Sessions<Process>,
}

impl Default for ClaudeCode {
    fn default() -> Self {
        Self::new("claude")
    }
}

impl ClaudeCode {
    /// An adapter that runs `binary`.
    pub fn new(binary: impl Into<PathBuf>) -> Self {
        Self {
            binary: binary.into(),
            permission_mode: PermissionMode::AcceptEdits,
            allowed_tools: Vec::new(),
            template_dir: None,
            sessions: Sessions::default(),
        }
    }

    fn session(&self, id: &SessionId) -> Result<std::sync::Arc<Process>> {
        self.sessions.get(ID, id)
    }
}

#[async_trait]
impl RuntimeAdapter for ClaudeCode {
    fn id(&self) -> &str {
        ID
    }

    async fn readiness(&self, _host: &HostCtx) -> Readiness {
        readiness(&Probe {
            name: "Claude Code",
            binary: &self.binary,
            cli: ID,
            install: "npm install -g @anthropic-ai/claude-code",
            login_args: &["auth", "status", "--json"],
            logged_in: |out| parse_logged_in(&String::from_utf8_lossy(&out.stdout)),
            login: "claude auth login",
        })
        .await
    }

    async fn effort(&self, _host: &HostCtx) -> EffortCatalog {
        let help = process::run(&self.binary, &["--help"]).await;
        EffortCatalog {
            effort_options: help
                .map(|out| parse_effort_help(&String::from_utf8_lossy(&out.stdout)))
                .unwrap_or_default(),
            models: Vec::new(),
        }
    }

    async fn start(&self, mut spec: SessionSpec) -> Result<SessionHandle> {
        let mode = PermissionMode::parse_opt(spec.permission_mode.as_deref())
            .map_err(|why| Error::InvalidInput(format!("session permission_mode: {why}")))?
            .unwrap_or(self.permission_mode);
        crate::byo_key::native_env(&mut spec, ID)?;
        let prompt = envelope::first_prompt(ID, self.template_dir.as_deref(), &spec)?;
        // Resuming keeps the conversation's id (no `--fork-session`).
        let (flag, native_id) = match &spec.resume {
            Some(id) => ("--resume", id.clone()),
            None => ("--session-id", native_session_id(spec.session_id)),
        };
        let mut cmd = Command::new(process::program(&self.binary, &spec.env));
        cmd.args(["-p", "--input-format", "stream-json"])
            .args(["--output-format", "stream-json", "--verbose"])
            .args([flag, &native_id])
            .args(["--permission-mode", cli_permission_mode(mode)]);
        // Sun's own MCP tools (`sun.task`, `sun.done`, …) never wait for a
        // permission prompt: a headless session can't answer one.
        let sun_tools = spec.mcp_config.as_ref().map(|_| "mcp__sun".to_owned());
        let allowed: Vec<String> = self
            .allowed_tools
            .iter()
            .cloned()
            .chain(sun_tools)
            .collect();
        if !allowed.is_empty() {
            cmd.arg("--allowedTools").arg(allowed.join(","));
        }
        if let Some(model) = &spec.model {
            cmd.args(["--model", model]);
        }
        if let Some(effort) = crate::effort::checked(spec.effort.as_deref())? {
            cmd.args(["--effort", effort]);
        }
        if let Some(mcp) = &spec.mcp_config {
            cmd.arg("--mcp-config").arg(mcp).arg("--strict-mcp-config");
        }
        cmd.current_dir(&spec.worktree).envs(&spec.env);
        let process = Process::spawn(
            ID,
            cmd,
            ClaudeStream::default(),
            Some(user_message(&prompt)),
            spec.env.into_values().collect(),
        )
        .await?;
        let handle = SessionHandle {
            session_id: spec.session_id,
            native_id: Some(native_id),
            pid: process.pid,
        };
        self.sessions.insert(spec.session_id, process);
        Ok(handle)
    }

    async fn send(&self, session: &SessionHandle, msg: Steer) -> Result<()> {
        self.session(&session.session_id)?
            .send_line(&user_message(&msg.text), ID)
            .await
    }

    async fn stop(&self, session: &SessionHandle, how: StopMode) -> Result<()> {
        self.session(&session.session_id)?.stop(how);
        Ok(())
    }

    fn events(&self, session: &SessionHandle) -> BoxStream<'static, SunEvent> {
        match self.session(&session.session_id) {
            Ok(process) => process.events(),
            Err(_) => stream(None),
        }
    }

    // ponytail: no native bytes; resume on the same host uses
    // `claude --resume <native_id>`. Read ~/.claude/projects when a hand-off
    // has to move a session to another host.
    async fn export(&self, session: &SessionHandle) -> Result<Transcript> {
        Ok(Transcript {
            events: self.session(&session.session_id)?.log(),
            native: None,
        })
    }

    fn context_window(&self, model: &str) -> Option<u32> {
        if model.ends_with("[1m]") {
            Some(1_000_000)
        } else if model.starts_with("claude-")
            || matches!(model, "default" | "opus" | "sonnet" | "haiku" | "opusplan")
        {
            Some(200_000)
        } else {
            None
        }
    }
}

/// Claude wants a UUID for `--session-id`; Sun's ULID is 128 bits too, so
/// format it as one. Deterministic, so a crashed session can be resumed from
/// Sun's id alone.
pub fn native_session_id(id: SessionId) -> String {
    let hex = format!("{:032x}", u128::from(id.0));
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}

/// Sun's permission mode → Claude Code's `--permission-mode` spelling
/// (Adapter boundary). Only Sun's modes exist here: no other spelling can
/// reach the CLI.
pub fn cli_permission_mode(mode: PermissionMode) -> &'static str {
    match mode {
        PermissionMode::Default => "default",
        PermissionMode::AcceptEdits => "acceptEdits",
        PermissionMode::Plan => "plan",
        PermissionMode::BypassPermissions => "bypassPermissions",
    }
}

/// One stream-json user message.
fn user_message(text: &str) -> String {
    json!({
        "type": "user",
        "message": {"role": "user", "content": [{"type": "text", "text": text}]},
    })
    .to_string()
}

/// `claude --help` → the levels its `--effort` option lists, e.g.
/// `--effort <level>  Effort level for the current session (low, medium,
/// high, xhigh, max)`, the list possibly wrapped onto the next line. Claude
/// Code scopes effort per session, not per model. Empty when the CLI
/// predates the flag or the list doesn't parse (the picker greys out).
pub fn parse_effort_help(help: &str) -> Vec<EffortLevel> {
    // The option's own line, not a mention in another option's text.
    let defined = help.match_indices("--effort").find(|(at, _)| {
        let line_start = help[..*at].rfind('\n').map_or(0, |n| n + 1);
        help[line_start..*at].trim().is_empty()
    });
    let Some((at, _)) = defined else {
        return Vec::new();
    };
    let rest = &help[at + "--effort".len()..];
    // The option's text ends where the next option's line starts.
    let text = rest.find("\n  -").map_or(rest, |end| &rest[..end]);
    let Some(list) = text
        .split_once('(')
        .and_then(|(_, after)| after.split_once(')'))
        .map(|(list, _)| list)
    else {
        return Vec::new();
    };
    list.split(',')
        .filter_map(|value| crate::effort::level(value.trim(), None))
        .collect()
}

/// `claude auth status --json` → `loggedIn`.
pub fn parse_logged_in(out: &str) -> bool {
    serde_json::from_str::<Value>(out).is_ok_and(|v| v["loggedIn"] == true)
}

/// Normalizes Claude Code's stream-json output to [`SunEvent`]s.
#[derive(Debug, Default)]
pub struct ClaudeStream {
    /// The session's working directory (from `system/init`); file paths are
    /// reported relative to it.
    cwd: Option<PathBuf>,
    /// Claude repeats a message (and its usage) once per content block.
    message_id: Option<String>,
    /// One `LimitHit` per turn, though Claude reports a limit up to 3 times.
    limit_reported: bool,
}

impl ClaudeStream {
    fn limit(
        &mut self,
        out: &mut Vec<SunEvent>,
        limit: LimitKind,
        reset_at: Option<sun_core::Timestamp>,
        raw: String,
    ) {
        if !self.limit_reported {
            self.limit_reported = true;
            out.push(SunEvent::LimitHit {
                limit,
                reset_at,
                raw,
            });
        }
    }

    /// A limit from free text: the shared classifier, defaulting to `fallback`.
    fn limit_text(&mut self, out: &mut Vec<SunEvent>, text: &str, fallback: LimitKind) {
        let kind = limit::classify(text).unwrap_or(fallback);
        self.limit(
            out,
            kind,
            limit::parse_reset(text, Utc::now()),
            text.to_owned(),
        );
    }

    fn assistant(&mut self, line: &Value, out: &mut Vec<SunEvent>) {
        let message = &line["message"];
        let id = message["id"].as_str().map(str::to_owned);
        if id.is_some() && id != self.message_id {
            self.message_id = id;
            let usage = &message["usage"];
            if usage.is_object() {
                out.push(SunEvent::Usage {
                    input: 0,
                    output: 0,
                    cost_usd: None,
                    context_used: u32::try_from(prompt_tokens(usage)).ok(),
                });
            }
        }
        let blocks = message["content"].as_array().map_or(&[][..], Vec::as_slice);
        if line["error"].is_string() {
            // An API error rendered as a message, e.g. `error: "rate_limit"`.
            let text = texts(blocks);
            if line["error"] == "rate_limit" || limit::classify(&text).is_some() {
                self.limit_text(out, &text, LimitKind::RateLimit);
            } else {
                out.push(SunEvent::Error { message: text });
            }
            return;
        }
        for block in blocks {
            match block["type"].as_str() {
                Some("text") => out.push(SunEvent::Text {
                    role: Role::Assistant,
                    text: block["text"].as_str().unwrap_or_default().to_owned(),
                }),
                Some("tool_use") => {
                    let name = block["name"].as_str().unwrap_or("tool");
                    let input = &block["input"];
                    out.push(SunEvent::ToolCall {
                        name: name.to_owned(),
                        args_hash: args_hash(input),
                        summary: tool_summary(name, input),
                    });
                    if name == "TodoWrite"
                        && let Some(todos) = input["todos"].as_array()
                    {
                        out.push(SunEvent::Plan {
                            todos: todos.iter().map(todo).collect(),
                        });
                    }
                }
                _ => {} // thinking: not surfaced
            }
        }
    }

    fn user(&self, line: &Value, out: &mut Vec<SunEvent>) {
        let Some(blocks) = line["message"]["content"].as_array() else {
            return; // a plain user message (our own input echoed back)
        };
        for block in blocks.iter().filter(|b| b["type"] == "tool_result") {
            let ok = block["is_error"] != true;
            let content = match &block["content"] {
                Value::String(text) => text.clone(),
                Value::Array(parts) => texts(parts),
                _ => String::new(),
            };
            out.push(SunEvent::ToolResult {
                ok,
                summary: truncate(&content, 200),
            });
            // Write/Edit report the file at the top level (Read nests it under `file`).
            if ok && let Some(path) = line["tool_use_result"]["filePath"].as_str() {
                out.push(SunEvent::FileChanged {
                    path: relative(self.cwd.as_deref(), path),
                });
            }
        }
    }

    fn result(&mut self, line: &Value, out: &mut Vec<SunEvent>) {
        let usage = &line["usage"];
        if usage.is_object() {
            // ponytail: assumes `result` usage covers only this turn; verify
            // with a multi-turn recording before trusting cumulative totals.
            out.push(SunEvent::Usage {
                input: prompt_tokens(usage),
                output: usage["output_tokens"].as_u64().unwrap_or(0),
                cost_usd: line["total_cost_usd"].as_f64(),
                context_used: None,
            });
        }
        let text = line["result"].as_str().unwrap_or_default();
        if line["is_error"] == true {
            if line["api_error_status"] == 429 || limit::classify(text).is_some() {
                self.limit_text(out, text, LimitKind::RateLimit);
            } else {
                let subtype = line["subtype"].as_str().unwrap_or("error");
                out.push(SunEvent::Error {
                    message: if text.is_empty() {
                        subtype.to_owned()
                    } else {
                        text.to_owned()
                    },
                });
            }
        } else {
            out.push(SunEvent::DoneClaimed {
                summary: text.to_owned(),
            });
        }
        self.limit_reported = false;
    }
}

impl Normalizer for ClaudeStream {
    fn line(&mut self, line: &Value, out: &mut Vec<SunEvent>) -> bool {
        match line["type"].as_str() {
            Some("system") => {
                if line["subtype"] == "init"
                    && let Some(cwd) = line["cwd"].as_str()
                {
                    self.cwd = Some(PathBuf::from(cwd));
                }
            }
            Some("assistant") => self.assistant(line, out),
            Some("user") => self.user(line, out),
            Some("rate_limit_event") => {
                let info = &line["rate_limit_info"];
                if info["status"] == "rejected" {
                    let reset_at = info["resetsAt"]
                        .as_i64()
                        .and_then(|secs| Utc.timestamp_opt(secs, 0).single());
                    // Subscription windows (five_hour, seven_day, ...) are quotas.
                    self.limit(out, LimitKind::Quota, reset_at, info.to_string());
                }
            }
            Some("result") => {
                self.result(line, out);
                return true;
            }
            _ => {}
        }
        false
    }
}

/// Prompt-side tokens: fresh input plus cache writes and reads.
fn prompt_tokens(usage: &Value) -> u64 {
    [
        "input_tokens",
        "cache_creation_input_tokens",
        "cache_read_input_tokens",
    ]
    .iter()
    .filter_map(|key| usage[key].as_u64())
    .sum()
}

/// Joins the `text` parts of a content array.
fn texts(blocks: &[Value]) -> String {
    blocks
        .iter()
        .filter_map(|b| b["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

/// `Write hello.txt`, `Bash cargo test`, ...: the tool name and its main argument.
fn tool_summary(name: &str, input: &Value) -> String {
    let arg = [
        "command",
        "file_path",
        "path",
        "pattern",
        "url",
        "description",
        "prompt",
    ]
    .iter()
    .find_map(|key| input[key].as_str())
    .map_or_else(|| input.to_string(), str::to_owned);
    truncate(&format!("{name} {arg}"), 120)
}

/// One TodoWrite item; ACP `plan` entries have the same shape.
pub(crate) fn todo(item: &Value) -> Todo {
    Todo {
        text: item["content"].as_str().unwrap_or_default().to_owned(),
        status: match item["status"].as_str() {
            Some("completed") => TodoStatus::Done,
            Some("in_progress") => TodoStatus::InProgress,
            _ => TodoStatus::Pending,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::cli_permission_mode;

    #[test]
    fn permission_mode_maps_at_the_adapter() {
        let mapped = sun_core::PermissionMode::ALL.map(cli_permission_mode);
        assert_eq!(
            mapped,
            ["default", "acceptEdits", "plan", "bypassPermissions"]
        );
    }
}
