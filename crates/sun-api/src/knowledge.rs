//! Knowledge (L4): `/api/v1/knowledge/**` (sources, uploads, re-index,
//! docs, chunks with citations, search), `/api/v1/settings/embeddings` and
//! each task's context selection and pack preview (`/tasks/{id}/context`).

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::{Extension, Json};
use serde::Deserialize;
use serde_json::{Value, json};
use sun_context::knowledge::{SourceDoc, SourceFile};
use sun_core::{TaskId, WorkspaceId};
use sun_engine::WorkspaceItem;
use sun_engine::{DocSummary, EmbeddingProvider, EmbeddingSettings, SourceSummary, TaskContext};

use crate::Api;
use crate::auth::Caller;
use crate::error::{ApiError, ApiResult};
use crate::routes::{Body, WorkspaceFilter, id, workspace_param};

const MAX_NAME: usize = 200;
const MAX_QUERY: usize = 1000;
const MAX_DOCS: usize = 500;
const MAX_SOURCES: usize = 100;
/// Pinned constraints per task, and characters per constraint (they ride in
/// every pack's P0, which should stay ≤ 2k tokens).
const MAX_PINNED: usize = 20;
const MAX_PINNED_CHARS: usize = 500;
/// Search hits returned.
const HITS: usize = 20;

fn source_view(s: SourceSummary) -> Value {
    let mut settings = s.source.config;
    let (cursor, last_sync) = match settings.as_object_mut() {
        Some(map) => {
            map.remove("exported");
            (map.remove("cursor"), map.remove("last_sync"))
        }
        None => (None, None),
    };
    json!({
        "id": s.source.id,
        "kind": s.source.kind,
        "name": s.source.name,
        "settings": settings,
        "cursor": cursor,
        "last_sync": last_sync,
        "docs": s.docs,
        "chunks": s.chunks,
        "last_indexed_at": s.last_indexed_at,
    })
}

fn doc_view(d: DocSummary) -> Value {
    json!({
        "id": d.id,
        "source_id": d.source_id,
        "external_id": d.external_id,
        "title": d.title,
        "version": d.version,
        "url": d.url,
        "updated_at": d.updated_at,
        "fetched_at": d.fetched_at,
        "chunks": d.chunks,
    })
}

