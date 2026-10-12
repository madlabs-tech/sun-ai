//! # sun-mod-sdk — build Sun mods in Rust
//!
//! **Responsibility:** the mod side of the **Plugin** protocol:
//! `mod.yaml` [`Manifest`] types and the JSON-RPC 2.0 stdio loop ([`run`]).
//! Apache-2.0; depends on nothing from the Sun engine.
//!
//! A mod is an executable. Implement [`Mod`] and hand it to [`run`]:
//!
//! ```no_run
//! use sun_mod_sdk::{CallParams, Mod, RpcError, code};
//! use serde_json::Value;
//!
//! struct Echo;
//! impl Mod for Echo {
//!     fn call(&mut self, call: CallParams) -> Result<Value, RpcError> {
//!         match call.action.as_str() {
//!             "echo" => Ok(call.params),
//!             other => Err(RpcError::new(code::METHOD_NOT_FOUND, other)),
//!         }
//!     }
//! }
//!
//! fn main() -> std::io::Result<()> {
//!     sun_mod_sdk::run(Echo)
//! }
//! ```
//!
//! stdout carries the protocol: log to stderr, never `println!`.
//! Requests are handled one at a time, in order. An extension reaches third
//! parties only through its declared plugins: override [`Mod::call_with`]
//! and use [`Kernel::call_plugin`].

#[cfg(feature = "http")]
pub mod http;
mod manifest;
mod protocol;

use std::collections::{HashSet, VecDeque};
use std::io::{self, BufRead, Write};
use std::sync::{Arc, Mutex};

use serde::de::DeserializeOwned;
use serde_json::{Value, json};

pub use manifest::{Entry, InvalidManifest, Manifest, ModKind, Permissions, Provides};
pub use protocol::{
    API_VERSION, CallParams, DEPRECATED, Deprecation, EventParams, InitParams, McpCallParams,
    McpToolSpec, McpTools, Message, PluginCallParams, RpcError, SubscribeParams, code,
    exec_env_action, hook, mcp_action, method, runtime_action,
};

/// A mod's handlers. Only [`call`](Self::call) is required.
pub trait Mod {
    /// First request after start. Fail it to refuse to run.
    fn init(&mut self, _params: InitParams) -> Result<(), RpcError> {
        Ok(())
    }

    /// Runs an action, step type or gate. Use [`code::UNAVAILABLE`] for
    /// failures worth retrying.
    fn call(&mut self, call: CallParams) -> Result<Value, RpcError>;

    /// [`call`](Self::call) with a handle to call the kernel back, e.g. an
    /// extension calling a plugin it declares. Defaults to `call`.
    fn call_with(&mut self, call: CallParams, _kernel: &mut Kernel<'_>) -> Result<Value, RpcError> {
        self.call(call)
    }

    /// Replaces the set of triggers to emit. Keep `emitter` (it is `Clone +
    /// Send`) and call [`Emitter::emit`] from any thread when one fires.
    fn subscribe(&mut self, _params: SubscribeParams, _emitter: &Emitter) -> Result<(), RpcError> {
        Err(RpcError::new(
            code::METHOD_NOT_FOUND,
            "this mod has no triggers",
        ))
    }

    /// Liveness probe.
    fn health(&mut self) -> Result<Value, RpcError> {
        Ok(json!({ "ok": true }))
    }
}

/// Writes messages to the kernel; safe to share across threads.
#[derive(Clone)]
pub struct Emitter(Arc<Mutex<Box<dyn Write + Send>>>);

impl Emitter {
    /// Sends an `event` notification for trigger `topic`.
    pub fn emit(&self, topic: &str, payload: Value) -> io::Result<()> {
        let params = json!({ "topic": topic, "payload": payload });
        self.send(&Message::notification(method::EVENT, params))
    }

