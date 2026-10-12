//! Test mod for `tests/serve.rs`: a `github`-shaped plugin whose every
//! action succeeds without a network, so the default workflow's
//! `github.open_pr` step can finish — if its params follow mod-github's
//! contract. `reconcile` reports nothing delivered;
//! `config` returns the `init.config` it got.

use serde_json::{Value, json};
use sun_mod_sdk::{CallParams, InitParams, Mod, RpcError};

#[derive(Default)]
struct Fake {
    config: Value,
}

impl Mod for Fake {
    fn init(&mut self, params: InitParams) -> Result<(), RpcError> {
        self.config = params.config;
        Ok(())
    }

    fn call(&mut self, call: CallParams) -> Result<Value, RpcError> {
        Ok(match call.action.as_str() {
            "reconcile" => json!({ "delivered": false }),
            "config" => self.config.clone(),
            // mod-github's contract: the engine fills it from the run.
            "open_pr" => {
                let p = &call.params;
                let text = |k: &str| p[k].as_str().is_some_and(|s| !s.is_empty());
                let head = p["head"].as_str().unwrap_or_default();
                if !(["repo", "base", "title", "body"].iter().all(|k| text(k))
                    && head.starts_with("sun/"))
                {
                    return Err(RpcError::invalid(format!("open_pr params: {p}")));
                }
                json!({ "number": 1, "url": "https://example.invalid/pr/1", "created": true })
            }
            _ => json!({ "number": 1, "url": "https://example.invalid/pr/1", "created": true }),
        })
    }
}

fn main() -> std::io::Result<()> {
    sun_mod_sdk::run(Fake::default())
}
