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

/// One request as the owner, with the response's headers (`call` drops them).
async fn call_with_headers(
    app: &axum::Router,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> (StatusCode, axum::http::HeaderMap, Value) {
    use axum::body::Body;
    use axum::http::{Request, header};
    use http_body_util::BodyExt;
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::AUTHORIZATION, format!("Bearer {}", common::TOKEN))
        .header(header::CONTENT_TYPE, "application/json")
        .body(body.map_or(Body::empty(), |b| Body::from(b.to_string())))
        .unwrap();
    let response = common::respond(app, request).await;
    let (status, headers) = (response.status(), response.headers().clone());
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, headers, value)
}

/// BLO-136 case 5 (no-users policy: removed, not aliased). Every old
/// `/runtimes` write and the v1 list answer `410 gone` with no
/// `Deprecation` header, pointing at `/credentials` where a write would
/// have gone. None writes through to a credential of the same id or
/// name, and a v0.1.4 client body changes nothing.
#[tokio::test]
async fn old_runtime_routes_are_gone_not_aliased_and_never_touch_a_credential() {
    let k = common::kernel().await;
    let app = &k.app;
    let list = format!("/api/v1/credentials?workspace={MAIN}");
    let (status, saved) = call(app, "POST", &list, Some(ollama("ollama-qwen"))).await;
    assert_eq!(status, StatusCode::CREATED, "{saved}");
    let id = saved["id"].as_str().unwrap();
    // What a v0.1.4 web or CLI sent to add or edit a key-backed runtime.
    let v014 = json!({ "name": "ollama-qwen", "adapter": "opencode", "source": "byo_key",
        "config": { "endpoint": "http://evil.example/v1", "model": "other",
            "key_secret": "OTHER_KEY" } });
    let gone = [
        ("POST", format!("/api/v1/runtimes?workspace={MAIN}"), true),
        (
            "PUT",
            format!("/api/v1/runtimes/{id}?workspace={MAIN}"),
            true,
        ),
        (
            "PUT",
            format!("/api/v1/runtimes/ollama-qwen?workspace={MAIN}"),
            true,
        ),
        (
            "PUT",
            format!("/api/v1/runtimes/{id}/policy?workspace={MAIN}"),
            false,
        ),
        (
            "GET",
            format!("/api/v1/runtimes?workspace={MAIN}&model=v1"),
            true,
        ),
    ];
    for (method, path, names_credentials) in gone {
        let body = (method != "GET").then(|| v014.clone());
        let (status, headers, value) = call_with_headers(app, method, &path, body).await;
        assert_eq!(status, StatusCode::GONE, "{method} {path}: {value}");
        assert_eq!(value["error"]["code"], "gone", "{method} {path}: {value}");
        assert!(
            headers.get("deprecation").is_none() && headers.get("sunset").is_none(),
            "{method} {path}: a removed route is not a deprecated alias: {headers:?}"
        );
        let message = value["error"]["message"].as_str().unwrap();
        if names_credentials {
            assert!(
                message.contains("/credentials"),
                "{method} {path}: {message}"
            );
        }
    }
    // A DELETE was never a runtime route; it must not reach the credential.
    let (status, _, value) = call_with_headers(
        app,
        "DELETE",
        &format!("/api/v1/runtimes/{id}?workspace={MAIN}"),
        None,
    )
    .await;
    assert!(
        status.is_client_error(),
        "DELETE /runtimes/{id}: {status} {value}"
    );

    let (_, after) = call(app, "GET", &list, None).await;
    assert_eq!(after, json!([saved]), "the credential is exactly as saved");
    let (_, runtimes) = call(
        app,
        "GET",
        &format!("/api/v1/runtimes?workspace={MAIN}"),
        None,
    )
    .await;
    let names: Vec<&str> = runtimes
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r["name"].as_str())
        .collect();
    assert!(
        !names.contains(&"ollama-qwen"),
        "a key is not a runtime: {runtimes}"
    );
}

