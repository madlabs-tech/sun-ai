//! Marketplace, buyer side: leasing a task through a fake Sun Cloud,
//! the task-bound join token, and Accept / Dispute.
#![allow(clippy::unwrap_used)]

use crate::common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use futures_util::{SinkExt, StreamExt};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sun_core::model::Task;
use sun_core::{Error, LeaseRequest, ListingFilter, Marketplace, Result, TaskStatus, WorkflowId};
use sun_store::{SettingRepo, TaskRepo};
use tokio_tungstenite::tungstenite::Message;
use tower::ServiceExt;

/// Sun Cloud with one listing; remembers each request.
#[derive(Default)]
struct FakeCloud {
    leased: Mutex<Vec<LeaseRequest>>,
    calls: Mutex<Vec<String>>,
}

#[async_trait]
impl Marketplace for FakeCloud {
    async fn listings(&self, filter: &ListingFilter) -> Result<Value> {
        let query = serde_json::to_string(filter).unwrap();
        self.calls.lock().unwrap().push(format!("listings {query}"));
        Ok(
            json!([{ "id": "lst_1", "model": "claude-sonnet", "vm_template": "ubuntu-24",
            "price_points": 100, "unit": "task", "runtime": "claude-code", "warning": null }]),
        )
    }
    async fn leases(&self) -> Result<Value> {
        Ok(json!([{ "id": "lea_1", "status": "open", "escrow": "held" }]))
    }
    async fn lease(&self, request: &LeaseRequest) -> Result<Value> {
        if request.max_price_points < 100 {
            return Err(Error::NotFound("no listing matches".into()));
        }
        self.leased.lock().unwrap().push(request.clone());
        Ok(json!({ "id": "lea_1", "listing_id": "lst_1", "status": "open", "escrow": "held" }))
    }
    async fn close(&self, id: &str) -> Result<Value> {
        self.calls.lock().unwrap().push(format!("close {id}"));
        Ok(json!({ "id": id, "status": "closed" }))
    }
    async fn accept(&self, id: &str) -> Result<Value> {
        self.calls.lock().unwrap().push(format!("accept {id}"));
        Ok(json!({ "id": id, "escrow": "released" }))
    }
    async fn release(&self, id: &str) -> Result<Value> {
        self.calls.lock().unwrap().push(format!("release {id}"));
        Ok(json!({ "id": id, "escrow": "refunded" }))
    }
    async fn dispute(&self, id: &str, reason: &str) -> Result<Value> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("dispute {id}: {reason}"));
        Ok(json!({ "id": id, "escrow": "disputed", "dispute": reason }))
    }
    async fn my_listings(&self) -> Result<Value> {
        Ok(json!([]))
    }
    async fn set_listing_status(&self, id: &str, status: &str) -> Result<Value> {
        Ok(json!({ "id": id, "status": status }))
    }
    async fn payouts(&self) -> Result<Value> {
        Ok(json!({ "payable": 0, "payouts": [] }))
    }
    async fn request_payout(&self, points: i64, _: &str, _: &str) -> Result<Value> {
        Ok(json!({ "points": points }))
    }
    async fn rate_lease(&self, id: &str, stars: i64, _: Option<&str>) -> Result<Value> {
        Ok(json!({ "id": id, "rating": stars }))
    }
}

async fn call(app: &Router, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::AUTHORIZATION, format!("Bearer {}", common::TOKEN))
        .header(header::CONTENT_TYPE, "application/json")
        .body(body.map_or(Body::empty(), |b| Body::from(b.to_string())))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[tokio::test]
