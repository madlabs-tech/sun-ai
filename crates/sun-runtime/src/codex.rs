//! Codex **Adapter** over headless JSON:
//! `codex exec --json`, one process per turn.
//!
//! `codex exec` takes no input while it runs, so a steer is queued and sent
//! as the next turn with `codex exec resume <thread_id>` once the current
//! turn completes. The resumed processes are chained into one event stream
//! with a single `Exited` at the end. Runs on whatever login the CLI has
//! (BYO runtime).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::Utc;
use serde_json::{Value, json};
use sun_core::{
    BoxStream, EffortCatalog, Error, HostCtx, ModelEffort, Readiness, Result, Role, RuntimeAdapter,
    SessionHandle, SessionId, SessionSpec, Steer, StopMode, SunEvent, Todo, TodoStatus, Transcript,
};
use tokio::process::Command;
use tokio::sync::{mpsc, watch};

use crate::process::{
    self, Normalizer, Probe, Process, Sessions, args_hash, native_id, readiness, relative, stream,
    truncate,
};
use crate::{envelope, limit};

/// Adapter id.
pub const ID: &str = "codex";

/// Codex runtime adapter.
pub struct Codex {
    /// The `codex` binary (a bare name is looked up on `PATH`).
    pub binary: PathBuf,
    /// `sandbox_mode` for sessions; policy may pick another.
    pub sandbox: String,
    /// Where a user's `codex.md` envelope template overrides the built-in
    /// one (sun.yaml `runtimes.template_dir`).
    pub template_dir: Option<PathBuf>,
    sessions: Sessions<Chain>,
}

impl Default for Codex {
    fn default() -> Self {
        Self::new("codex")
    }
}

impl Codex {
    /// An adapter that runs `binary`.
    pub fn new(binary: impl Into<PathBuf>) -> Self {
        Self {
            binary: binary.into(),
            sandbox: "workspace-write".into(),
            template_dir: None,
            sessions: Sessions::default(),
        }
    }

    fn session(&self, id: &SessionId) -> Result<Arc<Chain>> {
        self.sessions.get(ID, id)
    }
}

/// How to start one `codex exec` turn of a session.
#[derive(Clone)]
struct Launch {
    binary: PathBuf,
    sandbox: String,
    spec: SessionSpec,
}

impl Launch {
    /// `codex exec [--skip-git-repo-check] --json -c … [resume <thread>] -- <prompt>`.
    async fn spawn(
        &self,
        resume: Option<&str>,
        prompt: &str,
        thread_id: watch::Sender<Option<String>>,
    ) -> Result<Process> {
        let mut cmd = Command::new(process::program(&self.binary, &self.spec.env));
        cmd.arg("exec");
        // Codex refuses a cwd outside a git repo. A folder task (or the parent
        // dir of a multi-repo task) is a scratch dir Sun made for the task;
        // `sandbox_mode` still bounds what the agent may write.
        if !in_git_repo(&self.spec.worktree) {
            cmd.arg("--skip-git-repo-check");
        }
        cmd.args(["--json", "-c"])
            .arg(format!("sandbox_mode={}", json!(self.sandbox)));
        if let Some(model) = &self.spec.model {
            cmd.arg("-c").arg(format!("model={}", json!(model)));
        }
        if let Some(effort) = crate::effort::checked(self.spec.effort.as_deref())? {
            let effort = json!(effort);
            cmd.arg("-c")
                .arg(format!("model_reasoning_effort={effort}"));
        }
        // Codex reads MCP servers from config.toml; override per session.
        // Their env (the session token) rides in `spec.env`, never argv.
        if let Some(path) = &self.spec.mcp_config {
            for server in crate::mcp::read_mcp_config(path)? {
                cmd.args(server.codex_args());
            }
        }
        if let Some(thread) = resume {
            cmd.args(["resume", thread]);
        }
        cmd.arg("--")
            .arg(prompt)
            .current_dir(&self.spec.worktree)
            .envs(&self.spec.env);
        let normalizer = CodexStream::new(Some(self.spec.worktree.clone()), Some(thread_id));
        let secrets = self.spec.env.values().cloned().collect();
        Process::spawn(ID, cmd, normalizer, None, secrets).await
    }
}

/// Whether `dir` is inside a git work tree as Codex checks it: a `.git`
/// entry in `dir` or one of its parents.
fn in_git_repo(dir: &Path) -> bool {
    dir.ancestors().any(|dir| dir.join(".git").exists())
}

/// One Codex session: a chain of `codex exec` processes, one per turn.
struct Chain {
    state: Mutex<ChainState>,
    events: Mutex<Option<mpsc::UnboundedReceiver<SunEvent>>>,
    log: Arc<Mutex<Vec<SunEvent>>>,
    thread_id: watch::Receiver<Option<String>>,
}

