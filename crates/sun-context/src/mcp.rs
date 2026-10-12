//! The `sun` MCP server: the tools agents use to pull context and
//! report back. It reaches the engine only through [`EngineHandle`].

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock};
use rmcp::{ServerHandler, schemars, tool, tool_handler, tool_router};
use serde::Deserialize;
use sun_core::host_protocol::{DeviceFlowRequest, DeviceInput, DevicePlatform, base64};
use sun_core::model::NoteKind;
use sun_core::redact::redact;
use sun_core::{EngineHandle, Error, RunId, StepKey};
use sun_store::Store;

/// Longest accepted pointer id.
const MAX_POINTER: usize = 512;
/// Longest accepted note body, done summary or question.
const MAX_TEXT: usize = 16 * 1024;
/// Most answer options for `ask`, and the longest one.
const MAX_OPTIONS: usize = 10;
const MAX_OPTION: usize = 200;
/// Longest accepted search query.
const MAX_QUERY: usize = 1024;
/// Hits returned by `search` by default, and at most.
const DEFAULT_HITS: usize = 5;
const MAX_HITS: usize = 20;

/// One session's `sun` MCP server, scoped to one run and step: every tool call
/// acts on that run only. Serve it on any rmcp transport, e.g.
/// `server.serve(stream).await`.
#[derive(Clone)]
pub struct SunMcp {
    engine: Arc<dyn EngineHandle>,
    run_id: RunId,
    step_key: StepKey,
    knowledge: Option<Store>,
    tool_router: ToolRouter<Self>,
}

/// `get` arguments.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct GetArgs {
    /// A pointer from the context pack, e.g. `blob:…`, `artifact:…`, `memory:…`.
    pub id: String,
}

/// `note` arguments.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct NoteArgs {
    /// One of `decision`, `todo`, `learning`, `question`, `progress`.
    pub kind: String,
    /// The note.
    pub body: String,
}

/// `ask` arguments.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct AskArgs {
    /// The question for a human.
    pub question: String,
    /// Allowed answers; leave empty for a free-text answer.
    #[serde(default)]
    pub options: Vec<String>,
}

/// `search` arguments.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct SearchArgs {
    /// What to look for, in words (English or Bahasa Indonesia).
    pub query: String,
    /// How many snippets to return (default 5, at most 20).
    pub k: Option<usize>,
}

/// `done` arguments.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct DoneArgs {
    /// What was done, for the reviewer.
    pub summary: String,
}

/// `device_tap` arguments.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct TapArgs {
    /// Pixels from the left of the `device_screen` image.
    pub x: u32,
    /// Pixels from the top of the `device_screen` image.
    pub y: u32,
}

/// `device_flow` arguments.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct FlowArgs {
    /// A Maestro flow as YAML, e.g. `appId: com.example\n---\n- launchApp\n- tapOn: Login`.
    pub flow: String,
    /// `ios` or `android`: boots a simulator or emulator for this task when
    /// none is up. Leave empty to use the one that is up.
    pub platform: Option<String>,
    /// Simulator or AVD to boot, e.g. `iPhone 15`; default the host's first.
    pub device: Option<String>,
    /// Built app to install first, relative to the worktree (`.app` or `.apk`).
    pub app: Option<String>,
    /// Seconds for boot, install and flow (default 600, at most 1800).
    pub timeout_s: Option<u64>,
}

/// Longest accepted Maestro flow, device name and app path.
const MAX_FLOW: usize = 64 * 1024;
const MAX_NAME: usize = 512;
/// Default and longest `device_flow` time.
const DEFAULT_FLOW_S: u64 = 600;
const MAX_FLOW_S: u64 = 1800;

type ToolResult = Result<String, String>;

#[tool_router]
impl SunMcp {
    /// A server for `step_key` of `run_id`.
    pub fn new(engine: Arc<dyn EngineHandle>, run_id: RunId, step_key: StepKey) -> Self {
        Self {
            engine,
            run_id,
            step_key,
            knowledge: None,
            tool_router: Self::tool_router(),
        }
    }

    /// Enables `search` over the knowledge index in `store`.
    pub fn with_knowledge(mut self, store: Store) -> Self {
        self.knowledge = Some(store);
        self
    }

    #[tool(description = "The task, step goal, constraints, gates and acceptance criteria.")]
    async fn task(&self) -> ToolResult {
        let brief = self
            .engine
            .task(self.run_id, self.step_key.clone())
            .await
            .map_err(message)?;
        serde_json::to_string_pretty(&brief).map_err(|e| e.to_string())
    }