async fn buyer_leases_a_task_its_seller_host_joins_and_accepts_or_disputes() {
    // Outside Sun Cloud: 503.
    let plain = common::kernel().await;
    let (status, _) = call(&plain.app, "GET", "/api/v1/marketplace/listings", None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);

    let cloud = Arc::new(FakeCloud::default());
    let k = common::kernel_with_market(cloud.clone()).await;
    let app = &k.app;
    let (status, listings) = call(app, "GET", "/api/v1/marketplace/listings", None).await;
    assert_eq!(
        (status, &listings[0]["model"]),
        (StatusCode::OK, &json!("claude-sonnet"))
    );

    // A backlog task on a workflow with one repo on GitHub.
    let (_, workflow) = call(
        app,
        "POST",
        "/api/v1/workflows",
        Some(json!({ "source_yaml": "name: w\nsteps:\n  - id: work\n    type: agent\n    prompt: hi\n" })),
    )
    .await;
    let (status, _) = call(
        app,
        "POST",
        &format!("/api/v1/workspaces/{}/repos", sun_core::WorkspaceId::MAIN),
        Some(json!({ "url": "git@github.com:acme/app.git" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let workflow_id = workflow["id"]
        .as_str()
        .unwrap()
        .parse::<WorkflowId>()
        .unwrap();
    // The task hasn't run yet: its title names the repo, so the lease
    // resolves it the way the queue will (the VM reaches the repo's host).
    let task = Task {
        workflow_id: Some(workflow_id),
        status: TaskStatus::Backlog,
        ..Task::new(sun_core::WorkspaceId::MAIN, "Fix the app login")
    };
    let mut tx = k.store.begin().await.unwrap();
    tx.insert_task(&task).await.unwrap();
    tx.commit().await.unwrap();

    let want = |disclosed: bool, max: i64| {
        json!({ "task_id": task.id, "runtime": "claude-code", "unit": "task",
            "max_price_points": max, "accept_disclosure": disclosed })
    };
    let path = "/api/v1/marketplace/leases";
    // The buyer must confirm the disclosure; bad input never reaches the cloud.
    let (status, error) = call(app, "POST", path, Some(want(false, 150))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{error}");
    let (status, _) = call(app, "POST", path, Some(want(true, 0))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let (status, _) = call(app, "POST", path, Some(want(true, 50))).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "Sun Cloud's refusal passes through"
    );
    // The ecosystem policy can keep the marketplace off.
    let policy = "/api/v1/marketplace/policy";
    let (status, _) = call(app, "PUT", policy, Some(json!({ "allow": "none" }))).await;
    assert_eq!(status, StatusCode::OK);
    let (status, error) = call(app, "POST", path, Some(want(true, 150))).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{error}");
    let (status, _) = call(app, "PUT", policy, Some(json!({ "allow": "nope" }))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let only_w = json!({ "allow": "workflows", "workflows": ["w"] });
    let (_, stored) = call(app, "PUT", policy, Some(only_w.clone())).await;
    let (status, read) = call(app, "GET", policy, None).await;
    assert_eq!((status, &read), (StatusCode::OK, &stored));
    // The buyer allows one secret; the rest stay home.
    let mut allowing = want(true, 150);
    allowing["secrets"] = json!(["GITHUB_TOKEN"]);
    let (status, lease) = call(app, "POST", path, Some(allowing)).await;
    assert_eq!(status, StatusCode::CREATED, "{lease}");
    let (status, _) = call(app, "POST", path, Some(want(true, 150))).await;
    assert_eq!(status, StatusCode::CONFLICT, "leased once");

    // Sun Cloud got this kernel's URL and a fresh join token.
    let request = cloud.leased.lock().unwrap()[0].clone();
    assert_eq!(request.kernel_url, "https://buyer.example");
    assert_eq!(request.task_ref, task.id.to_string());
    assert_eq!(request.join_token.len(), 64);
    // The task runs in the listing's reset VM, reaching only its model API and git host.
    let mut tx = k.store.read().await.unwrap();
    let env = tx
        .setting(&format!("task_exec_env.{}", task.id))
        .await
        .unwrap()
        .unwrap();
    drop(tx);
    assert_eq!(
        env,
        json!({ "kind": "vm", "template": "ubuntu-24", "memory_mb": null, "cpus": null,
            "network": ["api.anthropic.com", "github.com"], "keep": false })
    );

    // The seller's host joins with the token: it is bound to the task.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/host", listener.local_addr().unwrap());
    tokio::spawn(axum::serve(listener, app.clone()).into_future());
    let (mut socket, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    let enroll = json!({ "type": "enroll", "token": request.join_token, "name": "seller" });
    socket
        .send(Message::Text(enroll.to_string().into()))
        .await
        .unwrap();
    let enrolled = tokio::time::timeout(Duration::from_secs(10), socket.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let enrolled: Value = serde_json::from_str(enrolled.to_text().unwrap()).unwrap();
    assert_eq!(enrolled["type"], "enrolled");
    let mut tx = k.store.read().await.unwrap();
    let bound = tx
        .setting(&format!("task_market.{}", task.id))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(bound["lease_id"], "lea_1");
    assert_eq!(bound["host_id"], enrolled["host_id"]);
    assert_eq!(
        (&bound["runtime"], &bound["secrets"]),
        (&json!("claude-code"), &json!(["GITHUB_TOKEN"]))
    );
    drop(tx);

    // Filters, the seller side and ratings pass through, validated first.
    let filtered = "/api/v1/marketplace/listings?region=id&capability=gpu&min_rating=4.5";
    let (status, _) = call(app, "GET", filtered, None).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = call(
        app,
        "GET",
        "/api/v1/marketplace/listings?min_rating=9",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        cloud
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|c| c == r#"listings {"region":"id","capability":"gpu","min_rating":4.5}"#)
    );
    let (status, _) = call(app, "GET", "/api/v1/marketplace/listings/mine", None).await;
    assert_eq!(status, StatusCode::OK);
    let pause = json!({ "status": "paused" });
    let (status, paused) = call(
        app,
        "POST",
        "/api/v1/marketplace/listings/lst_1/status",
        Some(pause),
    )
    .await;
    assert_eq!(
        (status, &paused["status"]),
        (StatusCode::OK, &json!("paused"))
    );
    let (status, _) = call(
        app,
        "POST",
        "/api/v1/marketplace/listings/lst_1/status",
        Some(json!({ "status": "gone" })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let payouts = "/api/v1/marketplace/payouts";
    let (status, _) = call(app, "GET", payouts, None).await;
    assert_eq!(status, StatusCode::OK);
    let pay =
        |currency: &str| json!({ "points": 100, "currency": currency, "destination": "acct_1" });
    let (status, _) = call(app, "POST", payouts, Some(pay("eur"))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let (status, _) = call(app, "POST", payouts, Some(pay("usd"))).await;
    assert_eq!(status, StatusCode::CREATED);
    let rating = format!("{path}/lea_1/rating");
    let (status, _) = call(app, "POST", &rating, Some(json!({ "stars": 6 }))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let (status, rated) = call(app, "POST", &rating, Some(json!({ "stars": 4 }))).await;
    assert_eq!((status, &rated["rating"]), (StatusCode::OK, &json!(4)));

    // Accept and dispute go to Sun Cloud; a bad id or empty reason does not.
    let (status, accepted) = call(app, "POST", &format!("{path}/lea_1/accept"), None).await;
    assert_eq!(
        (status, &accepted["escrow"]),
        (StatusCode::OK, &json!("released"))
    );
    let (status, _) = call(app, "POST", &format!("{path}/lea..1/accept"), None).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let dispute = |reason: &str| Some(json!({ "reason": reason }));
    let (status, _) = call(app, "POST", &format!("{path}/lea_1/dispute"), dispute(" ")).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let (status, disputed) = call(
        app,
        "POST",
        &format!("{path}/lea_1/dispute"),
        dispute("no PR was opened"),
    )
    .await;
    assert_eq!(
        (status, &disputed["escrow"]),
        (StatusCode::OK, &json!("disputed"))
    );
    let calls = cloud.calls.lock().unwrap().clone();
    let calls: Vec<&String> = calls
        .iter()
        .filter(|c| !c.starts_with("listings"))
        .collect();
    assert_eq!(calls, ["accept lea_1", "dispute lea_1: no PR was opened"]);
}

/// No `credits` runtime is added from the API for now, with or without Sun
/// Cloud: the deployment sells no credits yet.
#[tokio::test]
async fn credits_runtimes_are_refused_until_credits_are_sold() {
    let credits =
        json!({ "name": "claude-credits", "adapter": "claude-code", "source": "credits" });
    let plain = common::kernel().await;
    let connected = common::kernel_with_market(Arc::new(FakeCloud::default())).await;
    for app in [&plain.app, &connected.app] {
        let (status, body) = call(app, "POST", "/api/v1/runtimes", Some(credits.clone())).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        assert_eq!(
            body["error"]["message"],
            "source: Sun credits are not available on this deployment yet; bring your own key \
             or runtime."
        );
    }
    let (_, runtimes) = call(&connected.app, "GET", "/api/v1/runtimes", None).await;
    assert!(
        runtimes
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["source"] != "credits"),
        "{runtimes}"
    );
}

/// The ecosystem's monthly cap and a task's budget round-trip.
#[tokio::test]
async fn spend_cap_and_task_budget_routes() {
    let k = common::kernel().await;
    let app = &k.app;
    let (status, spend) = call(app, "GET", "/api/v1/settings/budget", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(spend["monthly_usd"], Value::Null);
    assert_eq!(spend["spent_usd"], 0.0);
    let cap = Some(json!({ "monthly_usd": 50.0 }));
    let (status, spend) = call(app, "PUT", "/api/v1/settings/budget", cap).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(spend["monthly_usd"], 50.0);
    let bad = Some(json!({ "monthly_usd": -1 }));
    let (status, _) = call(app, "PUT", "/api/v1/settings/budget", bad).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    let workflow = sun_core::model::Workflow {
        id: WorkflowId::generate(),
        workspace_id: sun_core::WorkspaceId::MAIN,
        name: "w".into(),
    };
    let task = Task {
        title: "t".into(),
        workflow_id: Some(workflow.id),
        status: TaskStatus::Backlog,
        ..sun_core::model::Task::new(sun_core::WorkspaceId::MAIN, "")
    };
    let mut tx = k.store.begin().await.unwrap();
    sun_store::WorkflowRepo::insert_workflow(&mut tx, &workflow)
        .await
        .unwrap();
    tx.insert_task(&task).await.unwrap();
    tx.commit().await.unwrap();
    let path = format!("/api/v1/tasks/{}/budget", task.id);
    let (status, _) = call(app, "PUT", &path, Some(json!({ "cost_usd": 2.5 }))).await;
    assert_eq!(status, StatusCode::OK);
    let (_, budget) = call(app, "GET", &path, None).await;
    assert_eq!(budget["cost_usd"], 2.5);
}