/// BLO-136 case 4 on `/credentials`: with the key secret set, its value
/// shows in no credential response (POST, PUT, list, one), nor in
/// `/runtimes`, `/agents`, `/secrets` or the audit log; and no response
/// has a `key_secret` field (only `key_secret_set`).
#[tokio::test]
async fn a_set_key_never_shows_in_any_response() {
    const KEY: &str = "sk-or-v1-dummy0123456789abcdefQA136";
    let k = common::kernel().await;
    let app = &k.app;
    let put = format!("/api/v1/secrets/workspace/{MAIN}/OLLAMA_KEY");
    let (status, body) = call(app, "PUT", &put, Some(json!({ "value": KEY }))).await;
    assert!(status.is_success(), "{status} {body}");
    let list = format!("/api/v1/credentials?workspace={MAIN}");
    let one = format!("/api/v1/credentials/ollama-qwen?workspace={MAIN}");
    let mut seen = Vec::new();
    let (status, created) = call(app, "POST", &list, Some(ollama("ollama-qwen"))).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(created["key_secret_set"], true);
    seen.push(("POST /credentials".to_owned(), created));
    let mut edit = ollama("ollama-qwen");
    edit["default_model"] = json!("llama3.1:8b");
    let (status, edited) = call(app, "PUT", &one, Some(edit)).await;
    assert_eq!(status, StatusCode::OK, "{edited}");
    seen.push(("PUT /credentials/{name}".to_owned(), edited));
    let chain = json!([{ "tool": "opencode", "credential": "ollama-qwen" }]);
    let (status, agent) = call(
        app,
        "POST",
        "/api/v1/agents",
        Some(json!({ "name": "Ada", "runtime_chain": chain })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{agent}");
    seen.push(("POST /agents".to_owned(), agent));
    for path in [
        list.clone(),
        one.clone(),
        "/api/v1/credentials".to_owned(),
        format!("/api/v1/runtimes?workspace={MAIN}"),
        "/api/v1/agents".to_owned(),
        "/api/v1/secrets".to_owned(),
        "/api/v1/audit".to_owned(),
    ] {
        let (status, value) = call(app, "GET", &path, None).await;
        assert_eq!(status, StatusCode::OK, "GET {path}: {value}");
        seen.push((format!("GET {path}"), value));
    }
    for (what, value) in &seen {
        let text = value.to_string();
        assert!(!text.contains(KEY), "{what} holds the key: {text}");
        assert!(
            !text.contains("\"key_secret\""),
            "{what} has key_secret: {text}"
        );
    }
}

/// BLO-136 case 1 through the API: a v0.1.4 store (every migration before
/// `20261111090000`) with `byo_key`, `credits` and CLI-login runtimes and
/// their sessions' usage boots on this kernel, and `GET /usage?by=runtime`
/// answers as v0.1.4 did on it (same names, sources, totals, quota window
/// only for the CLI login). The keyed rows read back from `/credentials`
/// under their ids; the credits row's gateway model still comes from
/// `credits_model:<id>`, and its `cred:<id>` cooldown still applies.
#[tokio::test]
async fn usage_by_runtime_reads_the_same_after_the_credential_migration() {
    use sqlx::Connection;
    use sqlx::migrate::Migrate;

    const CREDENTIAL: i64 = 20261111090000;
    const OLLAMA: &str = "01M58A449N5CEQWX89S358ZQE9";
    const CREDITS: &str = "01M58A00000000000000CRED01";
    const LOGIN: &str = "01M58A00000000000000BYRT01";
    let dir = tempfile::tempdir().unwrap();
    let url = format!("sqlite://{}?mode=rwc", dir.path().join("sun.db").display());
    let mut conn = sqlx::SqliteConnection::connect(&url).await.unwrap();
    conn.ensure_migrations_table().await.unwrap();
    let migrator = sqlx::migrate!("../sun-store/migrations");
    for migration in migrator.iter().filter(|m| m.version < CREDENTIAL) {
        conn.apply(migration).await.unwrap();
    }
    let now = chrono::Utc::now();
    let (recent, old) = (
        now.to_rfc3339(),
        (now - chrono::Duration::hours(10)).to_rfc3339(),
    );
    // Usage of sessions this test doesn't build: only the events matter.
    let mut statements = vec![
        "PRAGMA foreign_keys = OFF".to_owned(),
        format!(
            "INSERT INTO runtime (id, workspace_id, name, adapter, source, config_json) VALUES
               ('{OLLAMA}', '{MAIN}', 'ollama-qwen', 'opencode', 'byo_key',
                '{{\"kind\":\"openai_compatible\",\"endpoint\":\"http://localhost:11434/v1\",\"model\":\"qwen2.5-coder:7b\",\"key_secret\":\"OLLAMA_KEY\"}}'),
               ('{CREDITS}', '{MAIN}', 'claude-credits', 'claude-code', 'credits', NULL),
               ('{LOGIN}', '{MAIN}', 'claude-code', 'claude-code', 'byo_runtime', NULL)"
        ),
        format!(
            "INSERT INTO kernel_setting (key, value_json) VALUES
               ('credits_model:{CREDITS}', '\"claude-sonnet-4-5\"')"
        ),
        format!(
            "INSERT INTO account_cooldown (account, until) VALUES
               ('cred:{OLLAMA}', '{}')",
            (now + chrono::Duration::hours(1)).to_rfc3339()
        ),
    ];
    let mut seq = 0;
    for (session, name) in [
        ("SE1", "ollama-qwen"),
        ("SE2", "claude-credits"),
        ("SE3", "claude-code"),
    ] {
        let assigned = json!({ "session_id": session, "adapter": name, "source": "x" });
        statements.push(format!(
            "INSERT INTO event (seq, run_id, kind, payload_json, at)
             VALUES ({}, 'R', 'session_assigned', '{assigned}', '{old}')",
            seq + 1
        ));
        for (at, input, output, cost) in [(&old, 1000, 200, 0.5), (&recent, 10, 2, 0.01)] {
            let usage = json!({ "input": input, "output": output, "cost_usd": cost });
            statements.push(format!(
                "INSERT INTO event (seq, run_id, kind, payload_json, at, session_id, host_seq)
                 VALUES ({}, 'R', 'usage', '{usage}', '{at}', '{session}', {})",
                seq + 2,
                seq + 2
            ));
            seq += 1;
        }
        seq += 1;
    }
    for sql in statements {
        sqlx::query(&sql).execute(&mut conn).await.unwrap();
    }
    conn.close().await.unwrap();

    let k = common::kernel_in(dir).await;
    let app = &k.app;
    let (status, by_runtime) = call(app, "GET", "/api/v1/usage?by=runtime", None).await;
    assert_eq!(status, StatusCode::OK, "{by_runtime}");
    let total = json!({ "input_tokens": 1010, "output_tokens": 202, "cost_usd": 0.51 });
    let window = json!({ "hours": 5,
        "usage": { "input_tokens": 10, "output_tokens": 2, "cost_usd": 0.01 } });
    // What v0.1.4 answered on this store (BLO-136 replay, old binary).
    assert_eq!(
        by_runtime,
        json!([
            { "runtime": "claude-code", "source": "byo_runtime", "usage": total,
              "quota_window": window },
            { "runtime": "claude-credits", "source": "credits", "usage": total,
              "quota_window": null },
            { "runtime": "ollama-qwen", "source": "byo_key", "usage": total,
              "quota_window": null },
        ])
    );
    let (_, credentials) = call(
        app,
        "GET",
        &format!("/api/v1/credentials?workspace={MAIN}"),
        None,
    )
    .await;
    let rows: Vec<(&str, &str, &Value, bool)> = credentials
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            (
                c["id"].as_str().unwrap(),
                c["name"].as_str().unwrap(),
                &c["default_model"],
                c["cooldown_until"].is_string(),
            )
        })
        .collect();
    assert_eq!(
        rows,
        [
            (
                CREDITS,
                "claude-credits",
                &json!("claude-sonnet-4-5"),
                false
            ),
            (OLLAMA, "ollama-qwen", &json!("qwen2.5-coder:7b"), true),
        ],
        "{credentials}"
    );
}
