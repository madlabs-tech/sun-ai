//! Live device view: the browser socket relays the host's
//! screen pictures and forwards input, against a scripted fake host.
#![allow(clippy::unwrap_used, clippy::expect_used)] // test helpers

use crate::common;

use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, header};
use common::{SECRET, TOKEN, kernel};
use futures_util::{SinkExt, StreamExt};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tower::ServiceExt;

async fn call(app: &Router, method: &str, path: &str, body: Value) -> Value {
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
        .body(Body::from(body.to_string()))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn next(socket: &mut Socket) -> Message {
    tokio::time::timeout(Duration::from_secs(10), socket.next())
        .await
        .expect("socket went quiet")
        .expect("socket closed")
        .unwrap()
}

fn json_of(message: Message) -> Value {
    match message {
        Message::Text(text) => serde_json::from_str(&text).unwrap(),
        other => panic!("expected text, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn device_view_relays_screens_and_input_through_the_host() {
    let k = kernel().await;
    let app = &k.app;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(axum::serve(listener, app.clone()).into_future());

    let yaml = "name: look\nsteps:\n  - id: ok\n    type: approval\n    who: [role:admin]\n";
    let workflow = call(
        app,
        "POST",
        "/api/v1/workflows",
        json!({ "source_yaml": yaml }),
    )
    .await;
    let task = call(
        app,
        "POST",
        "/api/v1/tasks",
        json!({ "title": "Login screen", "workflow_id": workflow["id"] }),
    )
    .await;
    let task_id = task["id"].as_str().unwrap().to_owned();

    // The fake host: no device until the test says so, then one Android
    // picture per request; it reports every input it gets.
    let key: String = Sha256::digest(format!("{SECRET}:{}", k.host).as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let (mut host, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/host"))
        .await
        .unwrap();
    let hello = json!({
        "type": "hello", "host_id": k.host, "host_key": key, "protocol": "2.0",
        "capabilities": { "os": "linux", "arch": "x86_64", "cpus": 4, "runtimes": [] },
        "last_ack": {},
    });
    host.send(Message::Text(hello.to_string().into()))
        .await
        .unwrap();
    let (booted, mut device_up) = tokio::sync::watch::channel(false);
    let (inputs, mut got_input) = tokio::sync::mpsc::unbounded_channel();
    // Video: two access units at the start of each stream, until the host
    // can't stream any more.
    let (video_stops, video_stopped) = tokio::sync::watch::channel(false);
    let mut streams = std::collections::HashSet::new();
    tokio::spawn(async move {
        while let Some(Ok(message)) = host.next().await {
            // The kernel's liveness pings are answered by tungstenite.
            let Message::Text(text) = message else {
                continue;
            };
            let frame: Value = serde_json::from_str(&text).unwrap();
            let body = match frame["type"].as_str().unwrap() {
                "device.video" if *video_stopped.borrow() => {
                    json!({ "kind": "error", "code": "unavailable", "message": "no idb" })
                }
                "device.video" => {
                    let frames = match streams.insert(frame["stream"].to_string()) {
                        true => json!([{ "data": "AAAAAWdr" }, { "data": "AAAAAUFw" }]),
                        false => {
                            tokio::time::sleep(Duration::from_millis(50)).await;
                            json!([])
                        }
                    };
                    json!({ "kind": "video", "video": {
                        "device": "Pixel_7", "platform": "android", "input": true,
                        "frames": frames } })
                }
                "device.screen" if !*device_up.borrow_and_update() => {
                    json!({ "kind": "error", "code": "not_found", "message": "no device" })
                }
                "device.screen" => json!({ "kind": "screen", "screen": {
                    "device": "Pixel_7", "platform": "android", "input": true,
                    "mime": "image/png", "image": "UE5H" } }),
                "device.input" => {
                    inputs.send(frame["input"].clone()).unwrap();
                    json!({ "kind": "done" })
                }
                _ => continue,
            };
            let reply = json!({ "type": "reply", "id": frame["id"], "body": body });
            if host
                .send(Message::Text(reply.to_string().into()))
                .await
                .is_err()
            {
                break;
            }
        }
    });

    let url = format!("ws://{addr}/api/v1/tasks/{task_id}/device");
    let open = |origin: &str| {
        let mut request = url.as_str().into_client_request().unwrap();
        let headers = request.headers_mut();
        headers.insert(
            header::AUTHORIZATION,
            format!("Bearer {TOKEN}").parse().unwrap(),
        );
        headers.insert(header::ORIGIN, origin.parse().unwrap());
        tokio_tungstenite::connect_async(request)
    };

    // Another site can't open it (cross-site WebSocket hijacking).
    let refused = open("https://evil.example").await.unwrap_err();
    assert!(refused.to_string().contains("403"), "{refused}");

    let (mut view, _) = open(&format!("http://{addr}")).await.unwrap();
    // The host may still be dialling in: offline hosts also read as idle.
    assert_eq!(json_of(next(&mut view).await)["kind"], "idle");
    booted.send(true).unwrap();
    let live = json_of(next(&mut view).await);
    assert_eq!(
        live,
        json!({ "kind": "live", "device": "Pixel_7", "platform": "android",
                "input": true, "mime": "image/png" })
    );
    assert_eq!(
        next(&mut view).await,
        Message::Binary(b"PNG".to_vec().into())
    );

    let tap = json!({ "kind": "tap", "x": 540, "y": 1200 });
    view.send(Message::Text(tap.to_string().into()))
        .await
        .unwrap();
    assert_eq!(got_input.recv().await.unwrap(), tap);

    // Bad input is answered on the socket and never reaches the host.
    view.send(Message::Text(r#"{"kind":"swipe"}"#.into()))
        .await
        .unwrap();
    loop {
        if let Message::Text(text) = next(&mut view).await {
            let failed: Value = serde_json::from_str(&text).unwrap();
            assert_eq!(failed["kind"], "input_failed", "{failed}");
            break;
        }
    }
    assert!(got_input.try_recv().is_err());

    // A browser with WebCodecs gets H.264 access units, one per message.
    let (mut video, _) = {
        let mut request = format!("{url}?video=h264").into_client_request().unwrap();
        request.headers_mut().insert(
            header::AUTHORIZATION,
            format!("Bearer {TOKEN}").parse().unwrap(),
        );
        tokio_tungstenite::connect_async(request).await.unwrap()
    };
    assert_eq!(
        json_of(next(&mut video).await),
        json!({ "kind": "live", "device": "Pixel_7", "platform": "android",
                "input": true, "mime": "video/h264" })
    );
    for unit in [&b"\0\0\0\x01gk"[..], b"\0\0\0\x01Ap"] {
        assert_eq!(
            next(&mut video).await,
            Message::Binary(unit.to_vec().into())
        );
    }
    // The host stops streaming: one fresh stream is tried, then pictures.
    video_stops.send(true).unwrap();
    assert_eq!(json_of(next(&mut video).await)["mime"], "image/png");
    assert_eq!(
        next(&mut video).await,
        Message::Binary(b"PNG".to_vec().into())
    );

    // An unknown task is a 404 before the upgrade.
    let mut request = format!("ws://{addr}/api/v1/tasks/01JAR00000000000000000000Z/device")
        .into_client_request()
        .unwrap();
    request.headers_mut().insert(
        header::AUTHORIZATION,
        format!("Bearer {TOKEN}").parse().unwrap(),
    );
    let missing = tokio_tungstenite::connect_async(request).await.unwrap_err();
    assert!(missing.to_string().contains("404"), "{missing}");
}