/// `GET /knowledge/sources?workspace=`: that workspace's sources; every source
/// without `workspace`.
pub(crate) async fn list_sources(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Query(filter): Query<WorkspaceFilter>,
) -> ApiResult<Json<Vec<Value>>> {
    let workspace = workspace_param(filter.workspace.as_deref())?;
    let sources = match caller.workspaces.as_ref().filter(|_| workspace.is_none()) {
        None => api.engine.knowledge_sources(workspace).await?,
        // Every source on a workspace the caller sees, once.
        Some(workspaces) => {
            let mut sources = Vec::new();
            for workspace in workspaces {
                sources.extend(api.engine.knowledge_sources(Some(*workspace)).await?);
            }
            sources.sort_by(|a, b| a.source.id.cmp(&b.source.id));
            sources.dedup_by(|a, b| a.source.id == b.source.id);
            sources
        }
    };
    Ok(Json(sources.into_iter().map(source_view).collect()))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NewSource {
    kind: String,
    name: String,
    #[serde(default)]
    settings: Value,
    /// The workspace it goes on (default Main).
    workspace_id: Option<String>,
}

/// `POST /knowledge/sources {kind, name, settings, workspace_id?}` → 201 with the source.
pub(crate) async fn add_source(
    State(api): State<Api>,
    Body(new): Body<NewSource>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let name = new.name.trim();
    if name.is_empty() || name.len() > MAX_NAME {
        return Err(ApiError::invalid(format!(
            "Give the source a name of 1 to {MAX_NAME} characters."
        )));
    }
    let workspace = workspace_param(new.workspace_id.as_deref())?.unwrap_or(WorkspaceId::MAIN);
    let source = api
        .engine
        .add_knowledge_source(new.kind.trim(), name, &new.settings, workspace)
        .await?;
    let view = source_view(SourceSummary {
        source,
        docs: 0,
        chunks: 0,
        last_indexed_at: None,
    });
    Ok((StatusCode::CREATED, Json(view)))
}

/// `DELETE /knowledge/sources/{id}?workspace=` (default Main) → 204: removes the
/// source from that workspace, and deletes it with its docs when no workspace has it
/// anymore.
pub(crate) async fn delete_source(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Path(source): Path<String>,
    Query(filter): Query<WorkspaceFilter>,
) -> ApiResult<StatusCode> {
    let workspace = workspace_param(filter.workspace.as_deref())?.unwrap_or(WorkspaceId::MAIN);
    api.engine
        .detach_from_workspace(
            workspace,
            WorkspaceItem::KnowledgeSource(&source),
            &caller.actor(),
        )
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /knowledge/sources/{id}/docs`
pub(crate) async fn list_docs(
    State(api): State<Api>,
    Path(source): Path<String>,
) -> ApiResult<Json<Vec<Value>>> {
    let docs = api.engine.knowledge_docs(&source).await?;
    Ok(Json(docs.into_iter().map(doc_view).collect()))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Upload {
    #[serde(default)]
    docs: Vec<SourceDoc>,
    /// xlsx, docx and PDF files, base64.
    #[serde(default)]
    files: Vec<SourceFile>,
}

/// `POST /knowledge/sources/{id}/docs {docs: [SourceDoc], files: [SourceFile]}`
/// → the report.
pub(crate) async fn upload_docs(
    State(api): State<Api>,
    Path(source): Path<String>,
    Body(upload): Body<Upload>,
) -> ApiResult<Json<Value>> {
    let count = upload.docs.len() + upload.files.len();
    if count == 0 || count > MAX_DOCS {
        return Err(ApiError::invalid(format!(
            "Send 1 to {MAX_DOCS} docs or files at a time."
        )));
    }
    let mut report = api.engine.ingest_knowledge(&source, upload.docs).await?;
    if !upload.files.is_empty() {
        let files = api.engine.ingest_files(&source, upload.files).await?;
        report.indexed += files.indexed;
        report.unchanged += files.unchanged;
        report.retired += files.retired;
        report.skipped.extend(files.skipped);
    }
    Ok(Json(json!(report)))
}

/// `POST /knowledge/sources/{id}/reindex` → runs the connector's sync now.
pub(crate) async fn reindex(
    State(api): State<Api>,
    Path(source): Path<String>,
) -> ApiResult<Json<Value>> {
    let Some(mods) = &api.mods else {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "unavailable",
            "Mods are not running on this kernel.",
        ));
    };
    let report = api
        .engine
        .sync_knowledge_source(mods.as_ref(), &source)
        .await?;
    Ok(Json(json!(report)))
}

/// `GET /knowledge/docs/{id}` → the doc and its chunks, each with a citation.
pub(crate) async fn get_doc(
    State(api): State<Api>,
    Path(doc): Path<String>,
) -> ApiResult<Json<Value>> {
    let (doc, chunks) = api.engine.knowledge_doc(&doc).await?;
    let base = format!("{} · {}", doc.title, doc.version);
    let chunks: Vec<Value> = chunks
        .into_iter()
        .map(|c| {
            let citation = match c.heading_path.is_empty() {
                true => base.clone(),
                false => format!("{} > {} · {}", doc.title, c.heading_path, doc.version),
            };
            json!({
                "heading_path": c.heading_path,
                "body": c.body,
                "token_estimate": c.token_estimate,
                "citation": citation,
            })
        })
        .collect();
    Ok(Json(json!({ "doc": doc_view(doc), "chunks": chunks })))
}

#[derive(Deserialize)]
pub(crate) struct SearchQuery {
    q: String,
    /// Comma-separated source ids; absent = every source of the workspace.
    sources: Option<String>,
    /// The workspace whose sources are searched (default Main).
    workspace: Option<String>,
}

/// `GET /knowledge/search?q=&workspace=&sources=a,b` → cited hits, best first.
/// Only the workspace's sources are searched.
pub(crate) async fn search(
    State(api): State<Api>,
    Query(query): Query<SearchQuery>,
) -> ApiResult<Json<Vec<Value>>> {
    if query.q.trim().is_empty() || query.q.len() > MAX_QUERY {
        return Err(ApiError::invalid(format!(
            "Search for 1 to {MAX_QUERY} characters."
        )));
    }
    let workspace = workspace_param(query.workspace.as_deref())?.unwrap_or(WorkspaceId::MAIN);
    let sources: Option<Vec<String>> = query.sources.map(|s| {
        s.split(',')
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect()
    });
    let hits = api
        .engine
        .search_knowledge(&query.q, workspace, sources.as_deref(), HITS)
        .await?;
    Ok(Json(
        hits.into_iter()
            .map(|h| {
                json!({
                    "citation": h.citation,
                    "text": h.text,
                    "pointer": h.pointer,
                    "doc_id": h.pointer.strip_prefix("doc:"),
                    "flags": h.flags,
                })
            })
            .collect(),
    ))
}

/// `GET /settings/embeddings`
pub(crate) async fn get_embeddings(State(api): State<Api>) -> ApiResult<Json<Value>> {
    let (settings, has_api_key) = api.engine.embedding_settings().await?;
    let mut view = json!(settings);
    view["has_api_key"] = json!(has_api_key);
    Ok(Json(view))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EmbeddingsInput {
    provider: EmbeddingProvider,
    #[serde(default)]
    base_url: String,
    #[serde(default)]
    model: String,
    #[serde(default)]
    dimensions: Option<u32>,
    /// Write-only: stored as a secret; `""` clears it; absent keeps it.
    api_key: Option<String>,
}

/// `PUT /settings/embeddings {provider, base_url, model, dimensions?, api_key?}` (admin).
pub(crate) async fn put_embeddings(
    State(api): State<Api>,
    caller: axum::Extension<crate::auth::Caller>,
    Body(input): Body<EmbeddingsInput>,
) -> ApiResult<Json<Value>> {
    let settings = EmbeddingSettings {
        provider: input.provider,
        base_url: input.base_url.trim().to_owned(),
        model: input.model.trim().to_owned(),
        dimensions: input.dimensions,
    };
    api.engine
        .set_embedding_settings(&settings, input.api_key.as_deref(), &caller.actor())
        .await?;
    get_embeddings(State(api)).await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ContextInput {
    /// Source ids; `null` = every source.
    sources: Option<Vec<String>>,
    /// Pack cap; `null` = the step's own budget.
    budget_tokens: Option<u32>,
    /// Pinned constraints; absent = none.
    #[serde(default)]
    pinned: Vec<String>,
    /// Block an over-budget pack instead of trimming it; absent = trim.
    #[serde(default)]
    block_over_budget: bool,
}

impl ContextInput {
    fn check(self) -> ApiResult<TaskContext> {
        if self.sources.as_ref().is_some_and(|s| s.len() > MAX_SOURCES) {
            return Err(ApiError::invalid(format!(
                "Pick at most {MAX_SOURCES} sources."
            )));
        }
        if self.budget_tokens == Some(0) {
            return Err(ApiError::invalid("The budget must be at least 1 token."));
        }
        let pinned: Vec<String> = self
            .pinned
            .iter()
            .map(|p| p.trim().to_owned())
            .filter(|p| !p.is_empty())
            .collect();
        if pinned.len() > MAX_PINNED {
            return Err(ApiError::invalid(format!(
                "Pin at most {MAX_PINNED} constraints."
            )));
        }
        if pinned.iter().any(|p| p.chars().count() > MAX_PINNED_CHARS) {
            return Err(ApiError::invalid(format!(
                "Keep each pinned constraint under {MAX_PINNED_CHARS} characters."
            )));
        }
        Ok(TaskContext {
            sources: self.sources,
            budget_tokens: self.budget_tokens,
            pinned,
            block_over_budget: self.block_over_budget,
        })
    }
}

fn context_view(c: &TaskContext) -> Value {
    json!({
        "sources": c.sources,
        "budget_tokens": c.budget_tokens,
        "pinned": c.pinned,
        "block_over_budget": c.block_over_budget,
    })
}

/// `GET /tasks/{id}/context`
pub(crate) async fn get_context(
    State(api): State<Api>,
    Path(task): Path<String>,
) -> ApiResult<Json<Value>> {
    let task: TaskId = id(&task, "task")?;
    Ok(Json(context_view(&api.engine.task_context(task).await?)))
}

/// `PUT /tasks/{id}/context {sources, budget_tokens}`
pub(crate) async fn put_context(
    State(api): State<Api>,
    Path(task): Path<String>,
    Body(input): Body<ContextInput>,
) -> ApiResult<Json<Value>> {
    let task: TaskId = id(&task, "task")?;
    let context = input.check()?;
    api.engine.set_task_context(task, &context).await?;
    Ok(Json(context_view(&context)))
}

/// `POST /tasks/{id}/context/preview {sources, budget_tokens, ...}` → the
/// pack (summary, budget, items with priority, tokens, included or pointer),
/// the suggested sources, the cost estimate and `blocked`. Stores nothing.
pub(crate) async fn preview_context(
    State(api): State<Api>,
    Path(task): Path<String>,
    Body(input): Body<ContextInput>,
) -> ApiResult<Json<Value>> {
    let task: TaskId = id(&task, "task")?;
    let preview = api.engine.preview_context(task, &input.check()?).await?;
    let pack = &preview.pack;
    let items: Vec<Value> = pack
        .items
        .iter()
        .map(|i| {
            json!({
                "priority": i.priority,
                "label": i.label,
                "pointer": i.pointer,
                "tokens": i.tokens,
                "included": i.included,
            })
        })
        .collect();
    Ok(Json(json!({
        "summary": pack.summary(),
        "budget_tokens": pack.budget_tokens,
        "token_estimate": pack.token_estimate,
        "items": items,
        "suggested_sources": preview.suggested,
        "cost_estimate_usd": preview.cost_estimate_usd,
        "blocked": preview.blocked,
    })))
}
