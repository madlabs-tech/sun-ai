//! BYO key: an endpoint rendered into OpenCode's provider config, or
//! into Claude Code's and Codex's own provider env vars, with the key kept
//! out of files, the envelope and events. Fake CLI scripts; no real CLI is
//! called.
#![allow(clippy::unwrap_used)]

use crate::common;

use common::{collect, sandbox, spec};
use serde_json::{Value, json};
use sun_core::{ByoKeyKind, Error, HostCtx, HostId, Role, RuntimeAdapter, SunEvent};
use sun_runtime::byo_key::{ByoKey, KEY_ENV, native_env, opencode_config};
use sun_runtime::{ClaudeCode, OpenCode};

const KEY: &str = "sk-or-test-0123456789abcdef";

fn ollama() -> ByoKey {
    ByoKey {
        kind: ByoKeyKind::OpenaiCompatible,
        endpoint: "http://localhost:11434/v1".into(),
        model: "qwen2.5-coder:7b".into(),
        key_secret: None,
    }
}

fn openrouter() -> ByoKey {
    ByoKey {
        kind: ByoKeyKind::OpenaiCompatible,
        endpoint: "https://openrouter.ai/api/v1".into(),
        model: "anthropic/claude-sonnet-4".into(),
        key_secret: Some("OPENROUTER_KEY".into()),
    }
}

/// Records its config and stdin, then leaks the key in an agent message the
/// way a misbehaving model might. Writes only whether the key is set.
const FAKE_OPENCODE: &str = r#"#!/bin/sh
case "$1" in
  --version) echo "1.18.34"; exit 0 ;;
esac
printf '%s\n' "$OPENCODE_CONFIG_CONTENT" > "$SUN_OUT.config"
[ -n "$SUN_BYO_KEY" ] && echo set > "$SUN_OUT.key"
read -r line; printf '%s\n' "$line" > "$SUN_OUT.stdin"
echo '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1}}'
read -r line; printf '%s\n' "$line" >> "$SUN_OUT.stdin"
echo '{"jsonrpc":"2.0","id":2,"result":{"sessionId":"ses_1"}}'
read -r line; printf '%s\n' "$line" >> "$SUN_OUT.stdin"
printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"ses_1","update":{"sessionUpdate":"agent_message_chunk","messageId":"m1","content":{"type":"text","text":"key is %s"}}}}\n' "$SUN_BYO_KEY"
echo '{"jsonrpc":"2.0","id":3,"result":{"stopReason":"end_turn"}}'
"#;

#[test]
fn byo_key_renders_openai_compatible_provider_config() {
    assert_eq!(
        opencode_config(&ollama(), None),
        json!({
            "model": "sun/qwen2.5-coder:7b",
            "provider": {"sun": {
                "npm": "@ai-sdk/openai-compatible",
                "name": "Sun BYO key",
                "options": {"baseURL": "http://localhost:11434/v1"},
                "models": {"qwen2.5-coder:7b": {"name": "qwen2.5-coder:7b"}},
            }},
        })
    );
    let keyed = opencode_config(&openrouter(), Some("openai/gpt-5"));
    assert_eq!(keyed["model"], "sun/openai/gpt-5");
    assert_eq!(
        keyed["provider"]["sun"]["options"],
        json!({"baseURL": "https://openrouter.ai/api/v1", "apiKey": "{env:SUN_BYO_KEY}"})
    );
    assert_eq!(
        keyed["provider"]["sun"]["models"],
        json!({"openai/gpt-5": {"name": "openai/gpt-5"}})
    );
}

#[test]
fn byo_key_validates_endpoint_model_and_secret_name() {
    assert_eq!(ollama().validate(), Ok(()));
    assert_eq!(openrouter().validate(), Ok(()));
    let bad = [
        ByoKey {
            endpoint: " ".into(),
            ..ollama()
        },
        ByoKey {
            endpoint: "localhost:11434/v1".into(),
            ..ollama()
        },
        ByoKey {
            endpoint: "https://user:sk-secret@api.example.com/v1".into(),
            ..ollama()
        },
        ByoKey {
            model: String::new(),
            ..ollama()
        },
        ByoKey {
            key_secret: Some("my key".into()),
            ..openrouter()
        },
    ];
    for byo in bad {
        let Err(Error::InvalidInput(message)) = byo.validate() else {
            panic!("{byo:?} should be invalid");
        };
        assert!(!message.contains("sk-secret"), "{message}");
    }
}

#[tokio::test]
async fn byo_key_session_keeps_the_key_out_of_files_envelope_and_events() {
    let dir = sandbox("opencode", FAKE_OPENCODE, "byo-key");
    let mut adapter = OpenCode::new(dir.join("opencode"));
    adapter.byo_key = Some(openrouter());
    // The caller resolved `OPENROUTER_KEY` from the secret store.
    let handle = adapter.start(spec(&dir, &[(KEY_ENV, KEY)])).await.unwrap();
    let events = collect(adapter.events(&handle)).await;

    assert!(events.contains(&SunEvent::Text {
        role: Role::Assistant,
        text: "key is [REDACTED]".into(),
    }));
    let exported = adapter.export(&handle).await.unwrap();
    assert!(!format!("{events:?}{exported:?}").contains(KEY));

    // The config references the key; the process got the value via env only.
    let config: Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("out.config")).unwrap()).unwrap();
    assert_eq!(config, opencode_config(&openrouter(), None));
    assert_eq!(
        std::fs::read_to_string(dir.join("out.key")).unwrap(),
        "set\n"
    );
    let stdin = std::fs::read_to_string(dir.join("out.stdin")).unwrap();
    assert!(stdin.contains("Create hello.txt"), "{stdin}");

    // Nothing in the worktree holds the key: not the config, not the envelope.
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        let bytes = std::fs::read(&path).unwrap();
        assert!(
            !String::from_utf8_lossy(&bytes).contains(KEY),
            "{} holds the key",
            path.display()
        );
    }
}

