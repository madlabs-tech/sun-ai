//! The `credits` runtime source: a runtime pays with
//! Sun credits through the Sun gateway. The runtime runs on the user's host
//! and gets a short-lived gateway key for one session, never a provider key.
//!
//! [`session_env`] is what goes into `SessionSpec::env` for that session; being
//! there, the key is also redacted from every event.

use std::collections::BTreeMap;

use sun_core::{Error, Result};

use crate::byo_key::{ByoKey, KEY_ENV};

/// The env that points `adapter`'s CLI at the gateway at `gateway_url` with
/// gateway key `key` (checked against the CLIs' own auth):
/// - Claude Code: `ANTHROPIC_BASE_URL` + `ANTHROPIC_API_KEY` (Messages API;
///   `-p` and `--bare` use the env key, not the subscription login).
/// - Codex: `OPENAI_BASE_URL` (`<gateway>/v1`) + `CODEX_API_KEY` (Responses
///   API; the key `codex exec` reads). Only one auth env var: Codex flags
///   "multiple auth env vars" when `OPENAI_API_KEY` is set too.
/// - OpenCode: `model` (from the gateway's price table) is required; the
///   gateway becomes an OpenAI-compatible provider in
///   `OPENCODE_CONFIG_CONTENT`, the key in `SUN_BYO_KEY`.
pub fn session_env(
    adapter: &str,
    gateway_url: &str,
    key: &str,
    model: Option<&str>,
) -> Result<BTreeMap<String, String>> {
    let base = gateway_url.trim_end_matches('/');
    let pairs: Vec<(&str, String)> = match adapter {
        crate::claude::ID => vec![
            ("ANTHROPIC_BASE_URL", base.to_owned()),
            ("ANTHROPIC_API_KEY", key.to_owned()),
        ],
        crate::codex::ID => vec![
            ("OPENAI_BASE_URL", format!("{base}/v1")),
            ("CODEX_API_KEY", key.to_owned()),
        ],
        crate::opencode::ID => {
            let model = model.ok_or_else(|| {
                Error::InvalidInput(
                    "Sun credits on `opencode` need a `model` from the gateway's price table"
                        .into(),
                )
            })?;
            let provider = ByoKey {
                kind: sun_core::ByoKeyKind::OpenaiCompatible,
                endpoint: format!("{base}/v1"),
                model: model.to_owned(),
                key_secret: Some(KEY_ENV.into()),
            };
            provider.validate()?;
            vec![
                (
                    "OPENCODE_CONFIG_CONTENT",
                    crate::byo_key::opencode_config(&provider, None).to_string(),
                ),
                (KEY_ENV, key.to_owned()),
            ]
        }
        other => {
            return Err(Error::InvalidInput(format!(
                "Sun credits don't run on `{other}`; use `{}`, `{}` or `{}`",
                crate::claude::ID,
                crate::codex::ID,
                crate::opencode::ID
            )));
        }
    };
    Ok(pairs
        .into_iter()
        .map(|(name, value)| (name.to_owned(), value))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credits_env_points_each_cli_at_the_gateway() {
        let claude = session_env("claude-code", "https://cloud.sun/", "sun_gk_1", None).unwrap();
        assert_eq!(claude["ANTHROPIC_BASE_URL"], "https://cloud.sun");
        assert_eq!(claude["ANTHROPIC_API_KEY"], "sun_gk_1");
        let codex = session_env("codex", "https://cloud.sun", "sun_gk_2", None).unwrap();
        assert_eq!(codex["OPENAI_BASE_URL"], "https://cloud.sun/v1");
        assert_eq!(codex["CODEX_API_KEY"], "sun_gk_2");
        assert!(!codex.contains_key("OPENAI_API_KEY"));
        let refused = session_env("opencode", "https://cloud.sun", "sun_gk_3", None).unwrap_err();
        assert!(refused.to_string().contains("model") && !refused.to_string().contains("sun_gk_3"));
        let opencode = session_env(
            "opencode",
            "https://cloud.sun",
            "sun_gk_4",
            Some("gpt-5-mini"),
        )
        .unwrap();
        assert_eq!(opencode[KEY_ENV], "sun_gk_4");
        let config = &opencode["OPENCODE_CONFIG_CONTENT"];
        assert!(config.contains("https://cloud.sun/v1") && config.contains("sun/gpt-5-mini"));
        assert!(!config.contains("sun_gk_4"), "the key stays in env only");
        assert!(session_env("aider", "https://cloud.sun", "k", None).is_err());
    }
}
