//! JSON-RPC 2.0 frames and method params shared by the kernel and mods.
//! One message per line on stdio.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The mod API version this SDK speaks (semver). Manifests declare a range
/// (`api`) that must include it.
pub const API_VERSION: &str = "1.2.0";

/// Method names.
pub mod method {
    /// Kernel → mod request, first message after start. Params: [`InitParams`](super::InitParams).
    pub const INIT: &str = "init";
    /// Kernel → mod request. Params: [`CallParams`](super::CallParams).
    pub const CALL: &str = "call";
    /// Kernel → mod request. Params: [`SubscribeParams`](super::SubscribeParams).
    pub const SUBSCRIBE: &str = "subscribe";
    /// Mod → kernel notification (a fired trigger). Params: [`EventParams`](super::EventParams).
    pub const EVENT: &str = "event";
    /// Kernel → mod request. Result: any JSON, `{"ok": true}` by default.
    pub const HEALTH: &str = "health";
    /// Kernel → mod request. The mod replies, then exits.
    pub const SHUTDOWN: &str = "shutdown";
    /// Mod → kernel request (API 1.1, extensions only): calls an action of a
    /// plugin the extension declares in `permissions.plugins`.
    /// Params: [`PluginCallParams`](super::PluginCallParams). Result: the action's output.
    pub const PLUGIN_CALL: &str = "plugin_call";
}

/// A mod → kernel method the kernel still serves but will remove. It keeps
/// working for at least one minor release; each use logs a warning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Deprecation {
    /// The deprecated method.
    pub method: &'static str,
    /// The API version that deprecated it.
    pub since: &'static str,
    /// What to use instead.
    pub use_instead: &'static str,
}

/// The deprecated mod → kernel methods of this [`API_VERSION`] (none yet).
pub const DEPRECATED: &[Deprecation] = &[];

/// Error codes. Standard JSON-RPC ones plus [`UNAVAILABLE`](code::UNAVAILABLE).
pub mod code {
    /// Invalid JSON.
    pub const PARSE_ERROR: i64 = -32700;
    /// Not a valid request object.
    pub const INVALID_REQUEST: i64 = -32600;
    /// Unknown method or action.
    pub const METHOD_NOT_FOUND: i64 = -32601;
    /// Bad params.
    pub const INVALID_PARAMS: i64 = -32602;
    /// Bug in the mod.
    pub const INTERNAL_ERROR: i64 = -32603;
    /// Temporary failure (third party down, rate limited): the caller may retry.
    pub const UNAVAILABLE: i64 = -32000;
    /// The action ran and the third party refused it (4xx, unexpected
    /// response). Not retryable; the kernel maps it to `InvalidInput`, or
    /// to `Refused` with the status when it carries `data.http_status`
    /// ([`RpcError::with_http_status`](super::RpcError::with_http_status)).
    pub const FAILED: i64 = -32001;
}

/// One JSON-RPC 2.0 message: a request (`id` + `method`), a notification
/// (`method`, no `id`) or a response (`id` + `result` or `error`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Message {
    /// Always `"2.0"`.
    pub jsonrpc: String,
    /// Request id; absent on notifications.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<Value>,
    /// Method of a request or notification.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    /// Params of a request or notification.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
    /// Successful result of a response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    /// Error of a response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

impl Message {
    /// A request.
    pub fn request(id: u64, method: &str, params: Value) -> Self {
        Self {
            id: Some(id.into()),
            method: Some(method.into()),
            params: Some(params),
            ..Self::empty()
        }
    }

    /// A notification.
    pub fn notification(method: &str, params: Value) -> Self {
        Self {
            method: Some(method.into()),
            params: Some(params),
            ..Self::empty()
        }
    }

    /// A response to request `id`.
    pub fn response(id: Value, outcome: Result<Value, RpcError>) -> Self {
        let (result, error) = match outcome {
            Ok(v) => (Some(v), None),
            Err(e) => (None, Some(e)),
        };
        Self {
            id: Some(id),
            result,
            error,
            ..Self::empty()
        }
    }

