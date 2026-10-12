//! The fake runtime: a [`RuntimeAdapter`] over the
//! `sun-fake-runtime` binary, which replays a script of [`SunEvent`]s. Limit
//! hits, loops, stalls, crashes and bad "done" claims become a few lines of
//! JSONL, so engine features are tested without a real model.
//!
//! **Pattern:** **Adapter**, on the same shared JSONL [`Process`] driver as
//! the real CLIs (redaction, the EOF → `Error` rule, `Exited` last, stop).
//!
//! ## Script format
//! One JSON object per line (blank lines are skipped): a [`SunEvent`] in its
//! wire form plus an optional `delay_ms` to sleep before emitting it.
//!
//! ```text
//! {"kind":"tool_call","name":"Bash","args_hash":7,"summary":"cargo test"}
//! {"delay_ms":1500,"kind":"done_claimed","summary":"all green"}
//! ```
//!
//! - `{"kind":"exited","code":3}` ends the process with that code instead of
//!   being printed (`"code":null` aborts, like a crash by signal). A script
//!   that ends without `done_claimed`, `limit_hit` or `error` therefore reads
//!   as a crash: the driver adds an `Error`.
//! - A stall is a big `delay_ms`; a loop is the same `tool_call` repeated.
//! - `{"kind":"file_changed","path":"src/lib.rs","write":"text"}` also
//!   writes `text` to `path` in the session's worktree (the process's cwd),
//!   so gates such as `diff_not_empty` see a real change.
//! - `{"kind":"limit_hit",...,"reset_in_ms":1000}` sets `reset_at` to the
//!   moment the line is emitted plus that much, so a short cooldown starts
//!   when the limit is hit, not when the script was written.
//! - Every stdin line is a JSON string. The first is the envelope; each later
//!   one is a steer, echoed back as a user `text` event.

use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use sun_core::{
    BoxStream, Error, HostCtx, Readiness, Result, Role, RuntimeAdapter, SessionHandle, SessionSpec,
    Steer, StopMode, SunEvent, Transcript,
};
use tokio::process::Command;

use crate::process::{Normalizer, Process, Sessions};

/// Name of the binary and of the CLI in driver messages.
pub const BINARY: &str = "sun-fake-runtime";

/// One script line: wait `delay`, then emit `event`.
#[derive(Debug, Clone, PartialEq)]
pub struct ScriptLine {
    /// Sleep before the event.
    pub delay: Duration,
    /// The event to emit.
    pub event: SunEvent,
    /// `write`: text the binary writes to a `file_changed` path first.
    pub write: Option<String>,
    /// `reset_in_ms`: a `limit_hit`'s `reset_at` is this long after emission.
    pub reset_in: Option<Duration>,
}

/// Parses a script (see the module docs). Errors name the 1-based line.
pub fn parse_script(text: &str) -> Result<Vec<ScriptLine>> {
    let mut out = Vec::new();
    for (index, raw) in text.lines().enumerate() {
        if raw.trim().is_empty() {
            continue;
        }
        let bad = |why: String| Error::InvalidInput(format!("script line {}: {why}", index + 1));
        let mut value: Value = serde_json::from_str(raw).map_err(|e| bad(e.to_string()))?;
        let delay = match value.as_object_mut().and_then(|o| o.remove("delay_ms")) {
            None => 0,
            Some(ms) => ms
                .as_u64()
                .ok_or_else(|| bad("`delay_ms` must be a whole number".into()))?,
        };
        let write = match value.as_object_mut().and_then(|o| o.remove("write")) {
            None => None,
            Some(Value::String(text)) => Some(text),
            Some(_) => return Err(bad("`write` must be a string".into())),
        };
        let reset_in = match value.as_object_mut().and_then(|o| o.remove("reset_in_ms")) {
            None => None,
            Some(ms) => {
                Some(Duration::from_millis(ms.as_u64().ok_or_else(|| {
                    bad("`reset_in_ms` must be a whole number".into())
                })?))
            }
        };
        let event: SunEvent = serde_json::from_value(value).map_err(|e| bad(e.to_string()))?;
        if write.is_some() && !matches!(event, SunEvent::FileChanged { .. }) {
            return Err(bad("`write` only goes on a `file_changed` line".into()));
        }
        if reset_in.is_some() && !matches!(event, SunEvent::LimitHit { .. }) {
            return Err(bad("`reset_in_ms` only goes on a `limit_hit` line".into()));
        }
        out.push(ScriptLine {
            delay: Duration::from_millis(delay),
            event,
            write,
            reset_in,
        });
    }
    Ok(out)
}

/// Reads the binary's stdout: every line already is a [`SunEvent`].
/// A turn ends on `done_claimed`, `limit_hit`, `error`, or the echo of a steer.
#[derive(Debug, Default)]
pub struct FakeStream;

