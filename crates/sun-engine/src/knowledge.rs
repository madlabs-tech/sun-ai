//! Knowledge sources (L4), part of the engine's **Facade** for the
//! API: sources, docs and chunks, search, uploads, each task's context
//! selection and the embedding provider. Plus the knowledge worker: one
//! scheduled sync loop for every connector source (a **Strategy** table maps
//! a source kind to its mod and method) and the opt-in `embed_pending` pass.
//!
//! Connector contract: `call(mod, method, {<params>, since,
//! page_token})` answers `{docs, next_page_token, cursor, skipped?, deleted?}`;
//! the loop ingests the docs, retires the `deleted` ids, follows
//! `next_page_token` until null, then stores the last `cursor` (opaque) on the
//! source as the next `since`.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sun_context::knowledge::{
    self, Embedder, HttpEmbedder, Ingested, Scope, SourceDoc, SourceFile,
};
use sun_context::{ContextPack, KnowledgeHit, LedgerView, compile};
use sun_core::model::Task;
use sun_core::model::{AuditAction, SecretScope};
use sun_core::redact::redact;
use sun_core::{
    EcosystemId, Error, ModCall, ModLink, Result, StepBrief, StepKey, TaskId, WorkspaceId,
};
use sun_store::{
    AuditRepo, DocChunk, DocSummary, EventLog, KnowledgeRepo, KnowledgeSource, SecretRepo,
    SettingRepo, SourceSummary, Store, TaskContext, TaskRepo, Tx, WorkspaceRepo,
};
use tokio::task::JoinHandle;

use crate::Engine;

/// Kinds that hold uploaded docs only (no connector).
const UPLOAD: &str = "upload";
/// Markdown files of a checkout on the kernel's machine (`path`), synced by
/// the kernel itself.
const REPO: &str = "repo";
/// A connector source's optional webhook that gets every synced page
/// (schema `sun.sync_export.v1`).
const EXPORT_WEBHOOK: &str = "export_webhook";
/// A connector: source kind → mod, method, required and optional params.
struct Connector {
    kind: &'static str,
    mod_name: &'static str,
    method: &'static str,
    required: &'static [&'static str],
    optional: &'static [&'static str],
}
const CONNECTORS: &[Connector] = &[
    Connector {
        kind: "gdrive",
        mod_name: "gdrive",
        method: "sync",
        required: &[],
        optional: &["folder"],
    },
    Connector {
        kind: "confluence",
        mod_name: "confluence",
        method: "sync",
        required: &["space"],
        optional: &[],
    },
    Connector {
        kind: "jira",
        mod_name: "sync",
        method: "sync_jira",
        required: &["project"],
        optional: &[],
    },
    Connector {
        kind: "slack",
        mod_name: "sync",
        method: "sync_slack",
        required: &["channel"],
        optional: &[],
    },
];
/// Settings any source may have (docs older than this are stale).
const MAX_AGE_DAYS: &str = "max_age_days";
/// Keys the kernel keeps in a source's config; never taken from callers.
const CURSOR: &str = "cursor";
const LAST_SYNC: &str = "last_sync";
/// Export progress `{since, pages}`: pages of the sync from `since` already
/// delivered to the webhook, so a retried sync sends each page once.
const EXPORTED: &str = "exported";
/// Webhook export payload schema id.
const EXPORT_SCHEMA: &str = "sun.sync_export.v1";
/// Webhook request timeout.
const EXPORT_TIMEOUT: Duration = Duration::from_secs(30);
/// Longest connector param value.
const MAX_PARAM: usize = 256;
/// Pages one sync may follow before it stops (a looping page token).
const MAX_PAGES: usize = 1000;
/// Chunks embedded per provider call.
const EMBED_BATCH: u32 = 32;
/// Knowledge docs retrieved per step (P4).
pub(crate) const KNOWLEDGE_DOCS: usize = 10;
/// Context window every pack is budgeted against (`B` = 10% of it).
// ponytail: one window for every runtime; take it from the runtime's
// capabilities once adapters report it.
pub(crate) const CONTEXT_WINDOW: u32 = 200_000;
/// `kernel_setting` key of [`EmbeddingSettings`].
const EMBEDDINGS: &str = "embeddings";
/// Secret holding the embedding provider's API key (ecosystem scope).
pub const EMBEDDING_API_KEY: &str = "EMBEDDING_API_KEY";

/// Which embedding provider; `none` = BM25 only, nothing leaves.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EmbeddingProvider {
    /// Embeddings off (default).
    #[default]
    None,
    /// An OpenAI-compatible `/embeddings` endpoint.
    Openai,
    /// Ollama's `/api/embed`.
    Ollama,
}

/// The opt-in embedding provider; the key is the [`EMBEDDING_API_KEY`] secret.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmbeddingSettings {
    /// Provider.
    pub provider: EmbeddingProvider,
    /// e.g. `https://api.openai.com/v1` or `http://127.0.0.1:11434`.
    #[serde(default)]
    pub base_url: String,
    /// The provider's model name.
    #[serde(default)]
    pub model: String,
    /// Shorter vectors from Matryoshka models (OpenAI only).
    #[serde(default)]
    pub dimensions: Option<u32>,
}

/// What a sync or upload did.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct SyncReport {
    /// Docs stored (new or changed).
    pub indexed: u32,
    /// Docs whose content hash matched.
    pub unchanged: u32,
    /// Docs not indexed, with why (denylist, too large, the mod's `skipped`).
    pub skipped: Vec<String>,
    /// Docs retired because the source deleted them.
    pub retired: u32,
}

/// What the task drawer's context preview shows before a run.
#[derive(Debug, Clone)]
pub struct ContextPreview {
    /// The pack the first agent step would get.
    pub pack: ContextPack,
    /// Sources ranked for the task across every source (RRF over the hits).
    pub suggested: Vec<SuggestedSource>,
    /// `token_estimate` × the blended USD per token of past runs; `None`
    /// until a runtime has reported a cost.
    pub cost_estimate_usd: Option<f64>,
    /// Why a run would be blocked (block mode and the pack doesn't fit).
    pub blocked: Option<String>,
}

/// A source Sun proposes for a task.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SuggestedSource {
    /// Source id.
    pub source_id: String,
    /// Its name.
    pub name: String,
    /// Summed reciprocal-rank score of its docs among the task's hits.
    pub score: f64,
    /// Its docs among the hits.
    pub docs: u32,
    /// In the task's current selection.
    pub selected: bool,
}

/// The ecosystem scope of kernel-wide secrets (one per kernel).
/// Where kernel-wide secrets such as [`EMBEDDING_API_KEY`] live.
pub fn ecosystem_scope() -> Result<SecretScope> {
    // The nil ULID: there is no ecosystem row yet (one per kernel).
    Ok(SecretScope::Ecosystem(
        "00000000000000000000000000".parse::<EcosystemId>()?,
    ))
}

