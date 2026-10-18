//! `/api/v1/credentials` (TRD BLO-117 item 7, BLO-122): a workspace's API
//! keys, written by `manage_runtimes` there, read by any member, never with
//! key material; and the old `/runtimes` writes, gone (`410`) for everyone.
#![allow(clippy::unwrap_used)]

use axum::http::StatusCode;
use serde_json::{Value, json};
use sun_core::WorkspaceId;
use sun_core::model::Role;

use crate::common;
use crate::workspaces::{call, call_as, member_token};

const MAIN: WorkspaceId = WorkspaceId::MAIN;

fn ollama(name: &str) -> Value {
    json!({ "name": name, "source": "byo_key", "default_tool": "opencode",
        "endpoint": "http://localhost:11434/v1", "default_model": "qwen2.5-coder:7b",
        "key_secret": "OLLAMA_KEY" })
}

#[tokio::test]
async fn a_credential_is_added_read_edited_and_deleted_without_its_key_secret() {
    let k = common::kernel().await;
    let app = &k.app;
    let list = format!("/api/v1/credentials?workspace={MAIN}");
    let one = format!("/api/v1/credentials/ollama-qwen?workspace={MAIN}");

    let (status, saved) = call(app, "POST", &list, Some(ollama("ollama-qwen"))).await;
    assert_eq!(status, StatusCode::CREATED, "{saved}");
    let view = |v: &Value| {
        let mut v = v.clone();
        for field in ["id", "created_by", "state", "detail", "cooldown_until"] {
            v.as_object_mut().unwrap().remove(field);
        }
        v
    };
    assert_eq!(
        view(&saved),
        json!({ "name": "ollama-qwen", "workspace_id": MAIN, "source": "byo_key",
            "family": "openai_compatible", "default_tool": "opencode",
            "endpoint": "http://localhost:11434/v1", "default_model": "qwen2.5-coder:7b",
            "key_secret_set": true })
    );
    // The secret isn't set yet: the line says what to set, by name.
    assert!(
        saved["detail"].as_str().unwrap().starts_with("key_secret:"),
        "{saved}"
    );
    let (status, again) = call(app, "POST", &list, Some(ollama("ollama-qwen"))).await;
    assert_eq!(status, StatusCode::CONFLICT, "{again}");
    let (status, listed) = call(app, "GET", &list, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(listed, json!([saved.clone()]));
    let (_, read) = call(app, "GET", &one, None).await;
    assert_eq!(read, saved);

    // Without `key_secret` a PUT keeps the stored one; `""` clears it.
    let mut edit = ollama("ollama-qwen");
    edit.as_object_mut().unwrap().remove("key_secret");
    edit["default_model"] = json!("llama3.1:8b");
    let (status, edited) = call(app, "PUT", &one, Some(edit.clone())).await;
    assert_eq!(status, StatusCode::OK, "{edited}");
    assert_eq!(edited["id"], saved["id"], "same credential");
    assert_eq!(
        (&edited["default_model"], &edited["key_secret_set"]),
        (&json!("llama3.1:8b"), &json!(true))
    );
    edit["key_secret"] = json!("");
    let (_, cleared) = call(app, "PUT", &one, Some(edit.clone())).await;
    assert_eq!(cleared["key_secret_set"], false, "{cleared}");
    edit["name"] = json!("renamed");
    let (status, body) = call(app, "PUT", &one, Some(edit)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .starts_with("name:")
    );

    let (status, _) = call(app, "DELETE", &one, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = call(app, "DELETE", &one, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (_, listed) = call(app, "GET", &list, None).await;
    assert_eq!(listed, json!([]));
}

/// A raw key never gets in: `config`/`key` are unknown fields, and
/// `key_secret` takes only a secret's name. Errors name the field.
#[tokio::test]
async fn bad_input_is_refused_naming_the_field_and_never_echoes_a_key() {
    const KEY: &str = "sk-ant-api03-0123456789abcdef";
    let k = common::kernel().await;
    let app = &k.app;
    let list = format!("/api/v1/credentials?workspace={MAIN}");
    let with = |patch: Value| {
        let mut body = ollama("k");
        for (field, value) in patch.as_object().unwrap() {
            body[field] = value.clone();
        }
        body
    };
    for (body, field) in [
        (with(json!({ "key_secret": KEY })), "key_secret:"),
        (with(json!({ "default_model": "a b" })), "default_model:"),
        (with(json!({ "endpoint": "ftp://h" })), "endpoint:"),
        (with(json!({ "name": "Big Name" })), "name:"),
        (
            with(json!({ "default_tool": "codex", "family": "anthropic" })),
            "default_tool:",
        ),
        (with(json!({ "source": "byo_runtime" })), "source:"),
        (
            json!({ "name": "c", "source": "credits", "default_tool": "opencode" }),
            "source:",
        ),
    ] {
        let (status, value) = call(app, "POST", &list, Some(body.clone())).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}: {value}");
        let message = value["error"]["message"].as_str().unwrap();
        assert!(message.starts_with(field), "{body}: {message}");
        assert!(!value.to_string().contains(KEY), "{value}");
    }
    for extra in [
        json!({ "key": KEY }),
        json!({ "config": { "api_key": KEY } }),
    ] {
        let (status, value) = call(app, "POST", &list, Some(with(extra))).await;
        assert!(status.is_client_error(), "{status} {value}");
        assert!(!value.to_string().contains(KEY), "{value}");
    }
    let (_, listed) = call(app, "GET", &list, None).await;
    assert_eq!(listed, json!([]), "nothing was stored");
}

/// A credential an agent's chain names (bare or as `credential:`) stays
/// until the chain stops naming it; then it goes.
#[tokio::test]
async fn a_credential_an_agent_names_is_kept() {
    let k = common::kernel().await;
    let app = &k.app;
    let list = format!("/api/v1/credentials?workspace={MAIN}");
    let (status, _) = call(app, "POST", &list, Some(ollama("ollama-qwen"))).await;
    assert_eq!(status, StatusCode::CREATED);
    let chain = json!([{ "tool": "opencode", "credential": "ollama-qwen" }]);
    let body = json!({ "name": "Ada", "runtime_chain": chain });
    let (status, agent) = call(app, "POST", "/api/v1/agents", Some(body)).await;
    assert_eq!(status, StatusCode::CREATED, "{agent}");
    let one = format!("/api/v1/credentials/ollama-qwen?workspace={MAIN}");
    let (status, refused) = call(app, "DELETE", &one, None).await;
    assert_eq!(status, StatusCode::CONFLICT, "{refused}");
    assert!(refused.to_string().contains("Ada"), "{refused}");

    // A chain naming a missing credential is refused by name.
    let body = json!({ "name": "Bo",
        "runtime_chain": [{ "tool": "opencode", "credential": "nope" }] });
    let (status, value) = call(app, "POST", "/api/v1/agents", Some(body)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{value}");
    assert!(
        value["error"]["message"]
            .as_str()
            .unwrap()
            .contains("no credential named `nope`"),
        "{value}"
    );

    let path = format!("/api/v1/agents/{}", agent["id"].as_str().unwrap());
    let body = json!({ "runtime_chain": ["opencode"] });
    let (status, value) = call(app, "PATCH", &path, Some(body)).await;
    assert!(status.is_success(), "{value}");
    let (status, _) = call(app, "DELETE", &one, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

/// Any member reads; only `manage_runtimes` (an admin) writes.
#[tokio::test]
async fn members_read_credentials_and_admins_write_them() {
    let k = common::kernel().await;
    let app = &k.app;
    let list = format!("/api/v1/credentials?workspace={MAIN}");
    let viewer = member_token(&k.store, "vic", Role::Viewer).await;
    let admin = member_token(&k.store, "eve", Role::Admin).await;
    let (status, _) = call_as(app, &viewer, "POST", &list, Some(ollama("v"))).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, saved) = call_as(app, &admin, "POST", &list, Some(ollama("e"))).await;
    assert_eq!(status, StatusCode::CREATED, "{saved}");
    let (status, listed) = call_as(app, &viewer, "GET", &list, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(listed.as_array().unwrap().len(), 1);
    let one = format!("/api/v1/credentials/e?workspace={MAIN}");
    for (method, body) in [("PUT", Some(ollama("e"))), ("DELETE", None)] {
        let (status, _) = call_as(app, &viewer, method, &one, body).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method}");
    }
}

/// The old runtime writes are gone: `410` for an owner and for a member
/// without `manage_runtimes` alike (no role `403` in front of the `410`,
/// as for the retired policy route).
#[tokio::test]
async fn runtime_writes_are_gone() {
    let k = common::kernel().await;
    let app = &k.app;
    let member = member_token(&k.store, "bob", Role::Member).await;
    let body = json!({ "name": "x", "source": "byo_key",
        "config": { "endpoint": "http://localhost:11434/v1", "model": "qwen" } });
    for token in [common::TOKEN, member.as_str()] {
        for (method, path) in [
            ("POST", format!("/api/v1/runtimes?workspace={MAIN}")),
            ("PUT", format!("/api/v1/runtimes/x?workspace={MAIN}")),
            ("PUT", format!("/api/v1/runtimes/x/policy?workspace={MAIN}")),
        ] {
            let (status, value) = call_as(app, token, method, &path, Some(body.clone())).await;
            assert_eq!(status, StatusCode::GONE, "{method} {path}: {value}");
            assert_eq!(value["error"]["code"], "gone", "{value}");
        }
    }
    let (_, credentials) = call(app, "GET", "/api/v1/credentials", None).await;
    assert_eq!(credentials, json!([]), "nothing was written");
}
