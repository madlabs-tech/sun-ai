//! Stage 2 of the runtime rework (TRD BLO-117 §15 item 15; items 2 and 3):
//! `GET /runtimes` per `(host, tool)` read next to the `host_runtime`
//! projection. Who may see and re-probe a host, that no key or token rides
//! in a runtimes or hosts answer, one assertion per row of the §6 state
//! table with its precedence, and the `runtimes.model` kill switch.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use axum::Router;
use axum::http::StatusCode;
use futures_util::StreamExt;
use serde_json::{Value, json};
use sun_api::RuntimesModel;
use sun_core::model::{Account, Role, Runtime, RuntimeSource};
use sun_core::{HostId, WorkspaceId};
use sun_store::{HostRepo, RuntimeRepo};
use tokio_tungstenite::tungstenite::Message;

use crate::api::{
    AGENT, Socket, claude_code, connect_host, enrol_host, host_starts_session, kernel_host_hello,
    new_task, recv, runtimes_of, scripted_host, send, until_state,
};
use crate::common::{self, SECRET, TOKEN, kernel};
use crate::workspaces::{call, call_as, member_token_in};

const MAIN: WorkspaceId = WorkspaceId::MAIN;

/// A protocol 2.1 host (it takes `reprobe`) reporting `runtimes`.
async fn host_with(k: &common::Kernel, runtimes: Value) -> Socket {
    let mut hello = kernel_host_hello(k);
    hello["protocol"] = json!("2.1");
    hello["capabilities"]["runtimes"] = runtimes;
    connect_host(&k.app, k.host, hello).await
}

/// No text frame reaches `host` within half a second.
async fn assert_quiet(host: &mut Socket, why: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_millis(500);
    loop {
        match tokio::time::timeout_at(deadline, host.next()).await {
            Err(_) => return,
            Ok(Some(Ok(Message::Text(text)))) => panic!("{why}: the host got {text}"),
            Ok(Some(Ok(_))) => {}
            Ok(_) => panic!("{why}: the socket closed"),
        }
    }
}

fn reprobe_path(workspace: impl std::fmt::Display, host: HostId) -> String {
    format!("/api/v1/workspaces/{workspace}/hosts/{host}/reprobe")
}

/// A second workspace, Beta, made by the deployment owner.
async fn beta(app: &Router) -> WorkspaceId {
    let (status, w) = call(
        app,
        "POST",
        "/api/v1/workspaces",
        Some(json!({ "name": "Beta" })),
    )
    .await;
    assert!(status.is_success(), "{status} {w}");
    serde_json::from_value(w["id"].clone()).unwrap()
}

// --- Case 2: cross-member access ---------------------------------------