fn connector(kind: &str) -> Option<&'static Connector> {
    CONNECTORS.iter().find(|c| c.kind == kind)
}

fn invalid(message: impl Into<String>) -> Error {
    Error::InvalidInput(message.into())
}

/// The caller's config for a `kind` source: only that connector's params
/// (non-empty strings) and `max_age_days` (a positive integer).
fn check_config(kind: &str, config: &Value) -> Result<Map<String, Value>> {
    let empty = Map::new();
    let config = match config {
        Value::Null => &empty,
        Value::Object(map) => map,
        _ => return Err(invalid("The source settings must be an object.")),
    };
    let (required, optional): (&[&str], &[&str]) = match connector(kind) {
        Some(c) => (c.required, c.optional),
        None if kind == UPLOAD => (&[], &[]),
        None if kind == REPO => (&["path"], &[]),
        None => {
            let kinds: Vec<&str> = CONNECTORS.iter().map(|c| c.kind).collect();
            return Err(invalid(format!(
                "Unknown source kind `{kind}`. Use upload, repo or one of: {}.",
                kinds.join(", ")
            )));
        }
    };
    for key in required {
        if !config.contains_key(*key) {
            return Err(invalid(format!("A {kind} source needs `{key}`.")));
        }
    }
    for (key, value) in config {
        if key == MAX_AGE_DAYS {
            if !value.as_u64().is_some_and(|d| (1..=36_500).contains(&d)) {
                return Err(invalid("`max_age_days` must be a whole number of days."));
            }
        } else if key == EXPORT_WEBHOOK && connector(kind).is_some() {
            check_webhook(value.as_str().unwrap_or_default())?;
        } else if required.contains(&key.as_str()) || optional.contains(&key.as_str()) {
            let ok = value
                .as_str()
                .is_some_and(|v| !v.trim().is_empty() && v.len() <= MAX_PARAM);
            if !ok {
                return Err(invalid(format!(
                    "`{key}` must be 1..={MAX_PARAM} characters."
                )));
            }
        } else {
            return Err(invalid(format!("A {kind} source has no setting `{key}`.")));
        }
    }
    if kind == REPO
        && !config
            .get("path")
            .and_then(Value::as_str)
            .is_some_and(|p| std::path::Path::new(p).is_absolute())
    {
        return Err(invalid(
            "A repo source's `path` must be an absolute folder on the kernel's machine.",
        ));
    }
    Ok(config.clone())
}

/// A webhook must be https, or http to loopback (a local receiver).
fn check_webhook(url: &str) -> Result<()> {
    let loopback = url.strip_prefix("http://").is_some_and(|rest| {
        ["127.0.0.1", "localhost", "[::1]"].iter().any(|host| {
            rest.starts_with(&format!("{host}:")) || rest.starts_with(&format!("{host}/"))
        })
    });
    let ok = (url.starts_with("https://") && url.len() > "https://".len()) || loopback;
    if ok && url.len() <= 2048 && !url.chars().any(char::is_whitespace) {
        Ok(())
    } else {
        Err(invalid(
            "`export_webhook` must be an https URL (or http to localhost).",
        ))
    }
}

/// Sources the knowledge worker syncs: connectors and repo folders.
fn syncable(kind: &str) -> bool {
    kind == REPO || connector(kind).is_some()
}

/// One page of a connector's `sync`.
#[derive(Deserialize)]
struct SyncPage {
    #[serde(default)]
    docs: Vec<Value>,
    #[serde(default)]
    next_page_token: Option<String>,
    #[serde(default)]
    cursor: Option<Value>,
    #[serde(default)]
    skipped: Vec<Value>,
    #[serde(default)]
    deleted: Vec<String>,
}

impl Engine {
    /// The knowledge sources on `workspace` (`NotFound` if missing), every
    /// source when `None`, with their totals.
    pub async fn knowledge_sources(
        &self,
        workspace: Option<WorkspaceId>,
    ) -> Result<Vec<SourceSummary>> {
        let mut tx = self.store.read().await?;
        if let Some(workspace) = workspace {
            tx.get_workspace(workspace).await?;
        }
        Ok(tx.knowledge_sources(workspace).await?)
    }

    /// Adds a source on `workspace` after checking `config` against its kind.
    /// Removing it is [`Engine::detach_from_workspace`].
    pub async fn add_knowledge_source(
        &self,
        kind: &str,
        name: &str,
        config: &Value,
        workspace: WorkspaceId,
    ) -> Result<KnowledgeSource> {
        let config = Value::Object(check_config(kind, config)?);
        self.store.read().await?.get_workspace(workspace).await?;
        let id = knowledge::add_source_on(&self.store, workspace, kind, name, config).await?;
        Ok(self.store.read().await?.get_knowledge_source(&id).await?)
    }

    /// A source's live docs.
    pub async fn knowledge_docs(&self, source_id: &str) -> Result<Vec<DocSummary>> {
        let mut tx = self.store.read().await?;
        tx.get_knowledge_source(source_id).await?;
        Ok(tx.docs_of_source(source_id).await?)
    }

    /// One doc and its chunks, in order.
    pub async fn knowledge_doc(&self, doc_id: &str) -> Result<(DocSummary, Vec<DocChunk>)> {
        let mut tx = self.store.read().await?;
        let doc = tx.get_doc(doc_id).await?;
        Ok((doc, tx.doc_chunks(doc_id).await?))
    }

    /// Top `k` cited chunks for `query` in `workspace`'s sources (narrowed to
    /// `sources` when given), hybrid when embeddings are on.
    pub async fn search_knowledge(
        &self,
        query: &str,
        workspace: WorkspaceId,
        sources: Option<&[String]>,
        k: usize,
    ) -> Result<Vec<KnowledgeHit>> {
        let mut tx = self.store.read().await?;
        tx.get_workspace(workspace).await?;
        let sources = workspace_sources(&mut tx, workspace, sources).await?;
        drop(tx);
        let sources = Some(sources.as_slice());
        let embedder = embedder(&self.store).await.ok().flatten();
        let scope = Scope {
            sources,
            embedder: embedder.as_ref().map(|e| e as &dyn Embedder),
        };
        match knowledge::search(&self.store, query, scope, k).await {
            Err(_) if scope.embedder.is_some() => Ok(knowledge::search(
                &self.store,
                query,
                Scope {
                    sources,
                    embedder: None,
                },
                k,
            )
            .await?),
            other => Ok(other?),
        }
    }