    fn send(&self, msg: &Message) -> io::Result<()> {
        let mut line = serde_json::to_vec(msg)?;
        line.push(b'\n');
        let mut out = self
            .0
            .lock()
            .map_err(|_| io::Error::other("emitter lock poisoned"))?;
        out.write_all(&line)?;
        out.flush()
    }
}

/// Calls back into the kernel while a `call` is handled (API 1.1).
pub struct Kernel<'a> {
    lines: &'a mut dyn Iterator<Item = io::Result<String>>,
    /// Kernel requests that arrived while waiting for an answer.
    backlog: &'a mut VecDeque<String>,
    emitter: &'a Emitter,
    next_id: &'a mut u64,
}

impl Kernel<'_> {
    /// Runs `action` of `plugin`, which this extension must declare in
    /// `permissions.plugins`; the kernel refuses any other.
    pub fn call_plugin(
        &mut self,
        plugin: &str,
        action: &str,
        params: Value,
    ) -> Result<Value, RpcError> {
        *self.next_id += 1;
        let id = *self.next_id;
        let params = json!({ "plugin": plugin, "action": action, "params": params });
        self.emitter
            .send(&Message::request(id, method::PLUGIN_CALL, params))?;
        loop {
            let line = self
                .lines
                .next()
                .ok_or_else(|| RpcError::unavailable("the kernel closed stdin"))??;
            match serde_json::from_str::<Message>(&line) {
                Ok(msg) if msg.method.is_none() && msg.id == Some(json!(id)) => {
                    return msg.into_outcome();
                }
                Ok(msg) if msg.method.is_none() => {} // a stale answer
                _ => self.backlog.push_back(line),
            }
        }
    }
}

/// Subscribe bookkeeping: the trigger set the kernel wants now and the
/// [`Emitter`] to fire them on. Call [`set`](Self::set) from
/// [`Mod::subscribe`].
#[derive(Default)]
pub struct Triggers {
    wanted: HashSet<String>,
    emitter: Option<Emitter>,
}

impl Triggers {
    /// Replaces the set (each `subscribe` carries the full set).
    pub fn set(&mut self, params: SubscribeParams, emitter: &Emitter) {
        self.wanted = params.triggers.into_iter().collect();
        self.emitter = Some(emitter.clone());
    }

    /// Emits `topic` if it is subscribed. Returns whether it was sent.
    pub fn emit(&self, topic: &str, payload: Value) -> io::Result<bool> {
        match self
            .emitter
            .as_ref()
            .filter(|_| self.wanted.contains(topic))
        {
            Some(emitter) => emitter.emit(topic, payload).map(|()| true),
            None => Ok(false),
        }
    }
}

/// Serves `module` on stdin/stdout until `shutdown` or EOF.
pub fn run(module: impl Mod) -> io::Result<()> {
    serve(module, io::stdin().lock(), io::stdout())
}

/// Serves `module` on any line reader/writer pair (for tests).
pub fn serve(
    mut module: impl Mod,
    input: impl BufRead,
    output: impl Write + Send + 'static,
) -> io::Result<()> {
    let emitter = Emitter(Arc::new(Mutex::new(Box::new(output))));
    let mut lines = input.lines();
    let mut backlog = VecDeque::new();
    let mut next_id = 0;
    loop {
        let line = match backlog.pop_front() {
            Some(line) => line,
            None => match lines.next() {
                Some(line) => line?,
                None => break,
            },
        };
        if line.trim().is_empty() {
            continue;
        }
        let msg: Message = match serde_json::from_str(&line) {
            Ok(msg) => msg,
            Err(e) => {
                let err = RpcError::new(code::PARSE_ERROR, e.to_string());
                emitter.send(&Message::response(Value::Null, Err(err)))?;
                continue;
            }
        };
        // Responses are never addressed to a mod in API 1.0: ignore them.
        let Some(name) = msg.method else { continue };
        let params = msg.params.unwrap_or(Value::Null);
        let outcome = match name.as_str() {
            method::INIT => parse(params)
                .and_then(|p| module.init(p))
                .map(|()| Value::Null),
            method::CALL => parse(params).and_then(|p| {
                let mut kernel = Kernel {
                    lines: &mut lines,
                    backlog: &mut backlog,
                    emitter: &emitter,
                    next_id: &mut next_id,
                };
                module.call_with(p, &mut kernel)
            }),
            method::SUBSCRIBE => parse(params)
                .and_then(|p| module.subscribe(p, &emitter))
                .map(|()| Value::Null),
            method::HEALTH => module.health(),
            method::SHUTDOWN => {
                if let Some(id) = msg.id {
                    emitter.send(&Message::response(id, Ok(Value::Null)))?;
                }
                return Ok(());
            }
            other => Err(RpcError::new(
                code::METHOD_NOT_FOUND,
                format!("unknown method {other}"),
            )),
        };
        if let Some(id) = msg.id {
            emitter.send(&Message::response(id, outcome))?;
        }
    }
    Ok(())
}

