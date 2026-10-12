//! Design-canvas previews end to end: the real `sun-host` starts the
//! repo's dev server for the task branch and the base branch, and the kernel
//! proxies it at `/preview/<token>/` through the host socket.
#![allow(clippy::unwrap_used, clippy::expect_used)] // test helpers

use crate::common;

use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{SECRET, TOKEN, kernel};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tower::ServiceExt;

/// A dev server that answers every GET with `page.txt`, the path and the
/// cookie it got, and tries to set a cookie.
const SERVER: &str = r#"import http.server, os
class H(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        body = ("%s %s cookie=%s" % (open("page.txt").read().strip(), self.path, self.headers.get("Cookie"))).encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/html; charset=utf-8" if ".html" in self.path else "text/plain")
        self.send_header("Set-Cookie", "sun_token=evil; Path=/")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)
    def log_message(self, *args):
        pass
http.server.HTTPServer(("127.0.0.1", int(os.environ["PORT"])), H).serve_forever()
"#;

async fn send(
    app: &Router,
    method: &str,
    path: &str,
    auth: bool,
    body: Option<Value>,
) -> (StatusCode, axum::http::HeaderMap, String) {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::COOKIE, format!("sun_token={TOKEN}"));
    if auth {
        request = request.header(header::AUTHORIZATION, format!("Bearer {TOKEN}"));
    }
    let request = request
        .body(body.map_or(Body::empty(), |b| Body::from(b.to_string())))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let (status, headers) = (response.status(), response.headers().clone());
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        headers,
        String::from_utf8_lossy(&bytes).into_owned(),
    )
}