    /// Ingests uploaded docs into `source_id` (any kind), then wakes the
    /// embed worker.
    pub async fn ingest_knowledge(
        &self,
        source_id: &str,
        docs: Vec<SourceDoc>,
    ) -> Result<SyncReport> {
        self.store
            .read()
            .await?
            .get_knowledge_source(source_id)
            .await?;
        let mut report = SyncReport::default();
        for doc in docs {
            ingest_one(&self.store, source_id, doc, &mut report).await?;
        }
        self.knowledge_wake.notify_one();
        Ok(report)
    }

    /// Converts uploaded xlsx, docx and PDF files and ingests
    /// them into `source_id`; a file that can't be read is reported in
    /// `skipped`, not fatal.
    pub async fn ingest_files(
        &self,
        source_id: &str,
        files: Vec<SourceFile>,
    ) -> Result<SyncReport> {
        self.store
            .read()
            .await?
            .get_knowledge_source(source_id)
            .await?;
        let converted = tokio::task::spawn_blocking(move || {
            files
                .into_iter()
                .map(knowledge::convert)
                .collect::<Vec<_>>()
        })
        .await
        .map_err(|e| Error::Unavailable(format!("file conversion: {e}")))?;
        let mut report = SyncReport::default();
        for docs in converted {
            match docs {
                Ok(docs) => {
                    for doc in docs {
                        ingest_one(&self.store, source_id, doc, &mut report).await?;
                    }
                }
                Err(Error::InvalidInput(why)) => report.skipped.push(why),
                Err(e) => return Err(e),
            }
        }
        self.knowledge_wake.notify_one();
        Ok(report)
    }

    /// Runs a connector source's sync now: every page since its cursor, then
    /// the new cursor and the outcome are stored on the source. A failed sync
    /// keeps the old cursor (the next one re-sends; unchanged docs skip) and
    /// records the error.
    pub async fn sync_knowledge_source(&self, mods: &dyn ModLink, id: &str) -> Result<SyncReport> {
        let source = self.store.read().await?.get_knowledge_source(id).await?;
        let result = match connector(&source.kind) {
            Some(conn) => sync_pages(&self.store, mods, conn, &source).await,
            None if source.kind == REPO => sync_repo(&self.store, &source)
                .await
                .map(|report| (report, None)),
            None => {
                return Err(invalid(format!(
                    "`{}` sources have no connector; upload their docs again.",
                    source.kind
                )));
            }
        };
        // Re-read: the export progress was saved page by page.
        let source = self.store.read().await?.get_knowledge_source(id).await?;
        let mut config = match source.config {
            Value::Object(map) => map,
            _ => Map::new(),
        };
        let at = Utc::now().to_rfc3339();
        match &result {
            Ok((report, cursor)) => {
                if let Some(cursor) = cursor {
                    config.insert(CURSOR.into(), cursor.clone());
                }
                config.insert(
                    LAST_SYNC.into(),
                    json!({ "at": at, "report": report, "error": null }),
                );
            }
            Err(e) => {
                let mut last = config.get(LAST_SYNC).cloned().unwrap_or_else(|| json!({}));
                last["at"] = json!(at);
                last["error"] = json!(e.to_string());
                config.insert(LAST_SYNC.into(), last);
            }
        }
        let mut tx = self.store.begin().await?;
        tx.set_knowledge_source_config(id, &Value::Object(config))
            .await?;
        tx.commit().await?;
        self.knowledge_wake.notify_one();
        result.map(|(report, _)| report)
    }

    /// The embedding provider settings and whether its key is set.
    pub async fn embedding_settings(&self) -> Result<(EmbeddingSettings, bool)> {
        let mut tx = self.store.read().await?;
        let settings = match tx.setting(EMBEDDINGS).await? {
            Some(v) => serde_json::from_value(v).map_err(|e| invalid(e.to_string()))?,
            None => EmbeddingSettings::default(),
        };
        let has_key = tx
            .list_secrets(Some(&ecosystem_scope()?))
            .await?
            .iter()
            .any(|s| s.name == EMBEDDING_API_KEY);
        Ok((settings, has_key))
    }

    /// Saves the provider (checked by building it); `api_key` `Some("")`
    /// clears the key, `Some(k)` stores it sealed and redacts it from the
    /// index. The worker embeds pending chunks with the new provider. The
    /// change is audited as `actor` (provider and model; never the key).
    pub async fn set_embedding_settings(
        &self,
        settings: &EmbeddingSettings,
        api_key: Option<&str>,
        actor: &str,
    ) -> Result<()> {
        build_embedder(settings, None)?;
        let mut tx = self.store.begin().await?;
        tx.put_setting(EMBEDDINGS, &json!(settings)).await?;
        let key = match api_key {
            Some("") => ", key cleared",
            Some(_) => ", key set",
            None => "",
        };
        let detail = format!("{:?} {}{key}", settings.provider, settings.model);
        tx.append_audit(actor, AuditAction::SettingsChange, EMBEDDINGS, &detail)
            .await?;
        match api_key {
            Some("") => {
                tx.delete_secret(&ecosystem_scope()?, EMBEDDING_API_KEY)
                    .await?;
            }
            Some(key) => {
                tx.set_secret(&ecosystem_scope()?, EMBEDDING_API_KEY, key)
                    .await?;
            }
            None => {}
        }
        tx.commit().await?;
        if let Some(key) = api_key.filter(|k| !k.is_empty()) {
            knowledge::redact_indexed(&self.store, key).await?;
        }
        self.knowledge_wake.notify_one();
        Ok(())
    }

    /// The task's context selection.
    pub async fn task_context(&self, task_id: TaskId) -> Result<TaskContext> {
        let mut tx = self.store.read().await?;
        tx.get_task(task_id).await?;
        Ok(tx.task_context(task_id).await?)
    }

    /// Saves the task's sources (each must exist on the task's workspace) and
    /// budget cap.
    pub async fn set_task_context(&self, task_id: TaskId, context: &TaskContext) -> Result<()> {
        let mut tx = self.store.begin().await?;
        let task = tx.get_task(task_id).await?;
        let workspace = task.workspace_id;
        let on_workspace = tx.source_ids_of_workspace(workspace).await?;
        for id in context.sources.iter().flatten() {
            let source = tx.get_knowledge_source(id).await?;
            if !on_workspace.contains(id) {
                return Err(invalid(format!(
                    "Knowledge source `{}` isn't on this task's workspace.",
                    source.name
                )));
            }
        }
        tx.put_task_context(task_id, context).await?;
        tx.commit().await?;
        Ok(())
    }