#[tokio::test]
async fn byo_key_start_needs_the_key_value_when_a_secret_is_named() {
    let dir = sandbox("opencode", FAKE_OPENCODE, "byo-key-missing");
    let mut adapter = OpenCode::new(dir.join("opencode"));
    adapter.byo_key = Some(openrouter());
    let err = adapter.start(spec(&dir, &[])).await.unwrap_err();
    assert!(matches!(err, Error::InvalidInput(m) if m.contains(KEY_ENV)));

    // A keyless local endpoint (Ollama) starts without one; a stored
    // runtime's source comes in the spec (the engine sets it).
    adapter.byo_key = None;
    let mut keyless = spec(&dir, &[]);
    keyless.byo_key = Some(ollama());
    let handle = adapter.start(keyless).await.unwrap();
    collect(adapter.events(&handle)).await;
    assert!(!dir.join("out.key").exists());
    let config: Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("out.config")).unwrap()).unwrap();
    assert_eq!(config, opencode_config(&ollama(), None));
}

#[tokio::test]
async fn byo_key_readiness_reports_a_missing_endpoint() {
    let dir = sandbox("opencode", FAKE_OPENCODE, "byo-key-ready");
    let host = HostCtx {
        host_id: HostId::generate(),
    };
    let mut adapter = OpenCode::new(dir.join("opencode"));
    adapter.byo_key = Some(ollama());
    let ready = adapter.readiness(&host).await;
    assert!(ready.installed && ready.logged_in, "{ready:?}");

    adapter.byo_key = Some(ByoKey {
        endpoint: String::new(),
        ..ollama()
    });
    let ready = adapter.readiness(&host).await;
    assert!(ready.installed && !ready.logged_in, "{ready:?}");
    assert!(ready.detail.unwrap().contains("no endpoint"));
}

fn anthropic() -> ByoKey {
    ByoKey {
        kind: ByoKeyKind::Anthropic,
        endpoint: String::new(),
        model: "claude-sonnet-4-5".into(),
        key_secret: Some("ANTHROPIC_KEY".into()),
    }
}

/// Writes the provider env and argv it got, then ends like a turn.
const FAKE_CLAUDE: &str = r#"#!/bin/sh
case "$1" in
  --version) echo "2.1.291 (Claude Code)"; exit 0 ;;
esac
printf 'key=%s url=%s args=%s\n' "$ANTHROPIC_API_KEY" "${ANTHROPIC_BASE_URL-unset}" "$*" > "$SUN_OUT.env"
read -r line
echo '{"type":"result","subtype":"success","is_error":false,"result":"ok","session_id":"x"}'
"#;

#[tokio::test]
async fn byo_key_anthropic_sets_claude_env() {
    let dir = sandbox("claude", FAKE_CLAUDE, "byo-key-anthropic");
    let adapter = ClaudeCode::new(dir.join("claude"));
    let handle = adapter.start(spec_for(&dir, anthropic())).await.unwrap();
    let events = collect(adapter.events(&handle)).await;
    let seen = std::fs::read_to_string(dir.join("out.env")).unwrap();
    assert!(
        seen.starts_with(&format!("key={KEY} url=https://api.anthropic.com ")),
        "{seen}"
    );
    assert!(seen.contains("--model claude-sonnet-4-5"), "{seen}");
    assert!(!format!("{events:?}").contains(KEY));

    // An endpoint goes to ANTHROPIC_BASE_URL; the step's model wins.
    let proxied = ByoKey {
        endpoint: "https://llm.example.com".into(),
        ..anthropic()
    };
    let mut session = spec_for(&dir, proxied.clone());
    session.model = Some("opus".into());
    native_env(&mut session, "claude-code").unwrap();
    assert_eq!(session.env["ANTHROPIC_BASE_URL"], "https://llm.example.com");
    assert_eq!(session.model.as_deref(), Some("opus"));

    // Codex takes only an openai_compatible key, from its own env vars.
    assert!(native_env(&mut session, "codex").is_err());
    let mut session = spec_for(&dir, openrouter());
    native_env(&mut session, "codex").unwrap();
    assert_eq!(session.env["OPENAI_API_KEY"], KEY);
    assert_eq!(session.env["CODEX_API_KEY"], KEY);
    assert_eq!(
        session.env["OPENAI_BASE_URL"],
        "https://openrouter.ai/api/v1"
    );

    // A named secret needs its value; OpenCode renders its config instead.
    let mut keyless = spec(&dir, &[]);
    keyless.byo_key = Some(proxied);
    assert!(native_env(&mut keyless, "claude-code").is_err());
    assert!(native_env(&mut keyless, "opencode").is_ok());
    assert_eq!(
        opencode_config(&anthropic(), None)["provider"]["sun"]["npm"],
        "@ai-sdk/anthropic"
    );
}

fn spec_for(dir: &std::path::Path, byo: ByoKey) -> sun_core::SessionSpec {
    let mut spec = spec(dir, &[(KEY_ENV, KEY)]);
    spec.byo_key = Some(byo);
    spec
}
