//! The repo editor end to end: the real `sun-host` behind the kernel,
//! file operations in the owner's editor worktree, the write precondition,
//! and who is kept out (owner-only until editor grants land).
#![allow(clippy::unwrap_used, clippy::expect_used)] // test helpers

use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sun_core::model::Role;
use sun_core::{HostId, WorkspaceId};

use crate::common::{self, SECRET, TOKEN, kernel};
use crate::workspaces::member_token;

/// One request as `token`: a JSON body, or raw text with `If-Match`.
async fn call(
    app: &Router,
    token: &str,
    method: &str,
    path: &str,
    body: Body,
    if_match: Option<&str>,
) -> (StatusCode, Value) {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::AUTHORIZATION, format!("Bearer {token}"));
    request = match if_match {
        Some(hash) => request
            .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
            .header(header::IF_MATCH, format!("\"{hash}\"")),
        None => request.header(header::CONTENT_TYPE, "application/json"),
    };
    let response = common::respond(app, request.body(body).unwrap()).await;
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn json_body(value: Value) -> Body {
    Body::from(value.to_string())
}

fn git(cwd: &std::path::Path, args: &[&str]) {
    let ok = std::process::Command::new("git")
        .args(["-c", "user.email=t@t", "-c", "user.name=t"])
        .args(args)
        .current_dir(cwd)
        .status()
        .unwrap();
    assert!(ok.success(), "git {args:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_owner_edits_a_repo_on_a_host_and_others_are_kept_out() {
    let k = kernel().await;
    let app = &k.app;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/host", listener.local_addr().unwrap());
    tokio::spawn(axum::serve(listener, app.clone()).into_future());

    let dir = tempfile::tempdir().unwrap();
    let origin = dir.path().join("origin");
    std::fs::create_dir_all(&origin).unwrap();
    std::fs::write(origin.join("README.md"), "hi\n").unwrap();
    git(&origin, &["init", "-q", "-b", "main"]);
    git(&origin, &["add", "."]);
    git(&origin, &["commit", "-q", "-m", "base"]);
    let host_dir = dir.path().join("host");
    let host = sun_host::Host::open(sun_host::HostConfig::new(&host_dir), vec![]).unwrap();
    let key: String = Sha256::digest(format!("{SECRET}:{}", k.host).as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    tokio::spawn(host.run(sun_host::Credentials {
        kernel_url: url,
        host_id: k.host,
        host_key: key,
    }));
    let repo = common::local_repo(&k, WorkspaceId::MAIN, origin.to_str().unwrap()).await;
    let owner = member_token(&k.store, "olive", Role::Owner).await;
    let admin = member_token(&k.store, "adam", Role::Admin).await;
    let base = format!(
        "/api/v1/workspaces/{}/repos/{repo}/editor",
        WorkspaceId::MAIN
    );
    let at = |route: &str, path: &str| format!("{base}/{route}?host_id={}&path={path}", k.host);

    // Open (the host may still be dialling in).
    let mut opened = Value::Null;
    for _ in 0..500 {
        let (status, body) = call(app, &owner, "POST", &at("open", ""), Body::empty(), None).await;
        if status == StatusCode::OK {
            opened = body;
            break;
        }
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let root = std::path::PathBuf::from(opened["root"].as_str().unwrap());
    assert!(root.starts_with(host_dir.join("editor").canonicalize().unwrap()));

    let (status, tree) = call(app, &owner, "GET", &at("tree", ""), Body::empty(), None).await;
    assert_eq!(status, StatusCode::OK, "{tree}");
    let names: Vec<_> = tree["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["name"].clone())
        .collect();
    assert_eq!(names, [json!(".git"), json!("README.md")]);
    assert_eq!(tree["entries"][0]["collapsed"], true);
    let (_, index) = call(app, &owner, "GET", &at("index", ""), Body::empty(), None).await;
    assert_eq!(index, json!({"paths": ["README.md"], "truncated": false}));

    let (status, file) = call(
        app,
        &owner,
        "GET",
        &at("file", "README.md"),
        Body::empty(),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{file}");
    assert_eq!(file["content"], "hi\n");
    assert_eq!(file["binary"], false);
    let hash = file["hash"].as_str().unwrap().to_owned();
    let (status, saved) = call(
        app,
        &owner,
        "PUT",
        &at("file", "README.md"),
        Body::from("hello\n"),
        Some(&hash),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert_eq!(saved["size"], 6);
    assert_eq!(
        std::fs::read_to_string(root.join("README.md")).unwrap(),
        "hello\n"
    );
    // A second save from the old version is a conflict, not a clobber.
    let (status, conflict) = call(
        app,
        &owner,
        "PUT",
        &at("file", "README.md"),
        Body::from("stale\n"),
        Some(&hash),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{conflict}");
    assert_eq!(conflict["error"]["code"], "conflict");
    assert_eq!(conflict["current"]["hash"], saved["hash"]);
    assert_eq!(
        std::fs::read_to_string(root.join("README.md")).unwrap(),
        "hello\n"
    );
    let big = Body::from("a".repeat(2 * 1024 * 1024 + 1));
    let (status, _) = call(app, &owner, "PUT", &at("file", "big.txt"), big, Some("x")).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);

    let dir_body = json_body(json!({"kind": "dir"}));
    let (status, body) = call(app, &owner, "POST", &at("file", "docs"), dir_body, None).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let moved = json_body(json!({"from": "README.md", "to": "docs/README.md"}));
    let (status, body) = call(app, &owner, "POST", &at("rename", ""), moved, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    assert!(root.join("docs/README.md").is_file());
    let (status, body) = call(
        app,
        &owner,
        "DELETE",
        &at("file", "docs"),
        Body::empty(),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    assert!(!root.join("docs").exists());

    // The host keeps paths inside the worktree.
    for bad in ["../../../repos", "/etc/passwd", ".git/config"] {
        let (status, body) = call(app, &owner, "GET", &at("file", bad), Body::empty(), None).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{bad}: {body}");
    }

    // Owner only for now: an admin and the deployment token get 403, and a
    // host that isn't attached is 404.
    for token in [admin.as_str(), TOKEN] {
        let (status, body) = call(app, token, "GET", &at("tree", ""), Body::empty(), None).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    }
    let other = format!("{base}/tree?host_id={}&path=", HostId::generate());
    let (status, _) = call(app, &owner, "GET", &other, Body::empty(), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