/// An admin of Beta (every permission there, `manage_runtimes` included)
/// neither sees Main's host in any runtimes or hosts list nor makes it
/// re-probe, whichever workspace the path names.
#[tokio::test]
async fn another_workspaces_admin_cannot_see_or_reprobe_a_host() {
    let k = kernel().await;
    let app = &k.app;
    let mut host = host_with(&k, claude_code(true)).await;
    let b = beta(app).await;
    let bea = member_token_in(&k.store, "bea", &[(b, Role::Admin)]).await;

    for path in [
        format!("/api/v1/runtimes?workspace={MAIN}"),
        format!("/api/v1/runtimes?workspace={MAIN}&model=v1"),
        format!("/api/v1/runtimes?workspace={MAIN}&model=v2"),
        format!("/api/v1/hosts?workspace={MAIN}"),
    ] {
        let (status, body) = call_as(app, &bea, "GET", &path, None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "GET {path}: {body}");
        assert!(
            !body.to_string().contains(&k.host.to_string()),
            "{path}: {body}"
        );
    }
    // No `?workspace=`: only a deployment owner may list every workspace's.
    let (status, body) = call_as(app, &bea, "GET", "/api/v1/runtimes", None).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    // Her own workspace's lists hold nothing of Main's host.
    for path in [
        format!("/api/v1/runtimes?workspace={b}"),
        format!("/api/v1/runtimes?workspace={b}&model=v1"),
        format!("/api/v1/hosts?workspace={b}"),
    ] {
        let (status, body) = call_as(app, &bea, "GET", &path, None).await;
        assert_eq!(status, StatusCode::OK, "GET {path}: {body}");
        assert!(
            !body.to_string().contains(&k.host.to_string()),
            "{path}: {body}"
        );
    }
    let (status, v2) = call_as(
        app,
        &bea,
        "GET",
        &format!("/api/v1/runtimes?workspace={b}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        v2.as_array().unwrap().iter().all(|r| r["host"].is_null()),
        "Beta has no host: {v2}"
    );

    // "Check again" on Main's host: 404 through Main (she can't see it, as
    // every cross-workspace path answers) and through Beta (not attached
    // there); the host is never asked.
    let (status, body) = call_as(app, &bea, "POST", &reprobe_path(MAIN, k.host), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    let (status, body) = call_as(app, &bea, "POST", &reprobe_path(b, k.host), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_quiet(&mut host, "a re-probe from another workspace").await;
}

/// "Check again" needs `manage_runtimes` in the host's workspace: a member
/// (run, approve) and a viewer are refused and the host is never asked; an
/// admin there gets the re-probe.
#[tokio::test]
async fn reprobe_needs_manage_runtimes_in_the_hosts_workspace() {
    let k = kernel().await;
    let app = &k.app;
    let mut host = host_with(&k, claude_code(false)).await;
    let path = reprobe_path(MAIN, k.host);
    for (name, role) in [("mo", Role::Member), ("vi", Role::Viewer)] {
        let token = member_token_in(&k.store, name, &[(MAIN, role)]).await;
        // Both may read the list…
        let (status, _) = call_as(
            app,
            &token,
            "GET",
            &format!("/api/v1/runtimes?workspace={MAIN}"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{role:?} reads runtimes");
        // …neither may make the host re-probe.
        let (status, body) = call_as(app, &token, "POST", &path, None).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{role:?}: {body}");
        assert_quiet(&mut host, &format!("a re-probe by a {role:?}")).await;
    }

    let ada = member_token_in(&k.store, "ada", &[(MAIN, Role::Admin)]).await;
    let check = tokio::spawn({
        let (app, path) = (app.clone(), path.clone());
        async move { call_as(&app, &ada, "POST", &path, None).await }
    });
    let frame = recv(&mut host).await.unwrap();
    assert_eq!(frame["type"], "reprobe");
    let done = json!({ "type": "reply", "id": frame["id"], "body": { "kind": "done" } });
    send(&mut host, done).await;
    let (status, body) = check.await.unwrap();
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
}

// --- Case 3: no secret in any response ---------------------------------

/// A byo_key runtime's key, the host's key, an enrolment token and the
/// kernel token never show in any `/runtimes` or `/hosts` answer, under
/// either list model and either kill-switch setting.
#[tokio::test]
async fn no_key_or_token_in_runtimes_or_hosts_answers() {
    const KEY: &str = "sk-live-stage2-0123456789abcdef";
    let k = kernel().await;
    let app = &k.app;
    let _host = host_with(&k, claude_code(true)).await;
    let (_, issued) = call(
        app,
        "POST",
        &format!("/api/v1/workspaces/{MAIN}/hosts/tokens"),
        None,
    )
    .await;
    let enrol_token = issued["token"].as_str().unwrap().to_owned();
    let (_vps, _vps_socket) = enrol_host(app, "vps-sg-1", claude_code(false)).await;
    let put = format!("/api/v1/secrets/workspace/{MAIN}/OR_KEY");
    let (status, body) = call(app, "PUT", &put, Some(json!({ "value": KEY }))).await;
    assert!(status.is_success(), "{status} {body}");
    let runtime = json!({ "name": "or-qwen", "source": "byo_key", "config":
        { "endpoint": "http://localhost:11434/v1", "model": "qwen", "key_secret": "OR_KEY" } });
    let (status, saved) = call(
        app,
        "POST",
        &format!("/api/v1/runtimes?workspace={MAIN}"),
        Some(runtime),
    )
    .await;
    assert!(status.is_success(), "{status} {saved}");
    let host_key = sun_api::host_key_for(SECRET, k.host);

    let secrets = [KEY, host_key.as_str(), enrol_token.as_str(), TOKEN];
    let paths = [
        "/api/v1/runtimes".to_owned(),
        "/api/v1/runtimes?model=v1".to_owned(),
        "/api/v1/runtimes?model=v2".to_owned(),
        format!("/api/v1/runtimes?workspace={MAIN}"),
        format!("/api/v1/runtimes?workspace={MAIN}&model=v1"),
        "/api/v1/runtimes/or-qwen/readiness".to_owned(),
        "/api/v1/hosts".to_owned(),
        format!("/api/v1/hosts?workspace={MAIN}"),
    ];
    for model in [RuntimesModel::V1, RuntimesModel::V2] {
        let app = sun_api::router(k.api.clone().with_runtimes_model(model));
        for path in &paths {
            let (status, body) = call(&app, "GET", path, None).await;
            assert_eq!(status, StatusCode::OK, "{model:?} GET {path}: {body}");
            let text = body.to_string();
            for secret in secrets {
                assert!(
                    !text.contains(secret),
                    "{model:?} GET {path} shows a secret ({}…): {text}",
                    &secret[..6]
                );
            }
        }
    }
    // What a literal grep for `key_secret` finds: the secret's *name*, as v1
    // has always returned in `config`, never its value.
    let (_, v2) = call(app, "GET", "/api/v1/runtimes?model=v2", None).await;
    let keyed = v2
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["name"] == "or-qwen")
        .unwrap();
    assert_eq!(keyed["config"]["key_secret"], "OR_KEY", "{keyed}");
}

// --- Case 4: the §6 state table, one assertion per row -------------------

/// `availability` + `state` + `detail` of `host`'s `tool`.
fn row(entry: &Value) -> (Value, Value, Value) {
    (
        entry["availability"].clone(),
        entry["state"].clone(),
        entry["detail"].clone(),
    )
}

/// idle, idle with the `outdated` badge, needs_login, limited, offline: the
/// rows a host can produce today (`unsupported` can't appear until item 10).
#[tokio::test]
async fn each_state_row_of_the_copy_table() {
    let k = kernel().await;
    let app = &k.app;
    let untested = json!([
        { "adapter": "claude-code", "readiness": { "installed": true, "version": "2.2.0",
          "logged_in": true, "detail": "Claude Code 2.2.0 is untested" } },
        { "adapter": "codex", "readiness": { "installed": true, "version": "0.160.0",
          "logged_in": true, "detail": null } },
        { "adapter": "opencode", "readiness": { "installed": true, "version": "1.0.0",
          "logged_in": false, "detail": "OpenCode is not logged in. Run: opencode auth login" } },
    ]);
    let laptop = host_with(&k, untested).await;
    let at = |tool: &'static str| {
        let app = app.clone();
        let host = k.host.to_string();
        async move { runtimes_of(&app, tool).await.remove(&host).unwrap() }
    };

    // idle: success "Ready", no line.
    let codex = at("codex").await;
    assert_eq!(
        row(&codex),
        (json!("ready"), json!("idle"), Value::Null),
        "{codex}"
    );
    assert_eq!(codex["presence"], "online");
    assert_eq!(codex["load"], json!({ "active": 0, "max": null }));
    // idle with the "Untested version" badge: still idle (schedulable).
    let claude = at("claude-code").await;
    assert_eq!(
        row(&claude),
        (
            json!("outdated"),
            json!("idle"),
            json!("Claude Code 2.2.0 is untested")
        ),
        "{claude}"
    );
    // needs_login: the real command and the host's name.
    let opencode = at("opencode").await;
    assert_eq!(
        row(&opencode),
        (
            json!("needs_login"),
            json!("needs_login"),
            json!("Run `opencode auth login` on box")
        ),
        "{opencode}"
    );
    // limited: the account is cooling; back at `cooldown_until`.
    let until = chrono::Utc::now() + chrono::Duration::minutes(30);
    cool(&k, "codex", until).await;
    let codex = at("codex").await;
    assert_eq!(
        (&codex["availability"], &codex["state"]),
        (&json!("limited"), &json!("limited"))
    );
    let back: chrono::DateTime<chrono::Utc> =
        serde_json::from_value(codex["cooldown_until"].clone()).unwrap();
    assert!((back - until).num_seconds().abs() <= 1, "{back} vs {until}");
    assert!(
        codex["detail"]
            .as_str()
            .unwrap()
            .starts_with("Usage limit reached; back at"),
        "{codex}"
    );

    // offline: every tool on the host, whatever its login, cooldown or badge.
    drop(laptop);
    for tool in ["codex", "claude-code", "opencode"] {
        let gone = until_state(app, tool, k.host, "offline").await;
        assert_eq!(gone["presence"], "offline", "{gone}");
        assert_eq!(gone["detail"], "box is offline", "{gone}");
        assert!(
            gone["last_seen_at"].is_string(),
            "the UI's \"hasn't been seen since\": {gone}"
        );
    }
}

/// Cools the CLI login of `tool` on the kernel's host until `until`.
async fn cool(k: &common::Kernel, tool: &str, until: chrono::DateTime<chrono::Utc>) {
    let account = Account::Login(tool.into()).key(k.host);
    let mut tx = k.store.begin().await.unwrap();
    tx.set_cooldown(&account, Some(until)).await.unwrap();
    tx.commit().await.unwrap();
}

/// Item 5: a cooldown follows the account that hit the limit. A CLI login
/// cools only its own `(host, tool)`; a key cools every host that would
/// spend it (and its own entry); an ended cooldown reads `idle` again with
/// no restart. No entry carries the internal account key.
#[tokio::test]
async fn limited_follows_the_account_per_host_and_tool() {
    let k = kernel().await;
    let app = &k.app;
    let _laptop = host_with(&k, claude_code(true)).await;
    let (vps, _vps) = enrol_host(app, "vps-sg-1", claude_code(true)).await;
    let both = || async {
        let claude = runtimes_of(app, "claude-code").await;
        let state = |host: HostId| claude[&host.to_string()]["state"].clone();
        (state(k.host), state(vps))
    };
    assert_eq!(both().await, (json!("idle"), json!("idle")));

    // The laptop's own login hits its limit: the VPS, logged into another
    // account, stays idle. Briefly, so it also ends.
    let until = chrono::Utc::now() + chrono::Duration::milliseconds(1500);
    cool(&k, "claude-code", until).await;
    assert_eq!(both().await, (json!("limited"), json!("idle")));
    let laptop = runtimes_of(app, "claude-code").await[&k.host.to_string()].clone();
    let back: chrono::DateTime<chrono::Utc> =
        serde_json::from_value(laptop["cooldown_until"].clone()).unwrap();
    assert!((back - until).num_milliseconds().abs() <= 1, "{laptop}");
    until_state(app, "claude-code", k.host, "idle").await;

    // A key both hosts would run Claude Code with: both limited, and the
    // key's own entry carries the cooldown.
    let key = Runtime {
        id: sun_core::RuntimeId::generate(),
        workspace_id: MAIN,
        name: "claude-credits".into(),
        adapter: "claude-code".into(),
        source: RuntimeSource::Credits,
        byo_key: None,
        created_by: None,
    };
    let until = chrono::Utc::now() + chrono::Duration::minutes(30);
    let mut tx = k.store.begin().await.unwrap();
    assert!(tx.insert_runtime(&key).await.unwrap());
    tx.set_cooldown(&Account::Credential(key.id).key(k.host), Some(until))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let (status, list) = call(app, "GET", "/api/v1/runtimes", None).await;
    assert_eq!(status, StatusCode::OK, "{list}");
    let entries = list.as_array().unwrap();
    let claude: Vec<&Value> = entries
        .iter()
        .filter(|r| r["tool"] == "claude-code")
        .collect();
    assert_eq!(claude.len(), 3, "two hosts and the key: {list}");
    for entry in claude {
        assert!(entry["cooldown_until"].is_string(), "{entry}");
        // The key's own entry: credits aren't on this deployment, which
        // comes first.
        if !entry["host"].is_null() {
            assert_eq!(entry["state"], "limited", "{entry}");
        }
    }
    let text = list.to_string();
    assert!(
        !text.contains("cred:") && !text.contains("login:"),
        "no account key in the API: {list}"
    );
}

/// busy: one live session on the host's tool; the badge reads its load.
/// Then the precedence above it: a cooldown beats busy (limited), and a
/// host gone beats both (offline), with the session still counted.
#[tokio::test]
async fn busy_then_limited_over_busy_then_offline_over_all() {
    let k = kernel().await;
    let app = &k.app;
    let mut host = scripted_host(&k, json!({})).await;
    let task = new_task(app, AGENT).await;
    let session_id = host_starts_session(&mut host, "/data/work/t").await;
    let stats = json!({ "type": "stats", "session_id": session_id,
        "rss_bytes": 1024, "cpu_percent": 1.0, "uptime_s": 1 });
    send(&mut host, stats).await;
    let busy = until_state(app, "fake", k.host, "busy").await;
    assert_eq!(busy["availability"], "ready", "{busy}");
    assert_eq!(busy["load"], json!({ "active": 1, "max": null }), "{busy}");
    assert!(task["id"].is_string());

    cool(
        &k,
        "fake",
        chrono::Utc::now() + chrono::Duration::minutes(5),
    )
    .await;
    let limited = until_state(app, "fake", k.host, "limited").await;
    assert_eq!(limited["load"]["active"], 1, "still running: {limited}");

    drop(host);
    let offline = until_state(app, "fake", k.host, "offline").await;
    assert_eq!(offline["presence"], "offline", "{offline}");
}

/// The precedence rows the TRD names, as the API derives them, with the
/// `auth` the `host_runtime` projection stores for each report:
/// installed + logged in → `logged_in`, installed only → `logged_out`,
/// not installed → `unknown`.
#[tokio::test]
async fn an_offline_host_that_is_also_logged_out_is_offline_not_needs_login() {
    let k = kernel().await;
    let app = &k.app;
    let logged_out = host_with(&k, claude_code(false)).await;
    let entry = until_state(app, "claude-code", k.host, "needs_login").await;
    assert_eq!(entry["availability"], "needs_login");

    // The projection agrees with the mapping.
    let mut tx = k.store.begin().await.unwrap();
    let rows = tx.host_runtimes(Some(MAIN)).await.unwrap();
    drop(tx);
    let auth: std::collections::BTreeMap<_, _> = rows
        .iter()
        .filter(|r| r.host_id == k.host)
        .map(|r| (r.tool.as_str(), (r.installed, r.auth.as_str())))
        .collect();
    assert_eq!(
        auth.get("claude-code"),
        Some(&(true, "logged_out")),
        "{auth:?}"
    );
    assert_eq!(auth.get("codex"), Some(&(false, "unknown")), "{auth:?}");

    // Logged out *and* cooling: needs_login comes first.
    cool(
        &k,
        "claude-code",
        chrono::Utc::now() + chrono::Duration::minutes(5),
    )
    .await;
    let entry = runtimes_of(app, "claude-code")
        .await
        .remove(&k.host.to_string())
        .unwrap();
    assert_eq!(entry["state"], "needs_login", "{entry}");

    // The host goes: offline, not needs_login, though its last report
    // (still shown as `availability`) says logged out.
    drop(logged_out);
    let gone = until_state(app, "claude-code", k.host, "offline").await;
    assert_eq!(gone["availability"], "needs_login", "{gone}");
    assert_eq!(gone["detail"], "box is offline", "{gone}");
}

/// After a restart the hub has never seen an offline host, so under `v2`
/// its `last_seen_at` is the row's last probe, not the enrolment (BLO-149).
#[tokio::test]
async fn an_offline_host_after_a_restart_was_last_seen_at_its_last_probe() {
    let k = kernel().await;
    let laptop = host_with(&k, claude_code(true)).await;
    until_state(&k.app, "claude-code", k.host, "idle").await;
    drop(laptop);
    let k = common::restarted(k, sun_api::Features::default()).await;
    let app = sun_api::router(k.api.clone().with_runtimes_model(RuntimesModel::V2));
    let gone = runtimes_of(&app, "claude-code")
        .await
        .remove(&k.host.to_string())
        .unwrap();
    assert_eq!(gone["state"], "offline", "{gone}");
    assert!(gone["probed_at"].is_string(), "{gone}");
    assert_eq!(gone["last_seen_at"], gone["probed_at"], "{gone}");
}

// --- Case 5: the kill switch ---------------------------------------------

/// The old list's exact field set (main before item 2).
const V1_FIELDS: [&str; 13] = [
    "adapter",
    "allow_bypass",
    "config",
    "cooldown_until",
    "id",
    "max_sessions",
    "model",
    "name",
    "permission_mode",
    "sessions",
    "source",
    "status",
    "workspace_id",
];

/// `sun.yaml` `runtimes.model` picks the data source, not the shape (option
/// B, BLO-132): with no `?model=` both values answer the §6 per-`(host,
/// tool)` list, so the page works under either. `v1` reads the raw report
/// (`id` `<host>:<tool>`, no `probed_at`), `v2` the `host_runtime` rows (row
/// `id` and `probed_at`); every other field is the same.
#[tokio::test]
async fn sun_yaml_runtimes_model_picks_the_source_of_the_default_list() {
    let k = kernel().await;
    let _host = host_with(&k, claude_code(true)).await;
    let (vps, _socket) = enrol_host(&k.app, "vps-sg-1", claude_code(false)).await;
    let rows = k
        .store
        .read()
        .await
        .unwrap()
        .host_runtimes(None)
        .await
        .unwrap();
    let mut lists = Vec::new();
    for model in [RuntimesModel::V1, RuntimesModel::V2] {
        let app = sun_api::router(k.api.clone().with_runtimes_model(model));
        let (status, default) = call(&app, "GET", "/api/v1/runtimes", None).await;
        assert_eq!(status, StatusCode::OK, "{model:?}: {default}");
        let (_, v2) = call(&app, "GET", "/api/v1/runtimes?model=v2", None).await;
        assert_eq!(default, v2, "{model:?}: no ?model= is the §6 shape");
        let mut claude = runtimes_of(&app, "claude-code").await;
        assert_eq!(claude.len(), 2, "{model:?}: one entry per host: {claude:?}");
        for host in [k.host, vps] {
            let entry = claude.remove(&host.to_string()).unwrap();
            let row = rows
                .iter()
                .find(|r| r.host_id == host && r.tool == "claude-code")
                .unwrap();
            let (id, probed_at) = match model {
                RuntimesModel::V1 => (json!(format!("{host}:claude-code")), Value::Null),
                RuntimesModel::V2 => (json!(row.id.to_string()), json!(row.probed_at)),
            };
            assert_eq!(entry["id"], id, "{model:?} id: {entry}");
            assert_eq!(
                entry["probed_at"], probed_at,
                "{model:?} probed_at: {entry}"
            );
        }
        lists.push(strip_source(default));
    }
    assert_eq!(lists[0], lists[1], "same facts from either source");
}

/// `?model=` takes `v1` or `v2` only, under either `sun.yaml` value.
#[tokio::test]
async fn an_unknown_model_flag_is_refused_under_either_default() {
    let k = kernel().await;
    for model in [RuntimesModel::V1, RuntimesModel::V2] {
        let app = sun_api::router(k.api.clone().with_runtimes_model(model));
        for flag in ["v3", "V1", "", "v1%20"] {
            let path = format!("/api/v1/runtimes?model={flag}");
            let (status, body) = call(&app, "GET", &path, None).await;
            assert_eq!(
                status,
                StatusCode::UNPROCESSABLE_ENTITY,
                "{model:?} {path}: {body}"
            );
        }
    }
}

/// The list with each entry's source-specific `id` and `probed_at` blanked.
fn strip_source(mut list: Value) -> Value {
    for entry in list.as_array_mut().unwrap() {
        entry["id"] = Value::Null;
        entry["probed_at"] = Value::Null;
    }
    list
}

/// Flipping the switch v2 → v1 → v2 loses nothing: no `host_runtime` row
/// is lost or changed, `?model=v1` is the same old JSON under either value,
/// the default list comes back identical, and placement (an agent's
/// readiness) reads the same from either source.
#[tokio::test]
async fn flipping_the_kill_switch_loses_nothing_and_v1_stays_the_old_json() {
    let k = kernel().await;
    let _host = host_with(&k, claude_code(true)).await;
    let (_vps, _socket) = enrol_host(&k.app, "vps-sg-1", claude_code(false)).await;
    let rows = || async {
        let mut tx = k.store.begin().await.unwrap();
        let mut rows: Vec<_> = tx
            .host_runtimes(None)
            .await
            .unwrap()
            .into_iter()
            .map(|r| (r.id.to_string(), r.host_id, r.tool, r.auth, r.removed_at))
            .collect();
        rows.sort();
        rows
    };
    let before = rows().await;
    assert_eq!(
        before.len(),
        4,
        "2 hosts × (claude-code, codex): {before:?}"
    );

    for (name, tool) in [("Ada", "claude-code"), ("Oz", "opencode")] {
        let body = json!({ "name": name, "runtime_chain": [tool] });
        let (status, agent) = call(&k.app, "POST", "/api/v1/agents", Some(body)).await;
        assert_eq!(status, StatusCode::CREATED, "{agent}");
    }
    let get = |model: RuntimesModel, path: &'static str| {
        let app = sun_api::router(k.api.clone().with_runtimes_model(model));
        async move {
            let (status, body) = call(&app, "GET", path, None).await;
            assert_eq!(status, StatusCode::OK, "{model:?} {path}: {body}");
            body
        }
    };
    let list_on_v2 = get(RuntimesModel::V2, "/api/v1/runtimes").await;
    let v1_on_v2 = get(RuntimesModel::V2, "/api/v1/runtimes?model=v1").await;
    let agents_on_v2 = get(RuntimesModel::V2, "/api/v1/agents").await;
    // Flip to v1, then back to v2.
    let list_on_v1 = get(RuntimesModel::V1, "/api/v1/runtimes").await;
    let v1_on_v1 = get(RuntimesModel::V1, "/api/v1/runtimes?model=v1").await;
    let agents_on_v1 = get(RuntimesModel::V1, "/api/v1/agents").await;
    let list_again = get(RuntimesModel::V2, "/api/v1/runtimes").await;
    assert_eq!(
        v1_on_v1, v1_on_v2,
        "?model=v1 is the same under either value"
    );
    assert_eq!(list_on_v2, list_again, "flip and back");
    assert_eq!(strip_source(list_on_v1), strip_source(list_on_v2.clone()));
    assert_eq!(agents_on_v1, agents_on_v2, "placement reads the same");
    let ready: Vec<_> = agents_on_v2
        .as_array()
        .unwrap()
        .iter()
        .map(|a| (a["name"].clone(), a["ready"].clone()))
        .collect();
    assert_eq!(
        ready,
        [(json!("Ada"), json!(true)), (json!("Oz"), json!(false))],
        "{agents_on_v2}"
    );
    assert_eq!(rows().await, before, "no host_runtime row lost or changed");
    for entry in v1_on_v1.as_array().unwrap() {
        let mut keys: Vec<_> = entry
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(keys, V1_FIELDS, "the old shape: {entry}");
    }
    let claude = v1_on_v1
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["name"] == "claude-code")
        .unwrap();
    assert_eq!(
        claude["status"], "ready",
        "one online host is logged in: {claude}"
    );
}

// --- BLO-136 case 3: a cooling key stays in its own workspace ----------

/// Main's key on Claude Code cools. Beta's own host (enrolled into Beta
/// only) is not marked `limited` by it, in Beta's list, in the owner's
/// all-workspaces list, or to Beta's admin; Main's host is. Beta's admin
/// sees nothing of Main's host or key.
#[tokio::test]
async fn a_cooling_key_never_marks_another_workspaces_host() {
    use std::future::IntoFuture;
    let k = kernel().await;
    let app = &k.app;
    let _main_host = host_with(&k, claude_code(true)).await;
    let b = beta(app).await;
    let bea = member_token_in(&k.store, "bea", &[(b, Role::Admin)]).await;
    // Enrol a host into Beta.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/host", listener.local_addr().unwrap());
    tokio::spawn(axum::serve(listener, app.clone()).into_future());
    let (status, issued) = call(
        app,
        "POST",
        &format!("/api/v1/workspaces/{b}/hosts/tokens"),
        None,
    )
    .await;
    assert!(status.is_success(), "{status} {issued}");
    let enroll = json!({ "type": "enroll", "token": issued["token"], "name": "beta-vps" });
    let enrolled = crate::api::ask(&url, enroll).await.unwrap();
    let beta_host: HostId = serde_json::from_value(enrolled["host_id"].clone()).unwrap();
    let mut hello = crate::api::hello(beta_host, enrolled["host_key"].as_str().unwrap());
    hello["capabilities"]["runtimes"] = claude_code(true);
    let _beta_socket = connect_host(app, beta_host, hello).await;

    let key = Runtime {
        id: sun_core::RuntimeId::generate(),
        workspace_id: MAIN,
        name: "claude-credits".into(),
        adapter: "claude-code".into(),
        source: RuntimeSource::Credits,
        byo_key: None,
        created_by: None,
    };
    let until = chrono::Utc::now() + chrono::Duration::minutes(30);
    let mut tx = k.store.begin().await.unwrap();
    assert!(tx.insert_runtime(&key).await.unwrap());
    tx.set_cooldown(&Account::Credential(key.id).key(k.host), Some(until))
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let state_in = |list: &Value, host: HostId| {
        list.as_array()
            .unwrap()
            .iter()
            .find(|r| r["tool"] == "claude-code" && r["host"]["id"] == json!(host))
            .map(|r| r["state"].clone())
    };
    for model in [RuntimesModel::V2, RuntimesModel::V1] {
        let app = &sun_api::router(k.api.clone().with_runtimes_model(model));
        let (_, owner) = call(app, "GET", "/api/v1/runtimes", None).await;
        assert_eq!(
            state_in(&owner, k.host),
            Some(json!("limited")),
            "{model:?} {owner}"
        );
        assert_eq!(
            state_in(&owner, beta_host),
            Some(json!("idle")),
            "{model:?} {owner}"
        );
        let beta_path = format!("/api/v1/runtimes?workspace={b}");
        for (who, list) in [
            ("owner", call(app, "GET", &beta_path, None).await.1),
            ("bea", call_as(app, &bea, "GET", &beta_path, None).await.1),
        ] {
            assert_eq!(
                state_in(&list, beta_host),
                Some(json!("idle")),
                "{model:?} {who}: {list}"
            );
            let text = list.to_string();
            for hidden in [
                k.host.to_string(),
                key.id.to_string(),
                "cred:".into(),
                "login:".into(),
            ] {
                assert!(
                    !text.contains(&hidden),
                    "{model:?} {who} sees {hidden}: {list}"
                );
            }
        }
    }
}