    /// The outcome of a response. A `null` result parses as `None`, so a
    /// response without `error` is `Ok(result or null)`.
    pub fn into_outcome(self) -> Result<Value, RpcError> {
        match self.error {
            Some(e) => Err(e),
            None => Ok(self.result.unwrap_or(Value::Null)),
        }
    }

    fn empty() -> Self {
        Self {
            jsonrpc: "2.0".into(),
            ..Self::default()
        }
    }
}

/// A JSON-RPC error object.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RpcError {
    /// One of [`code`] or a mod-specific code.
    pub code: i64,
    /// Human-readable message. Never put secrets here.
    pub message: String,
    /// Optional details.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl RpcError {
    /// An error with `code` and `message`.
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    /// [`code::INVALID_PARAMS`]: bad input, rejected before any side effect.
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(code::INVALID_PARAMS, message)
    }

    /// [`code::FAILED`]: the third party refused the action.
    pub fn failed(message: impl Into<String>) -> Self {
        Self::new(code::FAILED, message)
    }

    /// [`code::UNAVAILABLE`]: temporary failure, worth retrying.
    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::new(code::UNAVAILABLE, message)
    }

    /// The same error carrying `data: {"http_status": status}`: the HTTP
    /// status a third party's API answered. The kernel reads it as data
    /// (e.g. renews a token once after a 401), never from the message.
    pub fn with_http_status(mut self, status: u16) -> Self {
        self.data = Some(serde_json::json!({ "http_status": status }));
        self
    }

    /// `data.http_status`, when the error came from a third party's HTTP API.
    pub fn http_status(&self) -> Option<u16> {
        let status = self.data.as_ref()?.get("http_status")?.as_u64()?;
        u16::try_from(status).ok()
    }
}

impl From<std::io::Error> for RpcError {
    fn from(e: std::io::Error) -> Self {
        Self::new(code::INTERNAL_ERROR, e.to_string())
    }
}

/// `init` params.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InitParams {
    /// The kernel's [`API_VERSION`].
    pub api_version: String,
    /// The installed mod name.
    pub mod_name: String,
    /// Mod settings (null until settings exist).
    #[serde(default)]
    pub config: Value,
}

/// `call` params.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CallParams {
    /// Action, step type or gate from the manifest's `provides`.
    pub action: String,
    /// Its input.
    #[serde(default)]
    pub params: Value,
    /// Set for side effects: the same key may arrive more than once and the
    /// mod must apply it at most once.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idem_key: Option<String>,
}

/// `plugin_call` params (mod → kernel).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PluginCallParams {
    /// An installed plugin the extension declares in `permissions.plugins`.
    pub plugin: String,
    /// One of the plugin's `provides.actions`.
    pub action: String,
    /// Its input.
    #[serde(default)]
    pub params: Value,
    /// As in [`CallParams::idem_key`]: set for side effects.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idem_key: Option<String>,
}

/// Reserved `call` actions of a plugin with `provides.mcp_tools` (API 1.1):
/// the kernel serves them to agent sessions as MCP tools, filtered by the
/// task's grant like any MCP server.
pub mod mcp_action {
    /// Params `null`; result [`McpTools`](super::McpTools).
    pub const TOOLS: &str = "mcp_tools";
    /// Params [`McpCallParams`](super::McpCallParams); result: the tool's
    /// output (a string goes to the agent as text, other JSON as its text).
    pub const CALL: &str = "mcp_call";
}