    /// The pack the task's first agent step would get with `context`,
    /// redacted, with the sources Sun proposes, the cost estimate
    /// and whether block mode would stop the run; nothing is stored.
    pub async fn preview_context(
        &self,
        task_id: TaskId,
        context: &TaskContext,
    ) -> Result<ContextPreview> {
        let mut tx = self.store.read().await?;
        let task = tx.get_task(task_id).await?;
        let secrets = tx.known_secrets().await?;
        let memory = crate::memory::active_for(&mut tx, &task).await?;
        let workspace_context = crate::workspaces::workspace_context(&mut tx, task_id).await?;
        let workspace = task.workspace_id;
        let selected = workspace_sources(&mut tx, workspace, context.sources.as_deref()).await?;
        let everywhere = workspace_sources(&mut tx, workspace, None).await?;
        drop(tx);
        let brief = StepBrief {
            task_id,
            title: task.title,
            body: task.body,
            step_key: StepKey::new("preview")?,
            goal: String::new(),
            constraints: context.pinned.clone(),
            gates: Vec::new(),
            acceptance: Vec::new(),
            context_budget_tokens: context.budget_tokens,
        };
        let query = format!("{} {}", brief.title, brief.body);
        let hits = retrieve_scoped(&self.store, &query, &selected).await;
        let ledger = LedgerView {
            workspace_context,
            ..LedgerView::default()
        };
        let mut pack = compile(&brief, &ledger, &memory, &hits, CONTEXT_WINDOW);
        let secrets: Vec<&str> = secrets.iter().map(String::as_str).collect();
        pack.redact(&secrets);
        let everywhere = retrieve_scoped(&self.store, &query, &everywhere).await;
        let suggested = self
            .suggest_sources(&everywhere, context.sources.as_deref())
            .await?;
        let usage = self.store.read().await?.usage(None).await?;
        let tokens = usage.input_tokens.saturating_add(usage.output_tokens);
        // ponytail: one blended rate (input + output) over every past run; a
        // per-runtime price table would be exact.
        let cost_estimate_usd = (tokens > 0 && usage.cost_usd > 0.0)
            .then(|| f64::from(pack.token_estimate) * usage.cost_usd / tokens as f64);
        let blocked = over_budget(&pack).filter(|_| context.block_over_budget);
        Ok(ContextPreview {
            pack,
            suggested,
            cost_estimate_usd,
            blocked,
        })
    }

    /// Ranks sources by their docs among `hits` (best first): each doc adds
    /// 1 / (60 + rank), the RRF weight the hits were fused with.
    async fn suggest_sources(
        &self,
        hits: &[KnowledgeHit],
        selection: Option<&[String]>,
    ) -> Result<Vec<SuggestedSource>> {
        let mut tx = self.store.read().await?;
        let mut by_source: HashMap<String, (f64, u32)> = HashMap::new();
        for (rank, hit) in hits.iter().enumerate() {
            let Some(doc_id) = hit.pointer.strip_prefix("doc:") else {
                continue;
            };
            let Ok(doc) = tx.get_doc(doc_id).await else {
                continue;
            };
            let entry = by_source.entry(doc.source_id).or_default();
            entry.0 += 1.0 / (60.0 + rank as f64 + 1.0);
            entry.1 += 1;
        }
        let names: HashMap<String, String> = tx
            .knowledge_sources(None)
            .await?
            .into_iter()
            .map(|s| (s.source.id, s.source.name))
            .collect();
        let mut out: Vec<SuggestedSource> = by_source
            .into_iter()
            .map(|(source_id, (score, docs))| SuggestedSource {
                name: names.get(&source_id).cloned().unwrap_or_default(),
                selected: selection.is_none_or(|s| s.contains(&source_id)),
                source_id,
                score,
                docs,
            })
            .collect();
        out.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.name.cmp(&b.name)));
        Ok(out)
    }

    /// Starts the knowledge worker: every `sync_every` it syncs each
    /// connector source through `mods`, and after every sync or upload it
    /// embeds pending chunks while embeddings are on (provider errors back
    /// off until the next wake).
    pub fn start_knowledge_worker(
        &self,
        mods: Arc<dyn ModLink>,
        sync_every: Duration,
    ) -> JoinHandle<()> {
        let engine = self.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(sync_every);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = ticker.tick() => engine.sync_all(mods.as_ref()).await,
                    () = engine.knowledge_wake.notified() => {}
                }
                engine.embed_all().await;
            }
        })
    }

    /// One pass of every connector source; each records its own outcome.
    async fn sync_all(&self, mods: &dyn ModLink) {
        let Ok(sources) = self.knowledge_sources(None).await else {
            return;
        };
        for s in sources {
            if syncable(&s.source.kind) {
                let _ = self.sync_knowledge_source(mods, &s.source.id).await;
            }
        }
    }

    /// Embeds pending chunks until none are left or the provider fails.
    async fn embed_all(&self) {
        let Ok(Some(embedder)) = embedder(&self.store).await else {
            return;
        };
        while let Ok(n) = knowledge::embed_pending(&self.store, &embedder, EMBED_BATCH).await {
            if n == 0 {
                break;
            }
        }
    }
}

/// The configured embedder, `None` when embeddings are off.
async fn embedder(store: &Store) -> Result<Option<HttpEmbedder>> {
    let mut tx = store.read().await?;
    let Some(settings) = tx.setting(EMBEDDINGS).await? else {
        return Ok(None);
    };
    let settings: EmbeddingSettings =
        serde_json::from_value(settings).map_err(|e| invalid(e.to_string()))?;
    let key = match settings.provider {
        EmbeddingProvider::Openai => tx
            .secret_value(&ecosystem_scope()?, EMBEDDING_API_KEY)
            .await
            .ok(),
        _ => None,
    };
    build_embedder(&settings, key)
}

fn build_embedder(
    settings: &EmbeddingSettings,
    key: Option<String>,
) -> Result<Option<HttpEmbedder>> {
    Ok(match settings.provider {
        EmbeddingProvider::None => None,
        EmbeddingProvider::Openai => Some(HttpEmbedder::openai(
            &settings.base_url,
            key,
            &settings.model,
            settings.dimensions,
        )?),
        EmbeddingProvider::Ollama => {
            Some(HttpEmbedder::ollama(&settings.base_url, &settings.model)?)
        }
    })
}

/// Why `pack` breaks a block-mode budget: an item was trimmed to a pointer.
pub(crate) fn over_budget(pack: &ContextPack) -> Option<String> {
    let trimmed = pack.items.iter().filter(|i| !i.included).count();
    (trimmed > 0).then(|| {
        format!(
            "The context pack is over budget: {trimmed} item(s) don't fit in {} tokens. \
             Raise the task's budget, remove sources or let Sun trim the pack.",
            pack.budget_tokens
        )
    })
}