/// For tests: serves newline-separated `input` and returns every message the
/// mod wrote (responses and events), in order.
pub fn serve_lines(module: impl Mod, input: &str) -> io::Result<Vec<Message>> {
    let buffer = Buffer::default();
    serve(module, input.as_bytes(), buffer.clone())?;
    let out = buffer
        .0
        .lock()
        .map_err(|_| io::Error::other("buffer lock poisoned"))?;
    out.split(|b| *b == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).map_err(io::Error::from))
        .collect()
}

#[derive(Clone, Default)]
struct Buffer(Arc<Mutex<Vec<u8>>>);

impl Write for Buffer {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .map_err(|_| io::Error::other("buffer lock poisoned"))?
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Deserializes params; a mismatch is [`code::INVALID_PARAMS`].
pub fn parse<T: DeserializeOwned>(params: Value) -> Result<T, RpcError> {
    serde_json::from_value(params).map_err(|e| RpcError::invalid(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Echo;
    impl Mod for Echo {
        fn call(&mut self, call: CallParams) -> Result<Value, RpcError> {
            Ok(call.params)
        }
        fn subscribe(&mut self, p: SubscribeParams, emitter: &Emitter) -> Result<(), RpcError> {
            for t in p.triggers {
                emitter.emit(&t, json!(1))?;
            }
            Ok(())
        }
    }

    #[test]
    fn serve_answers_requests_emits_events_and_stops_on_shutdown() {
        let input = [
            r#"{"jsonrpc":"2.0","id":1,"method":"init","params":{"api_version":"1.0.0","mod_name":"echo"}}"#,
            r#"{"jsonrpc":"2.0","id":2,"method":"call","params":{"action":"echo","params":{"a":1}}}"#,
            r#"{"jsonrpc":"2.0","id":3,"method":"subscribe","params":{"triggers":["tick"]}}"#,
            r#"{"jsonrpc":"2.0","id":4,"method":"nope"}"#,
            "not json",
            r#"{"jsonrpc":"2.0","id":5,"method":"shutdown"}"#,
            r#"{"jsonrpc":"2.0","id":6,"method":"health"}"#,
        ]
        .join("\n");
        let msgs = serve_lines(Echo, &input).unwrap();
        assert_eq!(msgs.len(), 7, "{msgs:?}");
        assert_eq!(msgs[0].id, Some(json!(1)));
        assert_eq!(msgs[0].clone().into_outcome(), Ok(Value::Null));
        assert_eq!(msgs[1].result, Some(json!({"a": 1})));
        assert_eq!(msgs[2].method.as_deref(), Some("event"));
        assert_eq!(msgs[2].params, Some(json!({"topic": "tick", "payload": 1})));
        assert_eq!(msgs[3].id, Some(json!(3)));
        assert_eq!(msgs[4].error.as_ref().unwrap().code, code::METHOD_NOT_FOUND);
        assert_eq!(msgs[5].error.as_ref().unwrap().code, code::PARSE_ERROR);
        assert_eq!(msgs[6].id, Some(json!(5)));
        assert_eq!(msgs[6].clone().into_outcome(), Ok(Value::Null));
    }

    fn manifest(kind: ModKind) -> Manifest {
        Manifest {
            name: "github".into(),
            kind,
            version: "0.3.1".into(),
            api: ">=1.0 <2.0".into(),
            entry: Entry {
                command: "./github-mod".into(),
                args: vec![],
            },
            provides: Provides::default(),
            permissions: Permissions::default(),
        }
    }

    #[test]
    fn kind_rules_reject_the_other_kinds_api() {
        let mut plugin = manifest(ModKind::Plugin);
        plugin.permissions.network = vec!["api.github.com".into()];
        plugin.provides.actions = vec!["open_pr".into()];
        assert_eq!(plugin.validate(), Ok(()));
        assert!(plugin.is_callable("open_pr") && !plugin.is_callable("x"));
        plugin.permissions.devices = vec!["kvm".into()];
        assert!(plugin.validate().unwrap_err().0.contains("devices"));

        let mut ext = manifest(ModKind::Extension);
        ext.permissions.devices = vec!["kvm".into()];
        assert_eq!(ext.validate(), Ok(()));
        ext.permissions.secrets = vec!["TOKEN".into()];
        assert!(ext.validate().unwrap_err().0.contains("secrets"));
        ext.permissions.secrets.clear();
        ext.provides.triggers = vec!["t".into()];
        assert!(ext.validate().is_err());

        let mut bad_name = manifest(ModKind::Plugin);
        bad_name.name = "GitHub".into();
        assert!(bad_name.validate().is_err());
    }

    #[test]
    fn extension_may_declare_runtime_adapters_and_exec_envs() {
        let mut ext = manifest(ModKind::Extension);
        ext.provides.runtime_adapters = vec!["gemini".into()];
        ext.provides.exec_envs = vec!["firecracker".into()];
        assert_eq!(ext.validate(), Ok(()));
        assert!(ext.is_callable(runtime_action::START));
        assert!(ext.is_callable(exec_env_action::EXEC));
        ext.provides.exec_envs = vec!["Fire cracker".into()];
        assert!(ext.validate().unwrap_err().0.contains("Fire cracker"));

        let mut plugin = manifest(ModKind::Plugin);
        assert!(!plugin.is_callable(runtime_action::START));
        plugin.provides.runtime_adapters = vec!["gemini".into()];
        let err = plugin.validate().unwrap_err().0;
        assert!(
            err.contains("runtime_adapters") && err.contains("extension-only"),
            "{err}"
        );
    }

    #[test]
    fn extension_may_declare_hooks_and_metrics() {
        let mut ext = manifest(ModKind::Extension);
        ext.provides.hooks = vec![hook::RUN_FINISHED.into(), hook::GATE_RESULT.into()];
        ext.provides.metrics = vec!["lines_changed".into()];
        assert_eq!(ext.validate(), Ok(()));
        assert!(ext.is_callable(hook::OBSERVE) && ext.is_callable(hook::METRICS));

        let mut unknown = ext.clone();
        unknown.provides.hooks.push("pr_opened".into());
        assert!(unknown.validate().unwrap_err().0.contains("pr_opened"));
        let mut bad_metric = ext.clone();
        bad_metric.provides.metrics = vec!["Lines Changed".into()];
        assert!(bad_metric.validate().is_err());

        // A plugin observes through its `observe` action, not `hooks`.
        let mut plugin = manifest(ModKind::Plugin);
        plugin.provides.hooks = ext.provides.hooks.clone();
        assert!(plugin.validate().unwrap_err().0.contains("extension-only"));
        plugin.provides.hooks.clear();
        plugin.provides.metrics = vec!["x".into()];
        assert!(plugin.validate().is_err());
        assert!(!manifest(ModKind::Extension).is_callable(hook::METRICS));
    }
}