    #[tool(
        description = "Fetch a pointer from the context pack: a doc, an earlier artifact \
                          or a transcript chunk."
    )]
    async fn get(&self, Parameters(args): Parameters<GetArgs>) -> ToolResult {
        let id = text(&args.id, "id", MAX_POINTER)?;
        self.engine.get(self.run_id, id).await.map_err(message)
    }

    #[tool(
        description = "Search the company knowledge (docs, PRDs, sheets). Returns snippets \
                          with citations; cite them in your output and fetch a whole doc \
                          with `get`."
    )]
    async fn search(&self, Parameters(args): Parameters<SearchArgs>) -> ToolResult {
        let query = text(&args.query, "query", MAX_QUERY)?;
        let k = args.k.unwrap_or(DEFAULT_HITS).clamp(1, MAX_HITS);
        // The engine scopes to the task's sources and adds vectors (hybrid).
        match self.engine.search(self.run_id, query.clone(), k).await {
            Err(Error::Unavailable(_)) => {}
            other => return other.map_err(message),
        }
        let store = self
            .knowledge
            .as_ref()
            .ok_or("knowledge search is not enabled")?;
        let hits = crate::knowledge::search(store, &query, crate::knowledge::Scope::default(), k)
            .await
            .map_err(|e| message(e.into()))?;
        Ok(render_hits(&query, &hits))
    }

    #[tool(
        description = "Record a decision, todo, learning, question or progress note. \
                          Notes survive a switch to another runtime."
    )]
    async fn note(&self, Parameters(args): Parameters<NoteArgs>) -> ToolResult {
        let kind: NoteKind = args.kind.parse().map_err(message)?;
        let body = text(&args.body, "body", MAX_TEXT)?;
        let id = self
            .engine
            .add_note(self.run_id, self.step_key.clone(), kind, body)
            .await
            .map_err(message)?;
        Ok(format!("noted {id}"))
    }

    #[tool(
        description = "Notes of this run, including parallel agents' decisions and \
                          questions."
    )]
    async fn notes(&self) -> ToolResult {
        let notes = self.engine.notes(self.run_id).await.map_err(message)?;
        serde_json::to_string_pretty(&notes).map_err(|e| e.to_string())
    }

    #[tool(description = "Ask a human a question and wait for the answer.")]
    async fn ask(&self, Parameters(args): Parameters<AskArgs>) -> ToolResult {
        let question = text(&args.question, "question", MAX_TEXT)?;
        if args.options.len() > MAX_OPTIONS {
            return Err(format!("at most {MAX_OPTIONS} options"));
        }
        let options = args
            .options
            .iter()
            .map(|o| text(o, "option", MAX_OPTION))
            .collect::<Result<Vec<_>, _>>()?;
        self.engine
            .ask(self.run_id, self.step_key.clone(), question, options)
            .await
            .map_err(message)
    }

    #[tool(
        description = "Claim the step is done and request verification. Gates decide \
                          whether it passes."
    )]
    async fn done(&self, Parameters(args): Parameters<DoneArgs>) -> ToolResult {
        let summary = text(&args.summary, "summary", MAX_TEXT)?;
        self.engine
            .done(self.run_id, self.step_key.clone(), summary)
            .await
            .map_err(message)?;
        Ok("verification requested".into())
    }

    #[tool(
        description = "A screenshot of this task's simulator or emulator. Tap coordinates \
                          are pixels of this image. Boot one with `device_flow` first."
    )]
    async fn device_screen(&self) -> Result<CallToolResult, String> {
        let screen = self
            .engine
            .device_screen(self.run_id)
            .await
            .map_err(message)?;
        Ok(CallToolResult::success(vec![
            ContentBlock::text(format!(
                "{} ({}), takes taps: {}",
                screen.device,
                serde_json::to_value(screen.platform).map_err(|e| e.to_string())?,
                screen.input
            )),
            ContentBlock::image(base64(&screen.image), screen.mime),
        ]))
    }

    #[tool(description = "Tap this task's simulator or emulator at a pixel of `device_screen`.")]
    async fn device_tap(&self, Parameters(args): Parameters<TapArgs>) -> ToolResult {
        let input = DeviceInput::Tap {
            x: args.x,
            y: args.y,
        };
        self.engine
            .device_input(self.run_id, self.step_key.clone(), input)
            .await
            .map_err(message)?;
        Ok(format!("tapped {} {}", args.x, args.y))
    }

    #[tool(
        description = "Run a Maestro flow (YAML) on this task's simulator or emulator, \
                          booting one first when `platform` is given and none is up. It \
                          stays up until your step ends. Returns pass/fail and Maestro's output."
    )]
    async fn device_flow(&self, Parameters(args): Parameters<FlowArgs>) -> ToolResult {
        if args.flow.trim().is_empty() || args.flow.len() > MAX_FLOW {
            return Err(format!("flow must be 1–{MAX_FLOW} bytes of Maestro YAML"));
        }
        let platform = match args.platform.as_deref().map(str::trim) {
            None | Some("") => None,
            Some(p) => Some(
                serde_json::from_value::<DevicePlatform>(serde_json::Value::from(p))
                    .map_err(|_| format!("platform must be `ios` or `android`, not {p:?}"))?,
            ),
        };
        let optional = |value: Option<String>, field| {
            value
                .filter(|v| !v.trim().is_empty())
                .map(|v| text(&v, field, MAX_NAME))
                .transpose()
        };
        let request = DeviceFlowRequest {
            cwd: Default::default(), // the engine resolves the task's worktree
            platform,
            device: optional(args.device, "device")?,
            app: optional(args.app, "app")?.map(Into::into),
            flow: args.flow,
            timeout_s: args
                .timeout_s
                .unwrap_or(DEFAULT_FLOW_S)
                .clamp(1, MAX_FLOW_S),
        };
        let result = self
            .engine
            .device_flow(self.run_id, self.step_key.clone(), request)
            .await
            .map_err(message)?;
        let verdict = if result.passed { "passed" } else { "failed" };
        Ok(format!(
            "flow {verdict} on {}\n\n{}",
            result.device, result.output
        ))
    }
}