async fn call(app: &Router, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
    let (status, _, text) = send(app, method, path, true, body).await;
    (status, serde_json::from_str(&text).unwrap_or(Value::Null))
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
async fn previews_serve_both_branches_sandboxed_through_the_host() {
    let k = kernel().await;
    let app = &k.app;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/host", listener.local_addr().unwrap());
    tokio::spawn(axum::serve(listener, app.clone()).into_future());

    let dir = tempfile::tempdir().unwrap();
    let origin = dir.path().join("origin");
    std::fs::create_dir_all(&origin).unwrap();
    std::fs::write(origin.join("serve.py"), SERVER).unwrap();
    std::fs::write(origin.join("page.txt"), "base\n").unwrap();
    std::fs::write(origin.join(".sun-preview"), "exec python3 serve.py\n").unwrap();
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

    common::local_repo(&k, sun_core::WorkspaceId::MAIN, origin.to_str().unwrap()).await;
    let yaml = "name: look\nsteps:\n  - id: ok\n    type: approval\n    who: [role:admin]\n";
    let (_, workflow) = call(
        app,
        "POST",
        "/api/v1/workflows",
        Some(json!({ "source_yaml": yaml })),
    )
    .await;
    let (status, task) = call(
        app,
        "POST",
        "/api/v1/tasks",
        // The title names the repo (`origin`): the run works in it.
        Some(json!({ "title": "Restyle origin", "workflow_id": workflow["id"] })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{task}");
    let task_id = task["id"].as_str().unwrap();
    let previews = format!("/api/v1/tasks/{task_id}/previews");

    // Starting needs a member; the host may still be dialling in.
    let (status, _, _) = send(
        app,
        "POST",
        &previews,
        false,
        Some(json!({"side": "after"})),
    )
    .await;
    assert_ne!(status, StatusCode::UNAUTHORIZED, "the cookie signs in");
    let mut after = Value::Null;
    for _ in 0..500 {
        let (status, body) = call(app, "POST", &previews, Some(json!({"side": "after"}))).await;
        if status == StatusCode::OK {
            after = body;
            break;
        }
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(after["side"], "after", "{after}");
    let after_url = after["url"].as_str().unwrap().to_owned();
    assert!(after_url.starts_with("/preview/") && after_url.ends_with('/'));
    // The agent's uncommitted edit shows on the task side.
    std::fs::write(
        host_dir.join("work").join(task_id).join("page.txt"),
        "after",
    )
    .unwrap();

    let (status, headers, body) =
        send(app, "GET", &format!("{after_url}app.js?v=1"), false, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body,
        format!("after {after_url}app.js?v=1 cookie=None"),
        "path kept, cookie stripped"
    );
    let csp: Vec<_> = headers
        .get_all(header::CONTENT_SECURITY_POLICY)
        .iter()
        .collect();
    assert!(
        csp.iter()
            .any(|v| v.to_str().unwrap().starts_with("sandbox "))
    );
    assert!(
        !csp.iter()
            .any(|v| v.to_str().unwrap().contains("allow-same-origin"))
    );
    assert!(headers.get(header::SET_COOKIE).is_none());
    assert_eq!(headers[header::CONTENT_TYPE], "text/plain");
    // HTML answers get the canvas bridge appended (pins); others don't.
    let (_, _, page) = send(app, "GET", &format!("{after_url}index.html"), false, None).await;
    assert!(page.starts_with("after "), "{page}");
    assert!(
        page.ends_with(r#"<script src="/preview_bridge.js"></script>"#),
        "{page}"
    );
    assert!(!body.contains("preview-bridge"));

    let (status, before) = call(app, "POST", &previews, Some(json!({"side": "before"}))).await;
    assert_eq!(status, StatusCode::OK, "{before}");
    let before_url = before["url"].as_str().unwrap().to_owned();
    assert_ne!(before_url, after_url);
    let (_, _, body) = send(app, "GET", &before_url, false, None).await;
    assert!(body.starts_with("base "), "{body}");

    // Idempotent: the same side keeps its URL.
    let (_, again) = call(app, "POST", &previews, Some(json!({"side": "after"}))).await;
    assert_eq!(again["url"], after["url"]);
    let (_, list) = call(app, "GET", &previews, None).await;
    assert_eq!(list, json!([before, after]));

    let (status, headers, _) = send(app, "GET", after_url.trim_end_matches('/'), false, None).await;
    assert_eq!(status, StatusCode::PERMANENT_REDIRECT);
    assert_eq!(headers[header::LOCATION], after_url.as_str());
    let (status, _, _) = send(app, "GET", "/preview/nope/", false, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, body) = call(app, "POST", &previews, Some(json!({"side": "sideways"}))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    let (status, _) = call(app, "DELETE", &previews, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _, _) = send(app, "GET", &after_url, false, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (_, list) = call(app, "GET", &previews, None).await;
    assert_eq!(list, json!([]));
}

#[tokio::test]
async fn pins_are_stored_on_the_task_with_bounded_input() {
    let k = kernel().await;
    let app = &k.app;
    let yaml = "name: look\nsteps:\n  - id: ok\n    type: approval\n    who: [role:admin]\n";
    let (_, workflow) = call(
        app,
        "POST",
        "/api/v1/workflows",
        Some(json!({ "source_yaml": yaml })),
    )
    .await;
    let (_, task) = call(
        app,
        "POST",
        "/api/v1/tasks",
        Some(json!({ "title": "Restyle", "workflow_id": workflow["id"] })),
    )
    .await;
    let pins = format!("/api/v1/tasks/{}/pins", task["id"].as_str().unwrap());
    let target = json!({
        "selector": "main > button:nth-of-type(2)", "tag": "button", "text": "Buy",
        "width": 360, "rect": {"x": 16.0, "y": 120.5, "width": 80.0, "height": 44.0},
        "styles": {"color": "rgb(0, 0, 0)"},
        "source": {"file": "src/Pricing.svelte", "line": 42, "column": 5}
    });

    let (status, pin) = call(
        app,
        "POST",
        &pins,
        Some(json!({ "target": target, "comment": " Make it primary " })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{pin}");
    assert_eq!(pin["status"], "open");
    assert_eq!(pin["comment"], "Make it primary");
    assert_eq!(pin["target"], target);

    let one = format!("{pins}/{}", pin["id"].as_str().unwrap());
    let edit = json!([{"property": "color", "from": "rgb(0, 0, 0)", "to": "#3466FF"}]);
    let (status, changed) = call(
        app,
        "PATCH",
        &one,
        Some(json!({ "status": "queued", "edits": edit })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{changed}");
    assert_eq!(changed["status"], "queued");
    assert_eq!(changed["edits"], edit);
    assert_eq!(changed["comment"], "Make it primary", "kept");
    let (_, list) = call(app, "GET", &pins, None).await;
    assert_eq!(list, json!([changed]));

    // The bridge's report is untrusted: unknown fields and huge values are refused.
    let mut bad = target.clone();
    bad["onclick"] = json!("x");
    let (status, _) = call(app, "POST", &pins, Some(json!({ "target": bad }))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let mut bad = target.clone();
    bad["selector"] = json!("x".repeat(2000));
    let (status, _) = call(app, "POST", &pins, Some(json!({ "target": bad }))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let (status, _) = call(app, "PATCH", &one, Some(json!({ "status": "lost" }))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    let (status, _) = call(app, "DELETE", &one, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = call(app, "PATCH", &one, Some(json!({ "comment": "x" }))).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (_, list) = call(app, "GET", &pins, None).await;
    assert_eq!(list, json!([]));
}