/// Indexes a repo source's markdown files and retires the ones gone.
async fn sync_repo(store: &Store, source: &KnowledgeSource) -> Result<SyncReport> {
    let path = PathBuf::from(
        source
            .config
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or_default(),
    );
    let (docs, skipped) = tokio::task::spawn_blocking(move || knowledge::repo_docs(&path))
        .await
        .map_err(|e| Error::Unavailable(format!("repo walk: {e}")))??;
    let mut report = SyncReport {
        skipped,
        ..SyncReport::default()
    };
    let seen: HashSet<String> = docs.iter().map(|d| d.external_id.clone()).collect();
    for doc in docs {
        ingest_one(store, &source.id, doc, &mut report).await?;
    }
    let gone: Vec<String> = store
        .read()
        .await?
        .docs_of_source(&source.id)
        .await?
        .into_iter()
        .map(|d| d.external_id)
        .filter(|id| !seen.contains(id))
        .collect();
    report.retired += retire(store, &source.id, &gone).await?;
    Ok(report)
}

/// Pages of the sync from `since` already delivered to the webhook.
fn exported_pages(config: &Value, since: &Option<Value>) -> usize {
    let since = since.clone().unwrap_or(Value::Null);
    config
        .get(EXPORTED)
        .filter(|e| e.get("since") == Some(&since))
        .and_then(|e| e.get("pages"))
        .and_then(Value::as_u64)
        .and_then(|n| usize::try_from(n).ok())
        .unwrap_or(0)
}

/// Sends one synced page to the source's webhook (redacted, keyed by
/// source, cursor and page so the receiver can dedupe a resend), then saves
/// the progress so a retried sync of the same cursor skips it.
async fn export_page(
    store: &Store,
    url: &str,
    source: &KnowledgeSource,
    since: &Option<Value>,
    page_no: usize,
    page: &SyncPage,
) -> Result<()> {
    let known = store.read().await?.known_secrets().await?;
    let known: Vec<&str> = known.iter().map(String::as_str).collect();
    let body = json!({
        "schema": EXPORT_SCHEMA,
        "source": { "id": source.id, "kind": source.kind, "name": source.name },
        "since": since,
        "page": page_no,
        "cursor": page.cursor,
        "docs": page.docs,
        "deleted": page.deleted,
    });
    let body = redact(&body.to_string(), &known);
    let since_text = since.as_ref().map_or_else(String::new, |s| match s {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    });
    let key: String = format!("sun-export:{}:{since_text}:{page_no}", source.id)
        .chars()
        .filter(char::is_ascii_graphic)
        .take(512)
        .collect();
    let url = url.to_owned();
    tokio::task::spawn_blocking(move || post_webhook(&url, &key, body))
        .await
        .map_err(|e| Error::Unavailable(format!("export: {e}")))??;
    let mut tx = store.begin().await?;
    let mut config = tx.get_knowledge_source(&source.id).await?.config;
    if let Value::Object(map) = &mut config {
        map.insert(
            EXPORTED.into(),
            json!({ "since": since, "pages": page_no + 1 }),
        );
    }
    tx.set_knowledge_source_config(&source.id, &config).await?;
    tx.commit().await?;
    Ok(())
}

/// Blocking POST; any non-2xx is a failed export (the sync keeps its cursor).
fn post_webhook(url: &str, key: &str, body: String) -> Result<()> {
    let agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(EXPORT_TIMEOUT))
        .build()
        .new_agent();
    // Messages carry the status only: the URL may hold a token.
    let status = agent
        .post(url)
        .content_type("application/json")
        .header("idempotency-key", key)
        .send(body)
        .map_err(|_| Error::Unavailable("export webhook: no answer".into()))?
        .status()
        .as_u16();
    if (200..300).contains(&status) {
        Ok(())
    } else {
        Err(Error::Unavailable(format!("export webhook: HTTP {status}")))
    }
}

/// The sources `workspace` sees, narrowed to `selection` (a task's or a
/// caller's) when given. Knowledge search and context compilation search only
/// these, so a workspace never sees another workspace's docs.
pub(crate) async fn workspace_sources(
    tx: &mut Tx,
    workspace: WorkspaceId,
    selection: Option<&[String]>,
) -> Result<Vec<String>> {
    let mut ids = tx.source_ids_of_workspace(workspace).await?;
    if let Some(selection) = selection {
        ids.retain(|id| selection.contains(id));
    }
    Ok(ids)
}

/// The task's workspace's sources, narrowed to its selection
/// ([`workspace_sources`]).
pub(crate) async fn task_sources(tx: &mut Tx, task: &Task) -> Result<Vec<String>> {
    let workspace = task.workspace_id;
    let selection = tx.task_context(task.id).await?.sources;
    workspace_sources(tx, workspace, selection.as_deref()).await
}

/// P4 knowledge for `query` within `sources` (from [`workspace_sources`]):
/// hybrid when embeddings are on, BM25 when they are off or fail. Never
/// fails a session: errors give none.
pub(crate) async fn retrieve_scoped(
    store: &Store,
    query: &str,
    sources: &[String],
) -> Vec<KnowledgeHit> {
    let sources = Some(sources);
    let embedder = embedder(store).await.ok().flatten();
    if let Some(e) = &embedder {
        let scope = Scope {
            sources,
            embedder: Some(e as &dyn Embedder),
        };
        if let Ok(hits) = knowledge::retrieve(store, query, scope, KNOWLEDGE_DOCS).await {
            return hits;
        }
    }
    let scope = Scope {
        sources,
        embedder: None,
    };
    knowledge::retrieve(store, query, scope, KNOWLEDGE_DOCS)
        .await
        .unwrap_or_default()
}

/// Follows a connector's pages; returns the report and the last cursor.
async fn sync_pages(
    store: &Store,
    mods: &dyn ModLink,
    conn: &Connector,
    source: &KnowledgeSource,
) -> Result<(SyncReport, Option<Value>)> {
    let mut params = Map::new();
    for key in conn.required.iter().chain(conn.optional) {
        if let Some(v) = source.config.get(*key) {
            params.insert((*key).into(), v.clone());
        }
    }
    let since = source.config.get(CURSOR).cloned();
    params.insert("since".into(), since.clone().unwrap_or(Value::Null));
    let mut report = SyncReport::default();
    let webhook = source.config.get(EXPORT_WEBHOOK).and_then(Value::as_str);
    let exported = exported_pages(&source.config, &since);
    let from = since.clone();
    let mut cursor = since;
    let mut page_token: Option<String> = None;
    for page_no in 0..MAX_PAGES {
        params.insert("page_token".into(), json!(page_token));
        let answer = mods
            .call(ModCall {
                mod_name: conn.mod_name.into(),
                method: conn.method.into(),
                params: Value::Object(params.clone()),
                idem_key: None,
            })
            .await?;
        let page: SyncPage = serde_json::from_value(answer).map_err(|e| {
            Error::Protocol(format!(
                "{}.{} answered a bad page: {e}",
                conn.mod_name, conn.method
            ))
        })?;
        if let Some(url) = webhook
            && page_no >= exported
        {
            export_page(store, url, source, &from, page_no, &page).await?;
        }
        for doc in page.docs {
            match serde_json::from_value::<SourceDoc>(doc) {
                Ok(doc) => ingest_one(store, &source.id, doc, &mut report).await?,
                Err(e) => report.skipped.push(format!("bad doc: {e}")),
            }
        }
        report.skipped.extend(page.skipped.iter().map(skipped_text));
        report.retired += retire(store, &source.id, &page.deleted).await?;
        if page.cursor.as_ref().is_some_and(|c| !c.is_null()) {
            cursor = page.cursor;
        }
        page_token = page.next_page_token;
        if page_token.is_none() {
            return Ok((report, cursor));
        }
    }
    Err(Error::Protocol(format!(
        "{}.{} sent more than {MAX_PAGES} pages",
        conn.mod_name, conn.method
    )))
}

