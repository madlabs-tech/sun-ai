//! Echo test mod: the smallest real mod, used by `tests/mod_host.rs`.
//!
//! Actions: `echo` (returns its input and idem key), `log` (prints its
//! `text` param to stderr), `fail` (mod error), `crash` (exits), `hang`
//! (never answers), `relay` (calls a plugin through the kernel). MCP tools
//! (`mcp_tools`/`mcp_call`): `lookup` (read-only, echoes its `word`) and
//! `post`. Triggers: emits one event per subscribed trigger, carrying its pid.
//! As an extension (mod API 1.2): runtime adapter `echo`, whose session
//! replays the contract case its envelope names (`normal`, `tool-error`,
//! `limit-hit`, `crash`), and exec env `echo`, which runs commands with
//! `IN_ECHO_ENV=<session>` set.

use std::collections::HashMap;
use std::time::Duration;

use serde_json::{Value, json};
use sun_mod_sdk::{CallParams, Emitter, Kernel, Mod, RpcError, SubscribeParams, code};

#[derive(Default)]
struct Echo {
    /// Runtime sessions' events, by session id.
    sessions: HashMap<String, Vec<Value>>,
}

/// The events of contract case `case`, as the `runtime.events` wire form.
fn case_events(case: &str) -> Vec<Value> {
    let text = |t: &str| json!({ "kind": "text", "role": "assistant", "text": t });
    let exited = |code: i32| json!({ "kind": "exited", "code": code });
    let done = json!({ "kind": "done_claimed", "summary": "echoed" });
    match case {
        "tool-error" => vec![
            json!({ "kind": "tool_call", "name": "Bash", "args_hash": 7, "summary": "false" }),
            json!({ "kind": "tool_result", "ok": false, "summary": "exit 1" }),
            done,
            exited(0),
        ],
        "limit-hit" => vec![
            json!({ "kind": "limit_hit", "limit": "quota",
                    "reset_at": "2030-01-01T00:00:00Z", "raw": "quota reached" }),
            exited(1),
        ],
        "crash" => vec![
            text("working"),
            json!({ "kind": "error", "message": "echo runtime died" }),
            exited(1),
        ],
        _ => vec![
            text("echo"),
            done,
            json!({ "kind": "usage", "input": 1, "output": 1 }),
            exited(0),
        ],
    }
}

impl Echo {
    /// `runtime.*` and `exec_env.*` (an extension's runtime adapter and exec env).
    fn serve(&mut self, action: &str, p: &Value) -> Result<Value, RpcError> {
        let session = p["session_id"].as_str().unwrap_or_default().to_owned();
        match action {
            "runtime.readiness" => Ok(json!({ "installed": true, "version": "echo 0.1",
                                             "logged_in": true, "detail": null })),
            "runtime.start" => {
                let spec = &p["spec"];
                let id = spec["session_id"].as_str().unwrap_or_default().to_owned();
                let mut events = case_events(spec["envelope"].as_str().unwrap_or_default());
                // What it was given: Sun sends no env (no secrets).
                let vars = spec["env"].as_object().map_or(0, serde_json::Map::len);
                events.insert(
                    0,
                    json!({ "kind": "text", "role": "system",
                                         "text": format!("env vars: {vars}") }),
                );
                self.sessions.insert(id.clone(), events);
                Ok(json!({ "native_id": format!("echo-{id}") }))
            }
            "runtime.events" => {
                let events = self.sessions.get(&session).cloned().unwrap_or_default();
                let after = p["after"].as_u64().unwrap_or(0) as usize;
                Ok(json!({ "events": events.get(after..).unwrap_or_default() }))
            }
            "runtime.send" | "runtime.stop" | "exec_env.teardown" => Ok(Value::Null),
            "exec_env.prepare" => Ok(json!({ "env": { "ECHO_ENV": p["kind"] } })),
            "exec_env.exec" => {
                let out =
                    std::process::Command::new("sh")
                        .arg("-c")
                        .arg(p["command"].as_str().unwrap_or_default())
                        .current_dir(p["cwd"].as_str().unwrap_or("."))
                        .envs(
                            p["env"].as_object().into_iter().flatten().map(|(k, v)| {
                                (k.clone(), v.as_str().unwrap_or_default().to_owned())
                            }),
                        )
                        .env("IN_ECHO_ENV", &session)
                        .output()
                        .map_err(|e| RpcError::new(code::FAILED, e.to_string()))?;
                let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
                text.push_str(&String::from_utf8_lossy(&out.stderr));
                Ok(json!({ "code": out.status.code(), "output": text, "timed_out": false }))
            }
            other => Err(RpcError::new(code::METHOD_NOT_FOUND, other)),
        }
    }
}

impl Mod for Echo {
    fn call(&mut self, call: CallParams) -> Result<Value, RpcError> {
        match call.action.as_str() {
            "echo" => Ok(json!({ "params": call.params, "idem_key": call.idem_key })),
            "log" => {
                eprintln!("{}", call.params["text"].as_str().unwrap_or_default());
                Ok(Value::Null)
            }
            "fail" => Err(RpcError::new(42, "boom")),
            "mcp_tools" => Ok(json!({ "tools": [
                { "name": "lookup", "description": "Looks a word up.", "read_only": true,
                  "input_schema": { "type": "object", "properties": { "word": { "type": "string" } } } },
                { "name": "post", "description": "Posts a message." },
            ] })),
            "mcp_call" => {
                let word = call.params["arguments"]["word"]
                    .as_str()
                    .unwrap_or_default();
                Ok(json!(format!(
                    "{}: {word}",
                    call.params["name"].as_str().unwrap_or_default()
                )))
            }
            "crash" => std::process::exit(3),
            "hang" => {
                std::thread::sleep(Duration::from_secs(3600));
                Ok(Value::Null)
            }
            other => self.serve(other, &call.params),
        }
    }

    /// `relay {plugin, action, params}`: calls a plugin through the kernel.
    fn call_with(&mut self, call: CallParams, kernel: &mut Kernel<'_>) -> Result<Value, RpcError> {
        if call.action != "relay" {
            return self.call(call);
        }
        let p = &call.params;
        let text = |key: &str| p[key].as_str().unwrap_or_default().to_owned();
        kernel.call_plugin(&text("plugin"), &text("action"), p["params"].clone())
    }

    fn subscribe(&mut self, params: SubscribeParams, emitter: &Emitter) -> Result<(), RpcError> {
        for topic in params.triggers {
            emitter.emit(&topic, json!({ "pid": std::process::id() }))?;
        }
        Ok(())
    }
}

fn main() -> std::io::Result<()> {
    sun_mod_sdk::run(Echo::default())
}