struct ChainState {
    current: Arc<Process>,
    /// Steers waiting for the current turn to finish.
    queue: Vec<String>,
    /// No more turns: the last process ended or a stop was asked for.
    closed: bool,
}

impl Chain {
    /// Forwards every process's events to the session stream, starting a
    /// resumed turn for queued steers, and ends with the last `Exited`.
    async fn forward(self: Arc<Self>, launch: Launch, tx: mpsc::UnboundedSender<SunEvent>) {
        let emit = |event: SunEvent| {
            if let Ok(mut log) = self.log.lock() {
                log.push(event.clone());
            }
            let _ = tx.send(event);
        };
        loop {
            let process = match self.state.lock() {
                Ok(state) => Arc::clone(&state.current),
                Err(_) => return,
            };
            let mut events = process.events();
            let (mut exited, mut done) = (SunEvent::Exited { code: None }, false);
            while let Some(event) = std::future::poll_fn(|cx| events.as_mut().poll_next(cx)).await {
                match event {
                    SunEvent::Exited { .. } => exited = event,
                    event => {
                        done |= matches!(event, SunEvent::DoneClaimed { .. });
                        emit(event);
                    }
                }
            }
            let thread = self.thread_id.borrow().clone();
            let mut dropped = Vec::new();
            let next = match (self.state.lock(), thread) {
                (Ok(mut state), Some(thread))
                    if done && !state.closed && !state.queue.is_empty() =>
                {
                    Some((
                        state.queue.drain(..).collect::<Vec<_>>().join("\n\n"),
                        thread,
                    ))
                }
                (Ok(mut state), _) => {
                    state.closed = true;
                    dropped = std::mem::take(&mut state.queue);
                    None
                }
                (Err(_), _) => None,
            };
            let Some((text, thread)) = next else {
                // `send` already returned Ok for these: say they were lost.
                if !dropped.is_empty() {
                    let secrets: Vec<&str> = launch.spec.env.values().map(String::as_str).collect();
                    let message = format!(
                        "{ID}: the turn did not finish; dropped queued steers: {}",
                        dropped.join(" | ")
                    );
                    emit(SunEvent::Error {
                        message: sun_core::redact::redact(&message, &secrets),
                    });
                }
                emit(exited);
                return;
            };
            // A dropped sender is fine: the thread id is already known.
            let (thread_tx, _) = watch::channel(None);
            match launch.spawn(Some(&thread), &text, thread_tx).await {
                Ok(process) => {
                    if let Ok(mut state) = self.state.lock() {
                        state.current = Arc::new(process);
                    }
                }
                Err(e) => {
                    if let Ok(mut state) = self.state.lock() {
                        state.closed = true;
                    }
                    emit(SunEvent::Error {
                        message: e.to_string(),
                    });
                    emit(exited);
                    return;
                }
            }
        }
    }
}

#[async_trait]
impl RuntimeAdapter for Codex {
    fn id(&self) -> &str {
        ID
    }

    async fn readiness(&self, _host: &HostCtx) -> Readiness {
        readiness(&Probe {
            name: "Codex",
            binary: &self.binary,
            cli: ID,
            install: "npm install -g @openai/codex",
            login_args: &["login", "status"],
            logged_in: |out| out.status.success(),
            login: "codex login",
        })
        .await
    }

    // ponytail: `--bundled` is the binary's own snapshot (no network, fast);
    // the live list adds account-only models but can take 15 s. Use it when
    // users ask for those models.
    async fn effort(&self, _host: &HostCtx) -> EffortCatalog {
        let out = process::run(&self.binary, &["debug", "models", "--bundled"]).await;
        let models = match out {
            Ok(out) if out.status.success() => parse_debug_models(&out.stdout),
            // Older than 0.122 (no `debug models`), or not installed.
            _ => Vec::new(),
        };
        EffortCatalog {
            effort_options: crate::effort::union(&models),
            models,
        }
    }

    async fn start(&self, mut spec: SessionSpec) -> Result<SessionHandle> {
        crate::byo_key::native_env(&mut spec, ID)?;
        crate::mcp::native_env(&mut spec, ID)?;
        let session_id = spec.session_id;
        let envelope = envelope::first_prompt(ID, self.template_dir.as_deref(), &spec)?;
        let launch = Launch {
            binary: self.binary.clone(),
            sandbox: self.sandbox.clone(),
            spec,
        };
        // A resumed thread keeps its id, known before `thread.started`.
        let resume = launch.spec.resume.clone();
        let (thread_tx, mut thread_rx) = watch::channel(resume.clone());
        let process = launch
            .spawn(resume.as_deref(), &envelope, thread_tx)
            .await?;
        let pid = process.pid;
        let (tx, rx) = mpsc::unbounded_channel();
        let chain = self.sessions.insert(
            session_id,
            Chain {
                state: Mutex::new(ChainState {
                    current: Arc::new(process),
                    queue: Vec::new(),
                    closed: false,
                }),
                events: Mutex::new(Some(rx)),
                log: Arc::default(),
                thread_id: thread_rx.clone(),
            },
        );
        tokio::spawn(chain.forward(launch, tx));
        Ok(SessionHandle {
            session_id,
            native_id: native_id(&mut thread_rx).await,
            pid,
        })
    }