/// Retires `deleted` ids and, for a deleted file, its `<id>#…` parts
/// (sheet tabs).
async fn retire(store: &Store, source_id: &str, deleted: &[String]) -> Result<u32> {
    if deleted.is_empty() {
        return Ok(0);
    }
    let docs = store.read().await?.docs_of_source(source_id).await?;
    let mut ids = deleted.to_vec();
    ids.extend(
        docs.into_iter()
            .map(|d| d.external_id)
            .filter(|e| deleted.iter().any(|id| e.starts_with(&format!("{id}#")))),
    );
    let retired = knowledge::retire(store, source_id, &ids).await?;
    Ok(u32::try_from(retired).unwrap_or(u32::MAX))
}

/// A mod's `skipped` entry as one line (`{name, reason}` or a string).
fn skipped_text(entry: &Value) -> String {
    match entry {
        Value::String(s) => s.clone(),
        Value::Object(o) => {
            let field = |k: &str| o.get(k).and_then(Value::as_str);
            match (
                field("name").or(field("title")).or(field("id")),
                field("reason"),
            ) {
                (Some(name), Some(reason)) => format!("{name}: {reason}"),
                (Some(name), None) => name.to_owned(),
                _ => entry.to_string(),
            }
        }
        _ => entry.to_string(),
    }
}

