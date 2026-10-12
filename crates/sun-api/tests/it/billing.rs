//! The subscription proxy against a stub Sun Cloud: the headers and
//! allowlisted bodies it forwards, who may read and write, machine actors
//! refused, the cloud's 404 as `billing_unavailable`, idempotency keys,
//! support tickets, and the cloud's own `seat_count` endpoint.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex, OnceLock};

use axum::Router;
use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::IntoResponse;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sun_core::WorkspaceId;
use sun_core::model::Role;
use sun_store::MemberRepo;

use crate::common::{self, TOKEN};
use crate::workspaces::{call_as, member_token, member_token_in};

const CLOUD_TOKEN: &str = "cloud-secret-token";

/// What the stub saw: method, path with query, headers, body.
#[derive(Clone, Debug)]
struct Seen {
    method: String,
    path: String,
    headers: HeaderMap,
    body: Value,
}

/// A Sun Cloud that records every call and answers `replies[path]`, else
/// 200 `{"ok": true}`. Given the `kernel`, it counts too: a reconcile asks
/// the kernel's `seat_count`, a summary shows it as `used`, and a checkout
/// for fewer seats is 409 `seat_capacity_overcommitted`.
#[derive(Clone, Default)]
struct Cloud {
    seen: Arc<Mutex<Vec<Seen>>>,
    replies: Arc<Mutex<Vec<(String, u16, Value)>>>,
    kernel: Arc<OnceLock<Router>>,
    members: Arc<Mutex<Option<u64>>>,
}

impl Cloud {
    fn reply(&self, path: &str, status: u16, body: Value) {
        self.replies
            .lock()
            .unwrap()
            .push((path.into(), status, body));
    }

    fn last(&self) -> Seen {
        self.seen.lock().unwrap().last().cloned().expect("a call")
    }

    fn count(&self) -> usize {
        self.seen.lock().unwrap().len()
    }

    /// What a counting cloud answers at `path`, if it counts.
    async fn counted(&self, path: &str, body: &Value) -> Option<(u16, Value)> {
        let kernel = self.kernel.get()?;
        let members = *self.members.lock().unwrap();
        if let Some(workspace) = path
            .strip_prefix("/api/v1/subscriptions/")
            .and_then(|p| p.strip_suffix("/seats/reconcile"))
        {
            let ask = axum::http::Request::get(format!(
                "/api/v1/internal/workspaces/{workspace}/seat_count"
            ))
            .header(header::AUTHORIZATION, format!("Bearer {CLOUD_TOKEN}"))
            .body(Body::empty())
            .unwrap();
            let response = common::respond(kernel, ask).await;
            let bytes = response.into_body().collect().await.unwrap().to_bytes();
            let count: Value = serde_json::from_slice(&bytes).unwrap();
            *self.members.lock().unwrap() = count["seat_count"].as_u64();
            return Some((
                200,
                json!({ "seat_capacity": { "used": count["seat_count"] } }),
            ));
        }
        if path.ends_with("/summary") {
            return Some((200, json!({ "seat_capacity": { "used": members } })));
        }
        let short = members.is_some_and(|n| body["seats"].as_u64().is_some_and(|s| s < n));
        if path == "/api/v1/subscriptions/checkout-sessions" && short {
            let refused = json!({ "error": "more members than seats", "code": "seat_capacity_overcommitted" });
            return Some((409, refused));
        }
        None
    }