    /// Queues the steer for the next turn (`codex exec` can't take input
    /// mid-turn). Steers still queued when a turn fails are dropped and
    /// named in an `Error` event.
    async fn send(&self, session: &SessionHandle, msg: Steer) -> Result<()> {
        let chain = self.session(&session.session_id)?;
        match chain.state.lock() {
            Ok(mut state) if !state.closed => {
                state.queue.push(msg.text);
                Ok(())
            }
            _ => Err(Error::Unavailable(format!(
                "{ID} session is no longer accepting input"
            ))),
        }
    }

    async fn stop(&self, session: &SessionHandle, how: StopMode) -> Result<()> {
        let chain = self.session(&session.session_id)?;
        if let Ok(mut state) = chain.state.lock() {
            state.closed = true;
            state.queue.clear();
            state.current.stop(how);
        }
        Ok(())
    }

    fn events(&self, session: &SessionHandle) -> BoxStream<'static, SunEvent> {
        stream(
            self.session(&session.session_id)
                .ok()
                .and_then(|chain| chain.events.lock().ok()?.take()),
        )
    }

    // ponytail: no native bytes; resume on the same host uses
    // `codex exec resume <native_id>`. Read ~/.codex/sessions when a hand-off
    // has to move a session to another host.
    async fn export(&self, session: &SessionHandle) -> Result<Transcript> {
        let chain = self.session(&session.session_id)?;
        let events = chain.log.lock().map(|log| log.clone()).unwrap_or_default();
        Ok(Transcript {
            events,
            native: None,
        })
    }

    // ponytail: GPT-5 family input window only; other models report None
    // until a model table exists.
    fn context_window(&self, model: &str) -> Option<u32> {
        model.starts_with("gpt-5").then_some(272_000)
    }
}

/// Normalizes `codex exec --json` output to [`SunEvent`]s.
#[derive(Debug, Default)]
pub struct CodexStream {
    /// The session's working directory; file paths are reported relative to it.
    cwd: Option<PathBuf>,
    /// Receives the thread id from `thread.started`.
    thread_id: Option<watch::Sender<Option<String>>>,
    /// Items whose `ToolCall` was already emitted (from `item.started`).
    started: HashSet<String>,
    /// The last agent message, the summary of `DoneClaimed`.
    last_text: String,
    /// The message of an `error` event, so the `turn.failed` repeating it is skipped.
    reported: Option<String>,
}

impl CodexStream {
    /// A normalizer for a session in `cwd` that reports the thread id to
    /// `thread_id`.
    pub fn new(cwd: Option<PathBuf>, thread_id: Option<watch::Sender<Option<String>>>) -> Self {
        Self {
            cwd,
            thread_id,
            ..Self::default()
        }
    }

    fn item(&mut self, line: &Value, out: &mut Vec<SunEvent>) {
        let item = &line["item"];
        let id = item["id"].as_str().unwrap_or_default().to_owned();
        let kind = item["type"].as_str().unwrap_or_default();
        if kind == "todo_list" {
            let todos = item["items"].as_array().map_or(&[][..], Vec::as_slice);
            out.push(SunEvent::Plan {
                todos: todos.iter().map(todo).collect(),
            });
            return;
        }
        let Some((name, args)) = tool(item) else {
            if kind == "agent_message" && line["type"] == "item.completed" {
                self.last_text = item["text"].as_str().unwrap_or_default().to_owned();
                out.push(SunEvent::Text {
                    role: Role::Assistant,
                    text: self.last_text.clone(),
                });
            }
            return; // reasoning, error items: not surfaced
        };
        if self.started.insert(id) {
            out.push(SunEvent::ToolCall {
                summary: truncate(&format!("{name} {}", summary(&args)), 120),
                args_hash: args_hash(&args),
                name,
            });
        }
        if line["type"] != "item.completed" {
            return;
        }
        let ok = item["status"] == "completed" && item["exit_code"].as_i64().is_none_or(|c| c == 0);
        let output = item["aggregated_output"].as_str().unwrap_or_default();
        out.push(SunEvent::ToolResult {
            ok,
            summary: truncate(output, 200),
        });
        if ok && kind == "file_change" {
            for change in item["changes"].as_array().map_or(&[][..], Vec::as_slice) {
                if let Some(path) = change["path"].as_str() {
                    out.push(SunEvent::FileChanged {
                        path: relative(self.cwd.as_deref(), path),
                    });
                }
            }
        }
    }

