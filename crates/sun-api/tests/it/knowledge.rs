//! Knowledge over REST: connector sources synced page by page with a
//! persisted cursor and tombstones, uploads, chunks with citations, search,
//! task context preview, embedding settings and the secret sweep.
#![allow(clippy::unwrap_used)]

use crate::common;

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{TOKEN, kernel_with_mods};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sun_core::{ModCall, ModLink, Result};
use tower::ServiceExt;

async fn call(app: &Router, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
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

fn issue(key: &str, text: &str) -> Value {
    json!({
        "external_id": key, "title": format!("{key} issue"), "version": "1",
        "updated_at": "2026-10-01T08:00:00.000Z",
        "content": { "markdown": text }, "meta": { "status": "Done" },
    })
}

/// mod-sync's `sync_jira`: two pages, then (from the stored cursor) a tombstone.
#[derive(Default)]
struct Jira {
    calls: Mutex<Vec<Value>>,
}

#[async_trait]
impl ModLink for Jira {
    async fn call(&self, call: ModCall) -> Result<Value> {
        assert_eq!(
            (call.mod_name.as_str(), call.method.as_str()),
            ("sync", "sync_jira")
        );
        self.calls.lock().unwrap().push(call.params.clone());
        let p = &call.params;
        Ok(match (p["since"].as_str(), p["page_token"].as_str()) {
            (None, None) => json!({
                "docs": [issue("PROJ-1", "Rotate the gateway keys"), issue("PROJ-2", "Fix the invoice export")],
                "next_page_token": "p2", "cursor": "2026-10-01T08:00:00Z",
            }),
            (None, Some("p2")) => json!({
                "docs": [issue("PROJ-3", "Rename the billing tables")],
                "next_page_token": null, "cursor": "2026-10-02T08:00:00Z",
            }),
            (Some("2026-10-02T08:00:00Z"), None) => json!({
                "docs": [], "deleted": ["PROJ-2"], "next_page_token": null, "cursor": null,
            }),
            other => panic!("unexpected sync {other:?}"),
        })
    }
}

#[tokio::test]
async fn knowledge_sources_sync_upload_search_preview_and_settings() {
    let jira = Arc::new(Jira::default());
    let kernel = kernel_with_mods(jira.clone()).await;
    let app = &kernel.app;

    // A connector source needs its param; then sync follows every page.
    let (status, _) = call(
        app,
        "POST",
        "/api/v1/knowledge/sources",
        Some(json!({"kind": "jira", "name": "Tickets"})),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let (status, jira_source) = call(
        app,
        "POST",
        "/api/v1/knowledge/sources",
        Some(json!({"kind": "jira", "name": "Tickets", "settings": {"project": "PROJ"}})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{jira_source}");
    let jira_id = jira_source["id"].as_str().unwrap().to_owned();
    let reindex = format!("/api/v1/knowledge/sources/{jira_id}/reindex");
    let (status, report) = call(app, "POST", &reindex, None).await;
    assert_eq!(status, StatusCode::OK, "{report}");
    assert_eq!(report["indexed"], 3);
    let (_, sources) = call(app, "GET", "/api/v1/knowledge/sources", None).await;
    assert_eq!(sources[0]["docs"], 3);
    assert_eq!(sources[0]["cursor"], "2026-10-02T08:00:00Z");
    assert_eq!(sources[0]["settings"], json!({"project": "PROJ"}));
    assert_eq!(sources[0]["last_sync"]["report"]["indexed"], 3);

    // The next sync starts at the cursor; the tombstone retires PROJ-2.
    let (_, report) = call(app, "POST", &reindex, None).await;
    assert_eq!(
        (report["indexed"].clone(), report["retired"].clone()),
        (json!(0), json!(1))
    );
    assert_eq!(jira.calls.lock().unwrap()[2]["project"], "PROJ");
    let (_, docs) = call(
        app,
        "GET",
        &format!("/api/v1/knowledge/sources/{jira_id}/docs"),
        None,
    )
    .await;
    let ids: Vec<&str> = docs
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["external_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["PROJ-1", "PROJ-3"]);

    // Uploads: a source without a connector, browsed by chunk with citations.
    let (_, upload) = call(
        app,
        "POST",
        "/api/v1/knowledge/sources",
        Some(json!({"kind": "upload", "name": "Runbooks"})),
    )
    .await;
    let upload_id = upload["id"].as_str().unwrap().to_owned();
    let (status, _) = call(
        app,
        "POST",
        &format!("/api/v1/knowledge/sources/{upload_id}/reindex"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let runbook = json!({
        "external_id": "ops/deploy.md", "title": "Deploy runbook", "version": "v2",
        "content": { "markdown": "# Deploy\n\n## Rollback\n\nRoll back with blue-green-standby-switch-42 then page the gateway owner." },
    });
    let (status, report) = call(
        app,
        "POST",
        &format!("/api/v1/knowledge/sources/{upload_id}/docs"),
        Some(json!({"docs": [runbook]})),
    )
    .await;
    assert_eq!(
        (status, report["indexed"].clone()),
        (StatusCode::OK, json!(1)),
        "{report}"
    );
    let (_, docs) = call(
        app,
        "GET",
        &format!("/api/v1/knowledge/sources/{upload_id}/docs"),
        None,
    )
    .await;
    let (_, doc) = call(
        app,
        "GET",
        &format!("/api/v1/knowledge/docs/{}", docs[0]["id"].as_str().unwrap()),
        None,
    )
    .await;
    assert_eq!(
        doc["chunks"][0]["citation"],
        "Deploy runbook > Deploy > Rollback · v2"
    );
    let (_, hits) = call(app, "GET", "/api/v1/knowledge/search?q=gateway", None).await;
    assert_eq!(hits.as_array().unwrap().len(), 2, "{hits}");
    let (_, hits) = call(
        app,
        "GET",
        &format!("/api/v1/knowledge/search?q=gateway&sources={upload_id}"),
        None,
    )
    .await;
    assert_eq!(hits.as_array().unwrap().len(), 1);
    assert!(
        hits[0]["citation"]
            .as_str()
            .unwrap()
            .starts_with("Deploy runbook > Deploy > Rollback")
    );

    // Task context: sources and budget, and the pack it would get.
    let (_, workflow) = call(app, "POST", "/api/v1/workflows", Some(json!({"source_yaml": "name: w\nsteps:\n  - id: work\n    type: agent\n    prompt: hi\n"}))).await;
    let (_, task) = call(
        app,
        "POST",
        "/api/v1/tasks",
        Some(json!({"title": "Rotate gateway keys", "body": "", "workflow_id": workflow["id"]})),
    )
    .await;
    let context = format!("/api/v1/tasks/{}/context", task["id"].as_str().unwrap());
    let (_, all) = call(
        app,
        "POST",
        &format!("{context}/preview"),
        Some(json!({"sources": null, "budget_tokens": null})),
    )
    .await;
    let knowledge = |pack: &Value| {
        pack["items"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|i| i["priority"] == 4)
            .count()
    };
    assert_eq!(knowledge(&all), 2, "{all}");
    let (_, narrow) = call(
        app,
        "POST",
        &format!("{context}/preview"),
        Some(json!({"sources": [upload_id], "budget_tokens": null})),
    )
    .await;
    assert_eq!(knowledge(&narrow), 1);
    // The narrow selection still gets every relevant source proposed.
    let suggested = narrow["suggested_sources"].as_array().unwrap();
    assert_eq!(suggested.len(), 2, "{narrow}");
    assert!(suggested.iter().any(|s| s["selected"] == false));
    assert_eq!(narrow["cost_estimate_usd"], Value::Null, "no usage yet");
    assert_eq!(narrow["blocked"], Value::Null);
    assert!(narrow["token_estimate"].as_u64() < all["token_estimate"].as_u64());
    let (_, capped) = call(
        app,
        "POST",
        &format!("{context}/preview"),
        Some(json!({"sources": null, "budget_tokens": 1})),
    )
    .await;
    assert!(
        capped["items"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|i| i["priority"] == 4)
            .all(|i| i["included"] == false)
    );
    let (status, _) = call(
        app,
        "PUT",
        &context,
        Some(json!({"sources": ["nope"], "budget_tokens": 900})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = call(
        app,
        "PUT",
        &context,
        Some(
            json!({"sources": [upload_id], "budget_tokens": 900, "pinned": [" No new deps. ", ""]}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, saved) = call(app, "GET", &context, None).await;
    assert_eq!(
        saved,
        json!({"sources": [upload_id], "budget_tokens": 900, "pinned": ["No new deps."], "block_over_budget": false})
    );
    // Binary files are converted on upload; a bad one is skipped.
    let docx = sun_context::knowledge::fixtures::docx(&[
        ("Heading1", "Escalation"),
        ("", "Page the on-call lead."),
    ]);
    let (status, report) = call(
        app,
        "POST",
        &format!("/api/v1/knowledge/sources/{upload_id}/docs"),
        Some(json!({"files": [
            {"external_id": "ops/escalation.docx", "title": "Escalation", "version": "v1",
             "content_base64": base64::Engine::encode(&base64::engine::general_purpose::STANDARD, docx)},
            {"external_id": "ops/broken.pdf", "title": "Broken", "version": "v1",
             "content_base64": "bm90IGEgcGRm"},
        ]})),
    )
    .await;
    assert_eq!(
        (status, report["indexed"].clone()),
        (StatusCode::OK, json!(1)),
        "{report}"
    );
    assert!(
        report["skipped"][0]
            .as_str()
            .unwrap()
            .contains("broken.pdf"),
        "{report}"
    );
    let too_many = vec!["x"; 21];
    let (status, _) = call(app, "PUT", &context, Some(json!({"pinned": too_many}))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    // Embedding settings: checked, key write-only; a new key is swept from the index.
    let (_, embeddings) = call(app, "GET", "/api/v1/settings/embeddings", None).await;
    assert_eq!(
        (
            embeddings["provider"].clone(),
            embeddings["has_api_key"].clone()
        ),
        (json!("none"), json!(false))
    );
    let (status, _) = call(
        app,
        "PUT",
        "/api/v1/settings/embeddings",
        Some(json!({"provider": "ollama", "base_url": "", "model": "bge-m3"})),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let (status, embeddings) = call(
        app,
        "PUT",
        "/api/v1/settings/embeddings",
        Some(json!({"provider": "none", "api_key": "blue-green-standby-switch-42"})),
    )
    .await;
    assert_eq!(
        (status, embeddings["has_api_key"].clone()),
        (StatusCode::OK, json!(true)),
        "{embeddings}"
    );
    assert!(embeddings.get("api_key").is_none());
    let (_, hits) = call(app, "GET", "/api/v1/knowledge/search?q=standby", None).await;
    assert!(hits.as_array().unwrap().is_empty(), "{hits}");

    // Deleting a source takes its docs along.
    let (status, _) = call(
        app,
        "DELETE",
        &format!("/api/v1/knowledge/sources/{jira_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = call(
        app,
        "GET",
        &format!("/api/v1/knowledge/sources/{jira_id}/docs"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