/// Reserved `call` actions of run hooks and metrics (API 1.2). An extension
/// gets them by declaring `provides.hooks` / `provides.metrics`; a plugin
/// gets the whole log by listing `observe` in `provides.actions`.
pub mod hook {
    /// Params `{events: [{hook?, seq, run_id, step_key, kind, payload, blob,
    /// at}]}` in `seq` order; an extension gets only its declared `hooks`,
    /// each tagged with `hook`. Result ignored.
    pub const OBSERVE: &str = "observe";
    /// Params `{run}` (the ended run: `id`, `task_id`, `workflow_id`,
    /// `workflow_version`, `status`, `end_reason`, `budget`). Result
    /// `{<metric name>: number}`; names not in `provides.metrics` and
    /// non-numbers are dropped.
    pub const METRICS: &str = "metrics";
    /// A task's first run was queued (`task_status` from `backlog`).
    pub const TASK_CREATED: &str = "task_created";
    /// A step began work (`step_status` to `running` from `queued`/`dispatched`).
    pub const STEP_STARTED: &str = "step_started";
    /// A step reached a terminal status.
    pub const STEP_FINISHED: &str = "step_finished";
    /// A gate passed or failed (`gate_result`).
    pub const GATE_RESULT: &str = "gate_result";
    /// A run reached a terminal status (`run_status`).
    pub const RUN_FINISHED: &str = "run_finished";
    /// Every event `provides.hooks` may name.
    pub const EVENTS: [&str; 5] = [
        TASK_CREATED,
        STEP_STARTED,
        STEP_FINISHED,
        GATE_RESULT,
        RUN_FINISHED,
    ];
}

/// Reserved `call` actions of an extension with `provides.runtime_adapters`
/// (API 1.2): Sun drives the mod as a runtime adapter. Every params object
/// carries `adapter`, one of the declared ids.
pub mod runtime_action {
    /// `{adapter, host_id}` → `Readiness`.
    pub const READINESS: &str = "runtime.readiness";
    /// `{adapter, spec}` (a `SessionSpec` without `env`) → `{native_id?}`.
    pub const START: &str = "runtime.start";
    /// `{adapter, session_id, text}` → `null`.
    pub const SEND: &str = "runtime.send";
    /// `{adapter, session_id, how}` (`graceful`|`kill`) → `null`; idempotent.
    pub const STOP: &str = "runtime.stop";
    /// `{adapter, session_id, after}` → `{events}`: the session's events
    /// from index `after` on, ending with `exited`.
    pub const EVENTS: &str = "runtime.events";
    /// Every runtime action.
    pub const ALL: [&str; 5] = [READINESS, START, SEND, STOP, EVENTS];
}

/// Reserved `call` actions of an extension with `provides.exec_envs`
/// (API 1.2): Sun runs a session's commands in the mod's environment. Every
/// params object carries `kind`, one of the declared kinds, and `session_id`.
pub mod exec_env_action {
    /// `{kind, session_id, worktree, env_names, config}` → `{env?}`: new
    /// session env vars (never `PATH` or one Sun set).
    pub const PREPARE: &str = "exec_env.prepare";
    /// `{kind, session_id, cwd, command, env, timeout_s}` →
    /// `{code, output, timed_out}`.
    pub const EXEC: &str = "exec_env.exec";
    /// `{kind, session_id}` → `null`; idempotent.
    pub const TEARDOWN: &str = "exec_env.teardown";
    /// Every exec env action.
    pub const ALL: [&str; 3] = [PREPARE, EXEC, TEARDOWN];
}

/// One MCP tool a plugin serves (`mcp_tools` result entry).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpToolSpec {
    /// Tool name, unique within the mod.
    pub name: String,
    /// What it does.
    #[serde(default)]
    pub description: String,
    /// JSON Schema of its arguments; `{"type": "object"}` when absent.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub input_schema: Value,
    /// It changes nothing. A grant without a tool list enables only these.
    #[serde(default)]
    pub read_only: bool,
}

/// `mcp_tools` result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpTools {
    /// Every tool the mod serves.
    pub tools: Vec<McpToolSpec>,
}

/// `mcp_call` params: which tool, with what arguments.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpCallParams {
    /// One of the `mcp_tools` names.
    pub name: String,
    /// The agent's arguments (a JSON object or `null`).
    #[serde(default)]
    pub arguments: Value,
}

/// `subscribe` params: the **full** set of triggers the kernel wants now
/// (each call replaces the previous set).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SubscribeParams {
    /// Names from the manifest's `provides.triggers`.
    pub triggers: Vec<String>,
}

/// `event` params.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventParams {
    /// The trigger that fired.
    pub topic: String,
    /// Its data.
    #[serde(default)]
    pub payload: Value,
}