    /// A fatal error, from `error` or `turn.failed`; reported once.
    fn error(&mut self, message: &str, out: &mut Vec<SunEvent>) {
        if self.reported.as_deref() == Some(message) {
            return;
        }
        self.reported = Some(message.to_owned());
        out.push(
            limit::detect(message, Utc::now()).unwrap_or_else(|| SunEvent::Error {
                message: message.to_owned(),
            }),
        );
    }
}

impl Normalizer for CodexStream {
    fn line(&mut self, line: &Value, out: &mut Vec<SunEvent>) -> bool {
        match line["type"].as_str() {
            Some("thread.started") => {
                if let (Some(tx), Some(id)) = (&self.thread_id, line["thread_id"].as_str()) {
                    tx.send_replace(Some(id.to_owned()));
                }
            }
            Some("turn.started") => {
                self.last_text.clear();
                self.reported = None;
            }
            Some("item.started" | "item.updated" | "item.completed") => self.item(line, out),
            Some("error") => self.error(line["message"].as_str().unwrap_or("error"), out),
            Some("turn.failed") => {
                self.error(
                    line["error"]["message"].as_str().unwrap_or("turn failed"),
                    out,
                );
                return true;
            }
            Some("turn.completed") => {
                let usage = &line["usage"];
                // ponytail: per-turn totals; Codex reports no per-request
                // context size, so `context_used` stays unknown.
                out.push(SunEvent::Usage {
                    input: usage["input_tokens"].as_u64().unwrap_or(0),
                    output: usage["output_tokens"].as_u64().unwrap_or(0),
                    cost_usd: None,
                    context_used: None,
                });
                out.push(SunEvent::DoneClaimed {
                    summary: self.last_text.clone(),
                });
                return true;
            }
            _ => {}
        }
        false
    }
}

/// The tool name and arguments of a tool item, or `None` for other items.
fn tool(item: &Value) -> Option<(String, Value)> {
    Some(match item["type"].as_str()? {
        "command_execution" => ("shell".into(), json!({"command": item["command"]})),
        "file_change" => ("apply_patch".into(), json!({"changes": item["changes"]})),
        "mcp_tool_call" => (
            format!(
                "{}.{}",
                item["server"].as_str().unwrap_or("mcp"),
                item["tool"].as_str().unwrap_or("tool")
            ),
            item["arguments"].clone(),
        ),
        "web_search" => ("web_search".into(), json!({"query": item["query"]})),
        _ => return None,
    })
}

/// The main argument of a tool call, for its summary.
fn summary(args: &Value) -> String {
    if let Some(changes) = args["changes"].as_array() {
        return changes
            .iter()
            .filter_map(|c| c["path"].as_str())
            .collect::<Vec<_>>()
            .join(", ");
    }
    ["command", "query"]
        .iter()
        .find_map(|key| args[key].as_str())
        .map_or_else(|| args.to_string(), str::to_owned)
}

/// One `todo_list` item.
fn todo(item: &Value) -> Todo {
    Todo {
        text: item["text"].as_str().unwrap_or_default().to_owned(),
        status: if item["completed"] == true {
            TodoStatus::Done
        } else {
            TodoStatus::Pending
        },
    }
}

/// `codex debug models` (Codex 0.122+) → each listed model's reasoning
/// levels, in the order Codex lists them. Hidden models and bad tokens are
/// left out; a payload that doesn't parse gives none.
pub fn parse_debug_models(json: &[u8]) -> Vec<ModelEffort> {
    let Ok(catalog) = serde_json::from_slice::<Value>(json) else {
        return Vec::new();
    };
    let text = |v: &Value, key: &str| v.get(key).and_then(Value::as_str).map(str::to_owned);
    catalog["models"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|m| m["visibility"] != "hide")
        .filter_map(|m| {
            // The id goes to `--model`: a plain name, never an option.
            let id = m["slug"].as_str().filter(|id| {
                !id.is_empty()
                    && id.len() <= 128
                    && !id.starts_with('-')
                    && id
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || "._-:/".contains(c))
            })?;
            let id = id.to_owned();
            let effort_options = m["supported_reasoning_levels"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|l| crate::effort::level(l["effort"].as_str()?, text(l, "description")))
                .collect();
            Some(ModelEffort {
                label: text(m, "display_name").unwrap_or_else(|| id.clone()),
                id,
                effort_options,
                default_effort: text(m, "default_reasoning_level"),
            })
        })
        .collect()
}