/// Ingests one doc; a doc refused at the boundary is reported, not fatal.
async fn ingest_one(
    store: &Store,
    source_id: &str,
    doc: SourceDoc,
    report: &mut SyncReport,
) -> Result<()> {
    let name = doc.title.clone();
    match knowledge::ingest(store, source_id, doc).await {
        Ok(Ingested::Indexed { .. }) => report.indexed += 1,
        Ok(Ingested::Unchanged { .. }) => report.unchanged += 1,
        Ok(Ingested::Denied { reason }) => report.skipped.push(format!("{name}: {reason}")),
        Err(sun_store::StoreError::Core(Error::InvalidInput(m))) => {
            report.skipped.push(format!("{name}: {m}"));
        }
        Err(e) => return Err(e.into()),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn source_config_takes_only_the_connector_params() {
        assert!(check_config("confluence", &json!({"space": "ENG"})).is_ok());
        assert!(check_config("confluence", &json!({})).is_err());
        assert!(check_config("gdrive", &Value::Null).is_ok());
        assert!(check_config("gdrive", &json!({"folder": "f1", "max_age_days": 90})).is_ok());
        assert!(check_config("upload", &json!({"max_age_days": 0})).is_err());
        assert!(check_config("jira", &json!({"project": "P", "cursor": "x"})).is_err());
        assert!(check_config("slack", &json!({"channel": ""})).is_err());
        assert!(check_config("notion", &json!({})).is_err());
    }

    use std::sync::Mutex;

    use async_trait::async_trait;
    use sun_context::knowledge::{DocContent, fixtures};
    use sun_core::{EngineHandle, HostId, RunId};
    use sun_store::{EventLog, NewEvent, RunRepo};

    use crate::gates::tests::{LocalHost, mods, seed};

    async fn engine(dir: &std::path::Path) -> (Engine, Store) {
        let store = Store::open(dir).await.unwrap();
        let config = crate::EngineConfig {
            host: HostId::generate(),
            adapter: "fake".into(),
            delivery_timeout: Duration::from_secs(5),
            watchdog: crate::WatchdogConfig::default(),
        };
        let host = LocalHost::new(dir, |_, _| {});
        let (engine, _task) = Engine::start(store.clone(), host, mods(json!({})), config)
            .await
            .unwrap();
        (engine, store)
    }

    async fn run_and_task(store: &Store) -> (RunId, TaskId) {
        let run = seed(
            store,
            "name: s\nsteps:\n  - { id: a, type: wait, for: 1h }\n",
        )
        .await;
        let task = store
            .read()
            .await
            .unwrap()
            .get_run(run)
            .await
            .unwrap()
            .task_id;
        (run, task)
    }

    fn doc(id: &str, text: &str) -> SourceDoc {
        SourceDoc {
            external_id: id.into(),
            title: id.into(),
            version: "1".into(),
            updated_at: None,
            url: None,
            acl: Vec::new(),
            content: DocContent::Markdown(format!("# {id}\n\n{text}")),
        }
    }

    async fn upload_source(engine: &Engine, name: &str, docs: Vec<SourceDoc>) -> String {
        let source = engine
            .add_knowledge_source("upload", name, &json!({}), WorkspaceId::MAIN)
            .await
            .unwrap();
        engine.ingest_knowledge(&source.id, docs).await.unwrap();
        source.id
    }

    /// `sun.search` from an agent searches only the task's selected sources.
    #[tokio::test]
    async fn sun_search_keeps_to_the_task_sources() {
        let dir = tempfile::tempdir().unwrap();
        let (engine, store) = engine(dir.path()).await;
        let billing = upload_source(
            &engine,
            "billing",
            vec![doc("billing", "Refunds take 5 days.")],
        )
        .await;
        upload_source(
            &engine,
            "hr",
            vec![doc("hr", "Refunds of travel take 30 days.")],
        )
        .await;
        let (run, task) = run_and_task(&store).await;

        let all = engine.search(run, "refunds".into(), 5).await.unwrap();
        assert!(all.contains("billing") && all.contains("hr"), "{all}");
        let only_billing = TaskContext {
            sources: Some(vec![billing]),
            ..TaskContext::default()
        };
        engine.set_task_context(task, &only_billing).await.unwrap();
        let some = engine.search(run, "refunds".into(), 5).await.unwrap();
        assert!(some.contains("billing") && !some.contains("hr"), "{some}");
    }

    /// A workspace's search, a task's search and its context only see the
    /// workspace's sources; a task can use only its workspace's repos. Importing
    /// links the same row; detaching the last workspace deletes a source but
    /// never orphans a repo.
    #[tokio::test]
    async fn workspace_scopes_knowledge_and_repos() {
        use sun_store::WorkspaceItem;

        let dir = tempfile::tempdir().unwrap();
        let (engine, store) = engine(dir.path()).await;
        let main_wiki = upload_source(
            &engine,
            "billing",
            vec![doc("billing", "Refunds take 5 days.")],
        )
        .await;
        let x = engine
            .create_workspace("X", None, None, &Default::default(), "owner")
            .await
            .unwrap()
            .id;
        let x_wiki = engine
            .add_knowledge_source("upload", "hr", &json!({}), x)
            .await
            .unwrap()
            .id;
        engine
            .ingest_knowledge(&x_wiki, vec![doc("hr", "Refunds of travel take 30 days.")])
            .await
            .unwrap();
        let cited = |hits: Vec<KnowledgeHit>| {
            hits.iter()
                .map(|h| h.citation.clone())
                .collect::<Vec<_>>()
                .join(" ")
        };

        // Workspace X's search never returns Main's docs, even when asked by id.
        let on_x = cited(
            engine
                .search_knowledge("refunds", x, None, 5)
                .await
                .unwrap(),
        );
        assert!(on_x.contains("hr") && !on_x.contains("billing"), "{on_x}");
        let asked = engine
            .search_knowledge("refunds", x, Some(std::slice::from_ref(&main_wiki)), 5)
            .await
            .unwrap();
        assert!(asked.is_empty());
        let on_main = cited(
            engine
                .search_knowledge("refunds", WorkspaceId::MAIN, None, 5)
                .await
                .unwrap(),
        );
        assert!(on_main.contains("billing") && !on_main.contains("hr"));

        // A task on Main: its search and context see Main's sources only.
        let (run, task) = run_and_task(&store).await;
        let found = engine.search(run, "refunds".into(), 5).await.unwrap();
        assert!(
            found.contains("billing") && !found.contains("hr"),
            "{found}"
        );
        let x_only = TaskContext {
            sources: Some(vec![x_wiki.clone()]),
            ..TaskContext::default()
        };
        assert!(engine.set_task_context(task, &x_only).await.is_err());
        let preview = engine
            .preview_context(task, &TaskContext::default())
            .await
            .unwrap();
        assert!(preview.suggested.iter().all(|s| s.source_id == main_wiki));

        // An admin imports X's source into Main: the same row, both workspaces see it.
        let item = WorkspaceItem::KnowledgeSource(&x_wiki);
        engine
            .import_to_workspace(WorkspaceId::MAIN, item, "owner")
            .await
            .unwrap();
        assert_eq!(engine.knowledge_sources(None).await.unwrap().len(), 2);
        let found = engine.search(run, "refunds".into(), 5).await.unwrap();
        assert!(found.contains("hr"), "{found}");
        // Detaching keeps it while a workspace has it, then deletes it.
        engine
            .detach_from_workspace(x, item, "owner")
            .await
            .unwrap();
        assert_eq!(engine.knowledge_sources(Some(x)).await.unwrap().len(), 0);
        engine
            .detach_from_workspace(WorkspaceId::MAIN, item, "owner")
            .await
            .unwrap();
        assert_eq!(engine.knowledge_sources(None).await.unwrap().len(), 1);
    }

    /// An uploaded xlsx becomes one sheet doc per tab,
    /// kept as a table with its header row.
    #[tokio::test]
    async fn xlsx_ingests_as_sheet() {
        let dir = tempfile::tempdir().unwrap();
        let (engine, store) = engine(dir.path()).await;
        let source = upload_source(&engine, "finance", Vec::new()).await;
        let xlsx = fixtures::xlsx(
            "Pricing",
            &[
                &["plan", "price"],
                &["team", "10"],
                &["", ""],
                &["pro", "25"],
            ],
        );
        use base64::Engine as _;
        let file = |name: &str, bytes: &[u8]| SourceFile {
            external_id: name.into(),
            title: "Plans".into(),
            version: "v3".into(),
            updated_at: None,
            url: None,
            content_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
        };
        let report = engine
            .ingest_files(
                &source,
                vec![file("plans.xlsx", &xlsx), file("logo.png", b"x")],
            )
            .await
            .unwrap();
        assert_eq!(report.indexed, 1, "{:?}", report.skipped);
        assert_eq!(report.skipped.len(), 1, "{:?}", report.skipped);

        let docs = engine.knowledge_docs(&source).await.unwrap();
        assert_eq!(docs[0].external_id, "plans.xlsx#Pricing");
        assert_eq!(docs[0].title, "Plans / Pricing");
        let chunks = store
            .read()
            .await
            .unwrap()
            .doc_chunks(&docs[0].id)
            .await
            .unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].heading_path, "rows 1-2");
        assert_eq!(
            chunks[0].body,
            "| plan | price |\n| --- | --- |\n| team | 10 |\n| pro | 25 |\n"
        );
    }

    /// The preview proposes sources ranked for the task, prices the
    /// pack from past usage and says when block mode would stop the run.
    #[tokio::test]
    async fn preview_suggests_sources_and_cost() {
        let dir = tempfile::tempdir().unwrap();
        let (engine, store) = engine(dir.path()).await;
        let refunds = upload_source(
            &engine,
            "billing",
            vec![
                doc("refunds", "The task: refund policy. Refunds take 5 days."),
                doc("chargebacks", "Refund disputes and chargebacks."),
            ],
        )
        .await;
        let hr = upload_source(
            &engine,
            "hr",
            vec![doc("travel", "Refund of travel costs.")],
        )
        .await;
        upload_source(
            &engine,
            "eng",
            vec![doc("deploys", "Deploys run on Fridays.")],
        )
        .await;
        let (run, task) = run_and_task(&store).await;

        let mut context = TaskContext {
            sources: Some(vec![hr.clone()]),
            ..TaskContext::default()
        };
        // A task about refunds: its title and body are the query.
        let mut refund_task = store.read().await.unwrap().get_task(task).await.unwrap();
        refund_task.id = TaskId::generate();
        refund_task.title = "refund policy".into();
        let task = refund_task.id;
        let mut tx = store.begin().await.unwrap();
        tx.insert_task(&refund_task).await.unwrap();
        tx.append_event(NewEvent::new(
            run,
            None,
            "usage",
            json!({"input": 900_000, "output": 100_000, "cost_usd": 3.0}),
        ))
        .await
        .unwrap();
        tx.commit().await.unwrap();

        let preview = engine.preview_context(task, &context).await.unwrap();
        let names: Vec<&str> = preview.suggested.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["billing", "hr"], "{:?}", preview.suggested);
        assert_eq!(preview.suggested[0].source_id, refunds);
        assert_eq!(preview.suggested[0].docs, 2);
        assert!(!preview.suggested[0].selected && preview.suggested[1].selected);
        let cost = preview.cost_estimate_usd.unwrap();
        let expected = f64::from(preview.pack.token_estimate) * 3.0 / 1_000_000.0;
        assert!((cost - expected).abs() < 1e-12, "{cost} vs {expected}");
        assert_eq!(preview.blocked, None);

        context.sources = None;
        context.budget_tokens = Some(1);
        context.block_over_budget = true;
        let blocked = engine
            .preview_context(task, &context)
            .await
            .unwrap()
            .blocked;
        assert!(blocked.unwrap().contains("over budget"));
        context.block_over_budget = false;
        let trimmed = engine.preview_context(task, &context).await.unwrap();
        assert_eq!(trimmed.blocked, None);
        assert!(trimmed.pack.items.iter().any(|i| !i.included));
    }

    /// A connector answering `pages` in order, by page token.
    struct Pages {
        pages: Vec<Value>,
        calls: Mutex<Vec<Value>>,
    }

    #[async_trait]
    impl ModLink for Pages {
        async fn call(&self, call: ModCall) -> Result<Value> {
            self.calls.lock().unwrap().push(call.params.clone());
            let index = call.params["page_token"]
                .as_str()
                .map_or(0, |t| t.parse::<usize>().unwrap());
            Ok(self.pages[index].clone())
        }
    }

    /// Every synced page goes to the webhook once per cursor: a
    /// failed sync retries from the same cursor and skips the pages the
    /// webhook already took; the next cursor starts over.
    #[tokio::test]
    async fn sync_export_pages_once_per_cursor() {
        use sun_mod_sdk::http::mock;

        let dir = tempfile::tempdir().unwrap();
        let (engine, store) = engine(dir.path()).await;
        let mut tx = store.begin().await.unwrap();
        tx.set_secret(&ecosystem_scope().unwrap(), "JIRA_TOKEN", "tok-secret-123")
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let seen = Arc::new(Mutex::new(Vec::<(String, Value)>::new()));
        let fail_page_1 = Arc::new(Mutex::new(true));
        let (seen2, fail2) = (seen.clone(), fail_page_1.clone());
        let (url, _log) = mock("", move |req| {
            let page = req.body["page"].as_u64().unwrap();
            if page == 1 && std::mem::replace(&mut *fail2.lock().unwrap(), false) {
                return (503, json!({}));
            }
            let key = req.header("idempotency-key").unwrap().to_owned();
            seen2.lock().unwrap().push((key, req.body.clone()));
            (200, json!({}))
        })
        .unwrap();
        let issue = |key: &str, cursor: &str, next: Value| {
            json!({
                "docs": [{
                    "external_id": key, "title": key, "version": cursor,
                    "content": {"markdown": format!("# {key}\n\nuses tok-secret-123")},
                    "meta": {"status": "Done"},
                }],
                "next_page_token": next,
                "cursor": cursor,
            })
        };
        let connector = Pages {
            pages: vec![
                issue("P-1", "2026-10-01T00:00:00Z", json!("1")),
                issue("P-2", "2026-10-02T00:00:00Z", Value::Null),
            ],
            calls: Mutex::default(),
        };
        let source = engine
            .add_knowledge_source(
                "jira",
                "jira",
                &json!({"project": "P", "export_webhook": format!("{url}/hook")}),
                WorkspaceId::MAIN,
            )
            .await
            .unwrap();

        // Page 0 lands, page 1 fails: the sync fails and keeps its cursor.
        assert!(
            engine
                .sync_knowledge_source(&connector, &source.id)
                .await
                .is_err()
        );
        assert_eq!(seen.lock().unwrap().len(), 1);
        // The retry resends from the same cursor; page 0 isn't exported again.
        engine
            .sync_knowledge_source(&connector, &source.id)
            .await
            .unwrap();
        // The next sync has a new cursor: its pages are new exports.
        engine
            .sync_knowledge_source(&connector, &source.id)
            .await
            .unwrap();

        let seen = seen.lock().unwrap();
        let keys: Vec<&str> = seen.iter().map(|(k, _)| k.as_str()).collect();
        let id = &source.id;
        assert_eq!(
            keys,
            [
                format!("sun-export:{id}::0"),
                format!("sun-export:{id}::1"),
                format!("sun-export:{id}:2026-10-02T00:00:00Z:0"),
                format!("sun-export:{id}:2026-10-02T00:00:00Z:1"),
            ]
        );
        let body = &seen[0].1;
        assert_eq!(body["schema"], EXPORT_SCHEMA);
        assert_eq!(body["source"]["kind"], "jira");
        assert_eq!(body["docs"][0]["meta"]["status"], "Done");
        assert!(!body.to_string().contains("tok-secret-123"), "redacted");
        assert_eq!(body["cursor"], "2026-10-01T00:00:00Z");
        assert!(
            check_config(
                "jira",
                &json!({"project": "P", "export_webhook": "http://example.com/x"})
            )
            .is_err()
        );
        assert!(
            check_config(
                "upload",
                &json!({"export_webhook": "https://example.com/x"})
            )
            .is_err()
        );
    }

    /// A repo source indexes the checkout's markdown and retires
    /// files that were removed.
    #[tokio::test]
    async fn repo_source_syncs_markdown_and_retires_removed_files() {
        let dir = tempfile::tempdir().unwrap();
        let (engine, _store) = engine(dir.path()).await;
        let repo = dir.path().join("checkout");
        std::fs::create_dir_all(repo.join("docs")).unwrap();
        std::fs::write(repo.join("README.md"), "# Readme\n\nHello.").unwrap();
        std::fs::write(repo.join("docs/plan.md"), "# Plan\n\nShip it.").unwrap();
        assert!(check_config("repo", &json!({"path": "relative"})).is_err());
        let source = engine
            .add_knowledge_source(
                "repo",
                "app",
                &json!({"path": repo.to_str().unwrap()}),
                WorkspaceId::MAIN,
            )
            .await
            .unwrap();
        let no_mods = mods(json!({}));
        let report = engine
            .sync_knowledge_source(no_mods.as_ref(), &source.id)
            .await
            .unwrap();
        assert_eq!((report.indexed, report.retired), (2, 0));
        std::fs::remove_file(repo.join("docs/plan.md")).unwrap();
        let report = engine
            .sync_knowledge_source(no_mods.as_ref(), &source.id)
            .await
            .unwrap();
        assert_eq!(
            (report.indexed, report.unchanged, report.retired),
            (0, 1, 1)
        );
        let docs = engine.knowledge_docs(&source.id).await.unwrap();
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].external_id, "README.md");
    }
}
