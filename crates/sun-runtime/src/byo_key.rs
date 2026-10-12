//! The `byo_key` runtime source: a provider API (Anthropic,
//! Google, or an OpenAI-compatible endpoint such as OpenAI, OpenRouter or a
//! local Ollama at `http://localhost:11434/v1`), a model and a reference to
//! the key in the secret store.
//!
//! [`ByoKey`] and its validation live in sun-core (the store and API check
//! it too); how each CLI takes the key stays here.
//!
//! The key value never lands in a file. The caller reads the secret named by
//! [`ByoKey::key_secret`] and puts its value into `SessionSpec::env` under
//! [`KEY_ENV`]. [`opencode_config`] only references it as
//! `{env:SUN_BYO_KEY}`, which OpenCode resolves when it loads the config;
//! [`native_env`] copies it into Claude Code's or Codex's own provider env
//! vars. Being in `spec.env`, the value is also redacted from every event.

use serde_json::{Value, json};
pub use sun_core::ByoKey;
use sun_core::{ByoKeyKind, Error, Result, SessionSpec};

/// The env var that carries the key value into the session.
pub const KEY_ENV: &str = sun_core::BYO_KEY_ENV;

/// OpenCode provider id of the rendered endpoint; models are `sun/<model>`.
pub const PROVIDER: &str = "sun";

/// OpenCode config (`OPENCODE_CONFIG_CONTENT`) that adds the endpoint as
/// provider [`PROVIDER`] and selects `model` (default [`ByoKey::model`]).
pub fn opencode_config(byo: &ByoKey, model: Option<&str>) -> Value {
    let model = model.unwrap_or(&byo.model);
    let mut options = json!({});
    if !byo.endpoint.trim().is_empty() {
        options["baseURL"] = byo.endpoint.trim().into();
    }
    if byo.key_secret.is_some() {
        options["apiKey"] = format!("{{env:{KEY_ENV}}}").into();
    }
    let npm = match byo.kind {
        ByoKeyKind::Anthropic => "@ai-sdk/anthropic",
        ByoKeyKind::Google => "@ai-sdk/google",
        ByoKeyKind::OpenaiCompatible => "@ai-sdk/openai-compatible",
    };
    json!({
        "model": format!("{PROVIDER}/{model}"),
        "provider": {
            PROVIDER: {
                "npm": npm,
                "name": "Sun BYO key",
                "options": options,
                "models": { model: { "name": model } },
            },
        },
    })
}

/// Points a CLI's own provider at the session's BYO key: Claude Code gets
/// `ANTHROPIC_API_KEY`/`ANTHROPIC_BASE_URL` from an `anthropic` key, Codex
/// `OPENAI_API_KEY`/`CODEX_API_KEY`/`OPENAI_BASE_URL` from an
/// `openai_compatible` one, and both the key's model unless the step set
/// one. Idempotent; a no-op without a key or for other adapters (OpenCode
/// renders [`opencode_config`]). The host calls it before an exec env is
/// prepared, so the vars reach a container or VM too.
pub fn native_env(spec: &mut SessionSpec, adapter: &str) -> Result<()> {
    let Some(byo) = spec.byo_key.clone() else {
        return Ok(());
    };
    let (key_vars, url_var): (&[&str], &str) = match adapter {
        "claude-code" => (&["ANTHROPIC_API_KEY"], "ANTHROPIC_BASE_URL"),
        "codex" => (&["OPENAI_API_KEY", "CODEX_API_KEY"], "OPENAI_BASE_URL"),
        _ => return Ok(()),
    };
    if !byo.kind.runs_on(adapter) {
        return Err(Error::InvalidInput(format!(
            "{adapter} can't use a BYO key of kind {}",
            byo.kind
        )));
    }
    byo.validate()?;
    if byo.key_secret.is_some() {
        let key = spec.env.get(KEY_ENV).cloned().ok_or_else(|| {
            Error::InvalidInput(format!(
                "BYO key runtime needs the key's value in env `{KEY_ENV}`"
            ))
        })?;
        for var in key_vars {
            spec.env.insert((*var).into(), key.clone());
        }
    }
    // Always set, so a base URL inherited from the host never gets the key.
    let endpoint = match byo.endpoint.trim() {
        "" => "https://api.anthropic.com", // only `anthropic` may omit it
        endpoint => endpoint,
    };
    spec.env.insert(url_var.into(), endpoint.into());
    spec.model.get_or_insert(byo.model);
    Ok(())
}