    async fn start(&self) -> String {
        let cloud = self.clone();
        let app = Router::new().fallback(move |request: Request| {
            let cloud = cloud.clone();
            async move {
                let (parts, body) = request.into_parts();
                let bytes = body.collect().await.unwrap().to_bytes();
                let path = parts.uri.path_and_query().unwrap().to_string();
                let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
                cloud.seen.lock().unwrap().push(Seen {
                    method: parts.method.to_string(),
                    path: path.clone(),
                    headers: parts.headers,
                    body: body.clone(),
                });
                let reply = cloud
                    .replies
                    .lock()
                    .unwrap()
                    .iter()
                    .rev()
                    .find(|(p, _, _)| *p == path)
                    .map(|(_, s, b)| (*s, b.clone()));
                let reply = match reply {
                    Some(reply) => Some(reply),
                    None => cloud.counted(&path, &body).await,
                };
                let (status, body) = reply.unwrap_or((200, json!({ "ok": true })));
                let mut response =
                    (StatusCode::from_u16(status).unwrap(), axum::Json(body)).into_response();
                if status == 429 {
                    response
                        .headers_mut()
                        .insert(header::RETRY_AFTER, "7".parse().unwrap());
                }
                response
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await });
        url
    }
}

/// A kernel connected to `cloud`.
async fn connected(cloud: &Cloud) -> (common::Kernel, Router) {
    let k = common::kernel().await;
    let url = cloud.start().await;
    let billing = sun_api::CloudBilling::new(&url, CLOUD_TOKEN).unwrap();
    let app = sun_api::router(k.api.clone().with_cloud(billing));
    (k, app)
}

async fn member_id(k: &common::Kernel, name: &str) -> String {
    let mut tx = k.store.read().await.unwrap();
    let member = tx
        .member_by_email(&format!("{name}@corp.io"))
        .await
        .unwrap()
        .unwrap();
    member.id.to_string()
}

const MAIN: &str = crate::route_matrix::MAIN;

#[tokio::test]
async fn features_say_billing_only_with_sun_cloud() {
    let k = common::kernel().await;
    let (_, features) = call_as(&k.app, TOKEN, "GET", "/api/v1/features", None).await;
    assert_eq!(features["billing"], false);
    let ann = member_token(&k.store, "ann", Role::Owner).await;
    let summary = format!("/api/v1/subscriptions/{MAIN}/summary");
    let (status, body) = call_as(&k.app, &ann, "GET", &summary, None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["error"]["code"], "billing_unavailable");

    let (k, app) = connected(&Cloud::default()).await;
    let (_, features) = call_as(&app, TOKEN, "GET", "/api/v1/features", None).await;
    assert_eq!(features["billing"], true);
    // sun.yaml turns it off; the routes then answer 404.
    let off = sun_api::Features {
        billing: false,
        ..Default::default()
    };
    let billing = sun_api::CloudBilling::new("http://127.0.0.1:9", CLOUD_TOKEN).unwrap();
    let app = sun_api::router(k.api.clone().with_features(off).with_cloud(billing));
    let (_, features) = call_as(&app, TOKEN, "GET", "/api/v1/features", None).await;
    assert_eq!(features["billing"], false);
    let ann = member_token(&k.store, "ann2", Role::Owner).await;
    let (status, _) = call_as(&app, &ann, "GET", &summary, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn workspace_reads_forward_the_member_and_the_cloud_token() {
    let cloud = Cloud::default();
    let (k, app) = connected(&cloud).await;
    let bob = member_token(&k.store, "bob", Role::Viewer).await;
    let bob_id = member_id(&k, "bob").await;

    for what in ["summary", "prices"] {
        let path = format!("/api/v1/subscriptions/{MAIN}/{what}");
        let (status, body) = call_as(&app, &bob, "GET", &path, None).await;
        assert_eq!((status, body), (StatusCode::OK, json!({ "ok": true })));
        let seen = cloud.last();
        assert_eq!(
            (seen.method.as_str(), seen.path.as_str()),
            ("GET", path.as_str())
        );
        assert_eq!(
            seen.headers["authorization"],
            format!("Bearer {CLOUD_TOKEN}")
        );
        assert_eq!(seen.headers["x-user-id"], bob_id.as_str());
        assert_eq!(seen.headers["x-request-id"].len(), 26, "a ULID");
    }

    // The cloud not knowing the workspace is "unavailable", never Free.
    let path = format!("/api/v1/subscriptions/{MAIN}/summary");
    cloud.reply(&path, 404, json!({ "error": "unknown workspace" }));
    let (status, body) = call_as(&app, &bob, "GET", &path, None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["error"]["code"], "billing_unavailable");
    // A cloud that fails: the same.
    cloud.reply(&path, 500, json!({}));
    let (status, _) = call_as(&app, &bob, "GET", &path, None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);

    // Another workspace's id: 404 before the cloud hears of it.
    let before = cloud.count();
    let other = WorkspaceId::generate();
    for path in [
        format!("/api/v1/subscriptions/{other}/summary"),
        format!("/api/v1/subscriptions/{other}/portal-sessions"),
    ] {
        let method = if path.ends_with("summary") {
            "GET"
        } else {
            "POST"
        };
        let (status, _) = call_as(&app, &bob, method, &path, None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
    }
    assert_eq!(cloud.count(), before);

    // The kernel token is a machine: 403 on every billing route.
    for (method, path) in [
        ("GET", format!("/api/v1/subscriptions/{MAIN}/summary")),
        (
            "POST",
            format!("/api/v1/subscriptions/{MAIN}/portal-sessions"),
        ),
    ] {
        let (status, body) = call_as(&app, TOKEN, method, &path, None).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{path}: {body}");
    }
    assert_eq!(cloud.count(), before);
}

/// An owner of one workspace reaching for another one that exists: every
/// subscription route, and a checkout naming it in the body, is 404 (as if
/// it didn't exist), and the cloud never hears of it.
#[tokio::test]
async fn another_workspace_s_subscription_is_hidden_from_its_non_members() {
    let cloud = Cloud::default();
    let (k, app) = connected(&cloud).await;
    let new = Some(json!({ "name": "Ops" }));
    let (status, ops) = call_as(&app, TOKEN, "POST", "/api/v1/workspaces", new).await;
    assert_eq!(status, StatusCode::CREATED, "{ops}");
    let ops: WorkspaceId = ops["id"].as_str().unwrap().parse().unwrap();
    let olga = member_token_in(&k.store, "olga", &[(ops, Role::Owner)]).await;

    let purchase = json!({ "additional_seats": 1, "expected_current_seats": 1,
        "expected_purchase_version": 0, "accepted_amount_minor": 1000, "currency": "usd",
        "idempotency_key": "p-1" });
    for (method, what, body) in [
        ("GET", "summary", None),
        ("GET", "prices", None),
        ("POST", "portal-sessions", None),
        ("POST", "cancel", None),
        ("POST", "seats/reconcile", None),
        (
            "POST",
            "seats/purchase-preview",
            Some(json!({ "additional_seats": 1 })),
        ),
        ("POST", "seats/purchases", Some(purchase)),
    ] {
        let path = format!("/api/v1/subscriptions/{MAIN}/{what}");
        let (status, value) = call_as(&app, &olga, method, &path, body).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{what}: {value}");
    }
    let checkout = json!({ "workspace_id": MAIN, "plan": "pro", "interval": "month",
        "seats": 1, "idempotency_key": "c-1" });
    let path = "/api/v1/subscriptions/checkout-sessions";
    let (status, _) = call_as(&app, &olga, "POST", path, Some(checkout)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(cloud.count(), 0);

    // Her own workspace's plan she reads and pays.
    let own = format!("/api/v1/subscriptions/{ops}/summary");
    let (status, _) = call_as(&app, &olga, "GET", &own, None).await;
    assert_eq!(status, StatusCode::OK);
    let checkout = json!({ "workspace_id": ops, "plan": "pro", "interval": "month",
        "seats": 1, "idempotency_key": "c-1" });
    let (status, _) = call_as(&app, &olga, "POST", path, Some(checkout)).await;
    assert_eq!(status, StatusCode::OK);
    // Each after a recount of her members.
    assert_eq!(cloud.count(), 4);
    assert_eq!(cloud.last().body["workspace_id"], ops.to_string());
}

#[tokio::test]
async fn workspace_writes_are_for_owners_and_admins_with_allowlisted_bodies() {
    let cloud = Cloud::default();
    let (k, app) = connected(&cloud).await;
    let member = member_token(&k.store, "mia", Role::Member).await;
    let admin = member_token(&k.store, "ada", Role::Admin).await;
    let ada_id = member_id(&k, "ada").await;
    let checkout = |extra: Value| {
        let mut body = json!({ "workspace_id": MAIN, "plan": "pro", "interval": "year",
            "seats": 3, "idempotency_key": "chk-1" });
        body.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        body
    };
    let path = "/api/v1/subscriptions/checkout-sessions";

    // A member reads but doesn't pay; nothing reaches the cloud.
    let (status, _) = call_as(&app, &member, "POST", path, Some(checkout(json!({})))).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    for what in ["portal-sessions", "seats/reconcile"] {
        let sub = format!("/api/v1/subscriptions/{MAIN}/{what}");
        let (status, _) = call_as(&app, &member, "POST", &sub, None).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{what}");
    }
    assert_eq!(cloud.count(), 0);

    // Unknown fields, bad plans and keys are refused here.
    for bad in [
        checkout(json!({ "customer_email": "evil@x.io" })),
        checkout(json!({ "plan": "gold" })),
        checkout(json!({ "interval": "week" })),
        checkout(json!({ "seats": 0 })),
        checkout(json!({ "idempotency_key": "" })),
        checkout(json!({ "idempotency_key": "k".repeat(201) })),
        checkout(json!({ "idempotency_key": "has space" })),
    ] {
        let (status, body) = call_as(&app, &admin, "POST", path, Some(bad.clone())).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{bad}: {body}");
    }
    assert_eq!(cloud.count(), 0);

    // An admin checks out: the body rebuilt, their email added, the key
    // derived (the client's never reaches the cloud) and sent as a header.
    let long_key = "k".repeat(200);
    let (status, _) = call_as(
        &app,
        &admin,
        "POST",
        path,
        Some(checkout(json!({ "idempotency_key": long_key }))),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let seen = cloud.last();
    assert_eq!(seen.path, path);
    assert_eq!(seen.headers["x-user-id"], ada_id.as_str());
    let key = seen.body["idempotency_key"].as_str().unwrap().to_owned();
    assert_eq!(key.len(), 64);
    assert_ne!(key, long_key);
    assert_eq!(seen.headers["idempotency-key"], key.as_str());
    assert_eq!(
        seen.body,
        json!({ "workspace_id": MAIN, "plan": "pro", "interval": "year", "seats": 3,
            "idempotency_key": key, "customer_email": "ada@corp.io" })
    );
    // The same key again: the same derived key (the cloud dedupes).
    call_as(
        &app,
        &admin,
        "POST",
        path,
        Some(checkout(json!({ "idempotency_key": long_key }))),
    )
    .await;
    assert_eq!(cloud.last().headers["idempotency-key"], key.as_str());

    // Seats: preview, then purchase with the previewed numbers.
    let preview = format!("/api/v1/subscriptions/{MAIN}/seats/purchase-preview");
    let (status, _) = call_as(
        &app,
        &admin,
        "POST",
        &preview,
        Some(json!({ "additional_seats": 2 })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(cloud.last().body, json!({ "additional_seats": 2 }));
    assert!(cloud.last().headers.get("idempotency-key").is_none());
    let purchase = format!("/api/v1/subscriptions/{MAIN}/seats/purchases");
    let buy = json!({ "additional_seats": 2, "expected_current_seats": 3,
        "expected_purchase_version": 4, "accepted_amount_minor": 300_000,
        "currency": "IDR", "idempotency_key": "seat-1" });
    let (status, _) = call_as(&app, &admin, "POST", &purchase, Some(buy)).await;
    assert_eq!(status, StatusCode::OK);
    let seen = cloud.last();
    assert_eq!(seen.body["currency"], "idr");
    assert_eq!(seen.body["expected_purchase_version"], 4);
    assert_eq!(
        seen.headers["idempotency-key"],
        seen.body["idempotency_key"].as_str().unwrap()
    );

    // The cloud's refusals pass through with their code.
    cloud.reply(
        &purchase,
        409,
        json!({ "code": "seat_capacity_overcommitted" }),
    );
    let buy = json!({ "additional_seats": 2, "expected_current_seats": 3,
        "expected_purchase_version": "v4", "accepted_amount_minor": 1,
        "currency": "usd", "idempotency_key": "seat-2" });
    let (status, body) = call_as(&app, &admin, "POST", &purchase, Some(buy)).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"]["code"], "seat_capacity_overcommitted");

    // Canceling is the owners' and admins' too; members can't.
    let cancel = format!("/api/v1/subscriptions/{MAIN}/cancel");
    let (status, _) = call_as(&app, &member, "POST", &cancel, None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = call_as(&app, &admin, "POST", &cancel, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(cloud.last().path, cancel);

    // An owner of the workspace pays too.
    let own = member_token(&k.store, "oz", Role::Owner).await;
    let portal = format!("/api/v1/subscriptions/{MAIN}/portal-sessions");
    let (status, _) = call_as(&app, &own, "POST", &portal, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(cloud.last().path, portal);
}

#[tokio::test]
async fn refusals_keep_their_meaning_and_support_is_any_person_s() {
    let cloud = Cloud::default();
    let (k, app) = connected(&cloud).await;
    let admin = member_token(&k.store, "ada", Role::Admin).await;
    let path = "/api/v1/subscriptions/checkout-sessions";
    let checkout = |key: &str| {
        json!({ "workspace_id": MAIN, "plan": "business", "interval": "month",
            "seats": 5, "idempotency_key": key })
    };
    cloud.reply(path, 402, json!({ "code": "insufficient_balance" }));
    let (status, body) = call_as(&app, &admin, "POST", path, Some(checkout("a"))).await;
    assert_eq!(status, StatusCode::PAYMENT_REQUIRED);
    assert_eq!(body["error"]["code"], "insufficient_balance");
    cloud.reply(
        path,
        409,
        json!({ "error": { "code": "idempotency_conflict" } }),
    );
    let (status, body) = call_as(&app, &admin, "POST", path, Some(checkout("b"))).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"]["code"], "idempotency_conflict");
    // The cloud refusing our own token is never shown as the person's 401.
    cloud.reply(path, 401, json!({}));
    let (status, body) = call_as(&app, &admin, "POST", path, Some(checkout("c"))).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["error"]["code"], "billing_unavailable");

    // Cancel or portal where the cloud has no subscription: 404, said so;
    // any other failure there is still unavailable.
    for what in ["cancel", "portal-sessions"] {
        let path = format!("/api/v1/subscriptions/{MAIN}/{what}");
        cloud.reply(&path, 404, json!({ "error": "no subscription" }));
        let (status, body) = call_as(&app, &admin, "POST", &path, None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{what}: {body}");
        assert_eq!(body["error"]["code"], "no_subscription");
        assert_eq!(
            body["error"]["message"],
            "This workspace has no subscription."
        );
        cloud.reply(&path, 500, json!({}));
        let (status, body) = call_as(&app, &admin, "POST", &path, None).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{what}");
        assert_eq!(body["error"]["code"], "billing_unavailable");
    }

    // 429 keeps its Retry-After.
    let summary = format!("/api/v1/subscriptions/{MAIN}/summary");
    cloud.reply(&summary, 429, json!({}));
    let request = axum::http::Request::get(&summary)
        .header(header::AUTHORIZATION, format!("Bearer {admin}"))
        .body(Body::empty())
        .unwrap();
    let response = common::respond(&app, request).await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(response.headers()[header::RETRY_AFTER], "7");

    // Support is any signed-in person's, at the cloud's `/account/support`,
    // as that person; the kernel token is a machine.
    let before = cloud.count();
    let (status, _) = call_as(&app, TOKEN, "GET", "/api/v1/support", None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let ticket = json!({ "subject": "Help", "body": "It broke." });
    let (status, _) = call_as(&app, TOKEN, "POST", "/api/v1/support", Some(ticket.clone())).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(cloud.count(), before);
    let lone = member_token_in(&k.store, "lou", &[]).await;
    let (status, _) = call_as(&app, &lone, "GET", "/api/v1/support", None).await;
    assert_eq!(status, StatusCode::OK);
    let seen = cloud.last();
    assert_eq!(seen.path, "/account/support");
    assert_eq!(
        seen.headers["x-user-id"],
        member_id(&k, "lou").await.as_str()
    );
    let (status, _) = call_as(&app, &lone, "POST", "/api/v1/support", Some(ticket.clone())).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(cloud.last().body, ticket);
    // One who pays for no plan: the cloud's 403 and its reason.
    let why = "support from people comes with the Pro and Business plans";
    cloud.reply(
        "/account/support",
        403,
        json!({ "error": why, "code": "forbidden" }),
    );
    let (status, body) = call_as(&app, &lone, "POST", "/api/v1/support", Some(ticket)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["error"]["message"], why);
}

/// The cloud recounts members before a checkout or a seat purchase, and
/// before an owner's or admin's summary: so a checkout for fewer seats than
/// people is refused, and the page shows the real `used`.
#[tokio::test]
async fn checkouts_and_seat_purchases_follow_a_recount() {
    let cloud = Cloud::default();
    let (k, app) = connected(&cloud).await;
    cloud.kernel.set(app.clone()).unwrap();
    let admin = member_token(&k.store, "ada", Role::Admin).await;
    let viewer = member_token(&k.store, "vic", Role::Viewer).await;
    member_token(&k.store, "mia", Role::Member).await;
    let members = main_members(&k).await;
    assert!(members >= 3, "{members}");

    let checkout = json!({ "workspace_id": MAIN, "plan": "pro", "interval": "month",
        "seats": 1, "idempotency_key": "c-1" });
    let path = "/api/v1/subscriptions/checkout-sessions";
    let (status, body) = call_as(&app, &admin, "POST", path, Some(checkout)).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"]["code"], "seat_capacity_overcommitted");
    let reconcile = format!("/api/v1/subscriptions/{MAIN}/seats/reconcile");
    let paths: Vec<String> = cloud
        .seen
        .lock()
        .unwrap()
        .iter()
        .map(|s| s.path.clone())
        .collect();
    assert_eq!(paths, [reconcile.as_str(), path]);
    let ada_id = member_id(&k, "ada").await;
    let first = cloud.seen.lock().unwrap()[0].clone();
    assert_eq!(first.headers["x-user-id"], ada_id.as_str());

    // The admin's summary is recounted first; a viewer's is read as is.
    let summary = format!("/api/v1/subscriptions/{MAIN}/summary");
    let (status, body) = call_as(&app, &admin, "GET", &summary, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["seat_capacity"]["used"], members);
    let before = cloud.count();
    call_as(&app, &viewer, "GET", &summary, None).await;
    assert_eq!(cloud.count(), before + 1);
    assert_eq!(cloud.last().path, summary);

    // A recount that fails stops the checkout and the purchase there; the
    // summary still shows the last count.
    cloud.reply(
        &reconcile,
        503,
        json!({ "error": "x", "code": "kernel_unavailable" }),
    );
    let checkout = json!({ "workspace_id": MAIN, "plan": "pro", "interval": "month",
        "seats": 9, "idempotency_key": "c-2" });
    let (status, body) = call_as(&app, &admin, "POST", path, Some(checkout)).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["error"]["code"], "kernel_unavailable");
    let buy = json!({ "additional_seats": 2, "expected_current_seats": 3,
        "expected_purchase_version": 4, "accepted_amount_minor": 1, "currency": "usd",
        "idempotency_key": "s-1" });
    let purchase = format!("/api/v1/subscriptions/{MAIN}/seats/purchases");
    let (status, _) = call_as(&app, &admin, "POST", &purchase, Some(buy)).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(cloud.last().path, reconcile);
    let (status, body) = call_as(&app, &admin, "GET", &summary, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["seat_capacity"]["used"], members);
}

/// `?currency=` on summary and prices, and `currency` in a checkout, pass
/// through when they are `usd` or `idr`; anything else stops here.
#[tokio::test]
async fn currencies_pass_through_from_the_allowlist() {
    let cloud = Cloud::default();
    let (k, app) = connected(&cloud).await;
    let bob = member_token(&k.store, "bob", Role::Viewer).await;
    let admin = member_token(&k.store, "ada", Role::Admin).await;
    for what in ["summary", "prices"] {
        let base = format!("/api/v1/subscriptions/{MAIN}/{what}");
        let (status, _) = call_as(&app, &bob, "GET", &format!("{base}?currency=IDR"), None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(cloud.last().path, format!("{base}?currency=idr"));
        call_as(&app, &bob, "GET", &format!("{base}?currency=usd&x=1"), None).await;
        assert_eq!(cloud.last().path, format!("{base}?currency=usd"));
        call_as(&app, &bob, "GET", &base, None).await;
        assert_eq!(cloud.last().path, base);
        let before = cloud.count();
        for bad in ["eur", "usd%26plan%3Dfree", ""] {
            let path = format!("{base}?currency={bad}");
            let (status, body) = call_as(&app, &bob, "GET", &path, None).await;
            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{bad}: {body}");
        }
        assert_eq!(cloud.count(), before);
    }
    // An admin's summary in a currency: the recount, then the summary in it.
    let base = format!("/api/v1/subscriptions/{MAIN}/summary");
    call_as(&app, &admin, "GET", &format!("{base}?currency=idr"), None).await;
    assert_eq!(cloud.last().path, format!("{base}?currency=idr"));

    let path = "/api/v1/subscriptions/checkout-sessions";
    let checkout = |currency: Value| {
        json!({ "workspace_id": MAIN, "plan": "pro", "interval": "month", "seats": 5,
            "idempotency_key": "c-1", "currency": currency })
    };
    let (status, _) = call_as(&app, &admin, "POST", path, Some(checkout(json!("IDR")))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(cloud.last().body["currency"], "idr");
    let before = cloud.count();
    for bad in [json!("eur"), json!(""), json!(5)] {
        let (status, body) = call_as(&app, &admin, "POST", path, Some(checkout(bad.clone()))).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{bad}: {body}");
    }
    assert_eq!(cloud.count(), before);
}

#[tokio::test]
async fn seat_count_answers_only_sun_cloud() {
    let (k, app) = connected(&Cloud::default()).await;
    member_token(&k.store, "bob", Role::Member).await;
    let path = format!("/api/v1/internal/workspaces/{MAIN}/seat_count");
    let ask = |auth: Option<String>| {
        let mut request = axum::http::Request::get(&path);
        if let Some(auth) = auth {
            request = request.header(header::AUTHORIZATION, auth);
        }
        common::respond(&app, request.body(Body::empty()).unwrap())
    };
    for auth in [
        None,
        Some(format!("Bearer {TOKEN}")),
        Some("Bearer cloud-secret-tokeX".to_owned()),
        Some(CLOUD_TOKEN.to_owned()),
    ] {
        assert_eq!(
            ask(auth.clone()).await.status(),
            StatusCode::UNAUTHORIZED,
            "{auth:?}"
        );
    }
    let response = ask(Some(format!("Bearer {CLOUD_TOKEN}"))).await;
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["seat_count"], main_members(&k).await);
    assert_eq!(body.as_object().unwrap().len(), 1);

    // Not connected to Sun Cloud: nobody may ask.
    let request = axum::http::Request::get(&path)
        .header(header::AUTHORIZATION, format!("Bearer {CLOUD_TOKEN}"))
        .body(Body::empty())
        .unwrap();
    let response = common::respond(&k.app, request).await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

async fn main_members(k: &common::Kernel) -> usize {
    use sun_store::MembershipRepo;
    let mut tx = k.store.read().await.unwrap();
    tx.members_of(WorkspaceId::MAIN).await.unwrap().len()
}