impl Normalizer for FakeStream {
    fn line(&mut self, line: &Value, out: &mut Vec<SunEvent>) -> bool {
        let Ok(event) = serde_json::from_value::<SunEvent>(line.clone()) else {
            return false;
        };
        let ends_turn = matches!(
            event,
            SunEvent::DoneClaimed { .. }
                | SunEvent::LimitHit { .. }
                | SunEvent::Error { .. }
                | SunEvent::Text {
                    role: Role::User,
                    ..
                }
        );
        // The driver emits the one real `Exited`.
        if !matches!(event, SunEvent::Exited { .. }) {
            out.push(event);
        }
        ends_turn
    }
}

/// **Adapter** that runs [`BINARY`] on a fixed script. Give each scripted
/// behavior its own id (e.g. `fake-limit`, `fake-ok`) to build failover chains.
pub struct FakeRuntime {
    id: String,
    binary: PathBuf,
    script: PathBuf,
    sessions: Sessions<Process>,
}

impl FakeRuntime {
    /// A fake runtime `id` that runs `binary` (the `sun-fake-runtime` build)
    /// on the JSONL `script`.
    pub fn new(
        id: impl Into<String>,
        binary: impl Into<PathBuf>,
        script: impl Into<PathBuf>,
    ) -> Self {
        Self {
            id: id.into(),
            binary: binary.into(),
            script: script.into(),
            sessions: Sessions::default(),
        }
    }
}

/// A JSON string literal: one stdin line, whatever the text holds.
fn line(text: &str) -> Result<String> {
    serde_json::to_string(text).map_err(|e| Error::InvalidInput(e.to_string()))
}

#[async_trait]
impl RuntimeAdapter for FakeRuntime {
    fn id(&self) -> &str {
        &self.id
    }

    /// Installed when the binary exists and the script parses; no login.
    async fn readiness(&self, _host: &HostCtx) -> Readiness {
        let problem = if self.binary.is_file() {
            std::fs::read_to_string(&self.script)
                .map_err(|e| Error::InvalidInput(format!("{}: {e}", self.script.display())))
                .and_then(|text| parse_script(&text))
                .err()
                .map(|e| e.to_string())
        } else {
            Some(format!(
                "{} not found; run `cargo build -p sun-runtime --bin {BINARY}`",
                self.binary.display()
            ))
        };
        Readiness {
            installed: problem.is_none(),
            version: Some(env!("CARGO_PKG_VERSION").to_owned()),
            logged_in: true,
            detail: problem,
        }
    }

    async fn start(&self, spec: SessionSpec) -> Result<SessionHandle> {
        let mut cmd = Command::new(&self.binary);
        cmd.arg(&self.script)
            .current_dir(&spec.worktree)
            .envs(&spec.env);
        let secrets = spec.env.values().cloned().collect();
        let process = Process::spawn(
            BINARY,
            cmd,
            FakeStream,
            Some(line(&spec.envelope)?),
            secrets,
        )
        .await?;
        let pid = process.pid;
        self.sessions.insert(spec.session_id, process);
        // "Resumes" by keeping the id; the script decides what happens next.
        Ok(SessionHandle {
            session_id: spec.session_id,
            native_id: Some(spec.resume.unwrap_or_else(|| spec.session_id.to_string())),
            pid,
        })
    }

    async fn send(&self, session: &SessionHandle, msg: Steer) -> Result<()> {
        let process = self.sessions.get(&self.id, &session.session_id)?;
        process.send_line(&line(&msg.text)?, BINARY).await
    }

    async fn stop(&self, session: &SessionHandle, how: StopMode) -> Result<()> {
        self.sessions.get(&self.id, &session.session_id)?.stop(how);
        Ok(())
    }

    fn events(&self, session: &SessionHandle) -> BoxStream<'static, SunEvent> {
        match self.sessions.get(&self.id, &session.session_id) {
            Ok(process) => process.events(),
            Err(_) => crate::process::stream(None),
        }
    }

    async fn export(&self, session: &SessionHandle) -> Result<Transcript> {
        let process = self.sessions.get(&self.id, &session.session_id)?;
        Ok(Transcript {
            events: process.log(),
            native: None,
        })
    }

    fn context_window(&self, _model: &str) -> Option<u32> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fake_script_parses_delays_and_names_bad_lines() {
        let script = "{\"kind\":\"text\",\"role\":\"assistant\",\"text\":\"hi\"}\n\n\
                      {\"delay_ms\":250,\"kind\":\"exited\",\"code\":3}\n";
        assert_eq!(
            parse_script(script).unwrap(),
            [
                ScriptLine {
                    delay: Duration::ZERO,
                    event: SunEvent::Text {
                        role: Role::Assistant,
                        text: "hi".into()
                    },
                    write: None,
                    reset_in: None,
                },
                ScriptLine {
                    delay: Duration::from_millis(250),
                    event: SunEvent::Exited { code: Some(3) },
                    write: None,
                    reset_in: None,
                },
            ]
        );
        let bad = parse_script(
            "{\"kind\":\"text\",\"role\":\"assistant\",\"text\":\"\"}\n{\"kind\":\"nope\"}",
        );
        assert!(bad.unwrap_err().to_string().contains("line 2"));
        let bad = parse_script("{\"delay_ms\":-1,\"kind\":\"exited\",\"code\":0}");
        assert!(bad.unwrap_err().to_string().contains("delay_ms"));
    }
}
