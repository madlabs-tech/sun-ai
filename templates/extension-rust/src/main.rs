//! my-extension: a Sun extension template. Build with `cargo build --release`,
//! then run `sun mod dev .` (hot reload).
//!
//! Log to stderr; stdout carries the protocol.

use serde_json::{Value, json};
use sun_mod_sdk::{CallParams, Mod, RpcError, code, hook};

#[derive(Default)]
struct MyExtension {
    /// Runs finished since start, counted by the `run_finished` hook.
    runs_seen: u64,
}

impl Mod for MyExtension {
    fn call(&mut self, call: CallParams) -> Result<Value, RpcError> {
        let text = call.params["text"].as_str().unwrap_or_default();
        let words = text.split_whitespace().count();
        match call.action.as_str() {
            // A step type: returns the step's output.
            "word_count" => Ok(json!({ "words": words })),
            // A gate: returns pass/fail with a reason.
            "max_words" => {
                let max = call.params["max"].as_u64().unwrap_or(100) as usize;
                Ok(json!({ "pass": words <= max, "reason": format!("{words} words, max {max}") }))
            }
            // Run hooks: the declared events, each tagged with its `hook`.
            // A batch may arrive twice after a crash; dedupe by `seq` if it matters.
            hook::OBSERVE => {
                let events = call.params["events"].as_array().cloned().unwrap_or_default();
                let ended = events.iter().filter(|e| e["hook"] == hook::RUN_FINISHED);
                self.runs_seen += ended.count() as u64;
                Ok(Value::Null)
            }
            // Metrics: one number per declared name, journalled on the run's timeline.
            hook::METRICS => Ok(json!({ "runs_seen": self.runs_seen })),
            other => Err(RpcError::new(code::METHOD_NOT_FOUND, other)),
        }
    }
}

fn main() -> std::io::Result<()> {
    sun_mod_sdk::run(MyExtension::default())
}