#[tool_handler(
    router = self.tool_router,
    name = "sun",
    instructions = "Sun's tools for this task: `task` for the brief, `get` to fetch pointers \
                    from .sun/context/pack.md, `note` to record decisions and todos, `notes` \
                    to read the run's notes, `search` for company docs, `ask` to ask a human, `done` when finished. \
                    Mobile apps: `device_flow` boots this task's simulator or emulator and runs a Maestro flow, \
                    `device_screen` shows it, `device_tap` taps it."
)]
impl ServerHandler for SunMcp {}

impl SunMcp {
    /// Serves this session on `io` (newline-delimited JSON-RPC, the stdio
    /// transport) until the client hangs up.
    pub async fn serve_on<T>(self, io: T) -> Result<(), Error>
    where
        T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin + 'static,
    {
        let failed = |e: String| Error::Unavailable(format!("sun mcp: {e}"));
        let running = rmcp::ServiceExt::serve(self, io)
            .await
            .map_err(|e| failed(e.to_string()))?;
        running
            .waiting()
            .await
            .map(drop)
            .map_err(|e| failed(e.to_string()))
    }
}

/// Validates agent text at the trust boundary and redacts secrets from it.
fn text(value: &str, field: &str, max: usize) -> Result<String, String> {
    let value = value.trim();
    if value.is_empty() {
        return Err(format!("{field} is empty"));
    }
    if value.len() > max {
        return Err(format!("{field} is longer than {max} bytes"));
    }
    Ok(redact(value, &[]))
}

fn message(error: Error) -> String {
    error.to_string()
}

/// `sun.search`'s answer: each hit as `## citation`, pointer, text.
pub fn render_hits(query: &str, hits: &[crate::KnowledgeHit]) -> String {
    if hits.is_empty() {
        return format!("no matches for {query:?}");
    }
    hits.iter()
        .map(|h| format!("## {}\n{}\n\n{}\n", h.citation, h.pointer, h.text))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Issues per-session tokens; a token opens a [`SunMcp`] that only sees its
/// own run and step. The stdio shim presents the token.
///
/// It holds no engine: the engine issues tokens, and the kernel transport
/// opens them with the engine handle.
#[derive(Default)]
pub struct McpSessions {
    knowledge: Option<Store>,
    scopes: Mutex<HashMap<String, (RunId, StepKey)>>,
}

impl McpSessions {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Servers opened from now on can `search` the knowledge index in `store`.
    pub fn with_knowledge(mut self, store: Store) -> Self {
        self.knowledge = Some(store);
        self
    }

    /// A new token for `step_key` of `run_id`.
    pub fn issue(&self, run_id: RunId, step_key: StepKey) -> String {
        // Two ULIDs: 160 random bits from `rand`'s thread-local CSPRNG.
        let token = format!("{}{}", ulid::Ulid::generate(), ulid::Ulid::generate());
        self.lock().insert(token.clone(), (run_id, step_key));
        token
    }

    /// Revokes a token (when its session ends). Unknown tokens are ignored.
    pub fn revoke(&self, token: &str) {
        self.lock().remove(token);
    }

    /// The run and step `token` was issued for, while it is live.
    pub fn scope(&self, token: &str) -> Option<(RunId, StepKey)> {
        self.lock().get(token).cloned()
    }

    /// The server for `token`, acting through `engine`, or `NotFound`.
    pub fn open(&self, token: &str, engine: Arc<dyn EngineHandle>) -> Result<SunMcp, Error> {
        let (run_id, step_key) = self
            .lock()
            .get(token)
            .cloned()
            .ok_or_else(|| Error::NotFound("mcp session token".into()))?;
        let server = SunMcp::new(engine, run_id, step_key);
        Ok(match &self.knowledge {
            Some(store) => server.with_knowledge(store.clone()),
            None => server,
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, (RunId, StepKey)>> {
        // A poisoned map is still a valid map; recover it.
        self.scopes.lock().unwrap_or_else(|e| e.into_inner())
    }
}
