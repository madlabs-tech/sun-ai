//! Payment webhooks: what the kernel forwards to a stub Sun Cloud (the
//! bytes and headers, nothing parsed), and what it refuses before then.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::Request;
use axum::http::{HeaderMap, StatusCode};
use http_body_util::BodyExt;

use crate::common;

const CLOUD_TOKEN: &str = "cloud-secret-token";

type Seen = Arc<Mutex<Vec<(String, HeaderMap, Bytes)>>>;

/// A cloud that records every call and answers 400 `refused`.
async fn cloud(seen: Seen) -> String {
    let app = Router::new().fallback(move |request: Request| {
        let seen = seen.clone();
        async move {
            let (parts, body) = request.into_parts();
            let bytes = body.collect().await.unwrap().to_bytes();
            let path = parts.uri.path().to_owned();
            seen.lock().unwrap().push((path, parts.headers, bytes));
            (
                StatusCode::BAD_REQUEST,
                [("content-type", "text/plain")],
                "refused",
            )
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await });
    url
}

async fn post(
    app: &Router,
    path: &str,
    headers: &[(&str, &str)],
    body: Vec<u8>,
) -> (StatusCode, Bytes) {
    let mut request = axum::http::Request::post(path);
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let response = common::respond(app, request.body(Body::from(body)).unwrap()).await;
    let status = response.status();
    (
        status,
        response.into_body().collect().await.unwrap().to_bytes(),
    )
}

#[tokio::test]
async fn deliveries_reach_the_cloud_byte_for_byte() {
    let seen = Seen::default();
    let url = cloud(seen.clone()).await;
    let k = common::kernel().await;
    let billing = sun_api::CloudBilling::new(&url, CLOUD_TOKEN).unwrap();
    let app = sun_api::router(k.api.clone().with_cloud(billing));

    // Not JSON, with a NUL and invalid UTF-8: the kernel never parses it.
    let body = b"t=1,{not json\0\xff".to_vec();
    for (provider, header) in [
        ("stripe", "stripe-signature"),
        ("xendit", "x-callback-token"),
    ] {
        let path = format!("/api/v1/payments/webhooks/{provider}");
        let headers = [
            (header, "t=1,v1=abc"),
            ("content-type", "application/json; charset=utf-8"),
            ("x-user-id", "forged"),
            ("authorization", "Bearer forged"),
        ];
        let (status, answer) = post(&app, &path, &headers, body.clone()).await;
        assert_eq!(
            (status, &answer[..]),
            (StatusCode::BAD_REQUEST, &b"refused"[..]),
            "{provider}"
        );
        let (path, headers, bytes) = seen.lock().unwrap().last().cloned().unwrap();
        assert_eq!(path, format!("/webhooks/{provider}"));
        assert_eq!(bytes, body);
        assert_eq!(headers[header], "t=1,v1=abc");
        assert_eq!(headers["content-type"], "application/json; charset=utf-8");
        assert_eq!(headers["authorization"], format!("Bearer {CLOUD_TOKEN}"));
        assert!(headers.get("x-user-id").is_none());
    }

    // Refused before the cloud hears of it.
    let before = seen.lock().unwrap().len();
    let stripe = "/api/v1/payments/webhooks/stripe";
    let (status, _) = post(&app, stripe, &[], b"{}".to_vec()).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "no signature");
    let xendit_signed_as_stripe = [("stripe-signature", "x")];
    let (status, _) = post(
        &app,
        "/api/v1/payments/webhooks/xendit",
        &xendit_signed_as_stripe,
        b"{}".to_vec(),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "the other provider's header"
    );
    let (status, _) = post(
        &app,
        "/api/v1/payments/webhooks/paypal",
        &xendit_signed_as_stripe,
        b"{}".to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let big = vec![b'a'; 1024 * 1024 + 1];
    let (status, _) = post(&app, stripe, &[("stripe-signature", "x")], big).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    let (status, _) = post(
        &app,
        stripe,
        &[("stripe-signature", "x")],
        vec![b'a'; 1024 * 1024],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "1 MiB exactly is taken");
    assert_eq!(seen.lock().unwrap().len(), before + 1);

    // A cloud that can't be reached: 502, so the provider retries.
    let down = sun_api::CloudBilling::new("http://127.0.0.1:9", CLOUD_TOKEN).unwrap();
    let app = sun_api::router(k.api.clone().with_cloud(down));
    let (status, _) = post(&app, stripe, &[("stripe-signature", "x")], b"{}".to_vec()).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);

    // Billing off in sun.yaml: no such route.
    let off = sun_api::Features {
        billing: false,
        ..Default::default()
    };
    let billing = sun_api::CloudBilling::new(&url, CLOUD_TOKEN).unwrap();
    let app = sun_api::router(k.api.clone().with_features(off).with_cloud(billing));
    let (status, _) = post(&app, stripe, &[("stripe-signature", "x")], b"{}".to_vec()).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(seen.lock().unwrap().len(), before + 1);
}
