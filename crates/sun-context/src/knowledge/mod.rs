//! Knowledge ingest and search (L4):
//! connector doc → denylist → version-hash skip → normalize + redact →
//! heading-aware chunks → `chunk_fts` (unicode61 + trigram) (+ opt-in int8
//! vectors via [`embed_pending`]) → BM25 ∪ trigram ∪ cosine, scoped to the
//! task's sources → reciprocal rank fusion → cited [`KnowledgeHit`]s, with
//! stale and conflicting sources flagged on them ([`flags`]).
//! Connector tombstones go to [`retire`]. Uploaded xlsx, docx and PDF files
//! become docs through [`convert`].

mod chunk;
mod embed;
mod file;
pub mod flags;
mod repo;

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sun_core::redact::redact;
use sun_core::{Error, StepBrief, Timestamp, WorkspaceId};
use sun_store::{
    ChunkHit, DocChunk, FtsIndex, KnowledgeRepo, KnowledgeSource, NewDoc, Result, SecretRepo,
    Store, StoreError,
};

pub use chunk::{MAX_TOKENS, MIN_TOKENS};
pub use embed::{Embedder, HttpEmbedder};
pub use file::{FILE_EXTENSIONS, SourceFile, convert, fixtures};
pub use flags::{DEFAULT_MAX_AGE_DAYS, FlagKind, KnowledgeFlag, unique_flags};
pub use repo::repo_docs;

use embed::{check_vectors, quantize};

use crate::KnowledgeHit;

/// Candidates taken from each index before fusion (top 50).
const CANDIDATES: u32 = 50;
/// Reciprocal rank fusion constant.
const RRF_K: f64 = 60.0;
/// Largest doc accepted from a connector.
const MAX_CONTENT_BYTES: usize = 8 * 1024 * 1024;
/// Longest external id, title, version or url.
const MAX_FIELD: usize = 2048;

/// File names that are never ingested (lowercase, exact).
const DENIED_NAMES: &[&str] = &[
    "id_rsa",
    "id_dsa",
    "id_ecdsa",
    "id_ed25519",
    "credentials",
    "credentials.json",
    ".netrc",
    ".npmrc",
    ".pypirc",
    ".pgpass",
    ".htpasswd",
    ".git-credentials",
    "secrets.json",
    "secrets.yml",
    "secrets.yaml",
];
/// File extensions that are never ingested (key and certificate stores).
const DENIED_EXTENSIONS: &[&str] = &["pem", "key", "p12", "pfx", "jks", "keystore", "ppk"];

/// A doc as a connector emits it. Everything here is untrusted.
/// Mods send it as JSON (`sync`'s `docs`); unknown fields (e.g. `meta`) are ignored.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SourceDoc {
    /// The connector's id: a repo path, page id, file id, ...
    pub external_id: String,
    /// Title, for citations.
    pub title: String,
    /// The connector's version label (commit, page version, revision).
    pub version: String,
    /// When the doc last changed at the source (RFC 3339).
    #[serde(default)]
    pub updated_at: Option<Timestamp>,
    /// Link to the original.
    #[serde(default)]
    pub url: Option<String>,
    /// Principals allowed to read it (empty = whole ecosystem).
    #[serde(default)]
    pub acl: Vec<String>,
    /// The body.
    pub content: DocContent,
}

/// A doc body. Connectors convert HTML and rich text to markdown.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DocContent {
    /// Markdown or plain text.
    Markdown(String),
    /// One sheet: a header row and data rows (stays a table).
    Sheet {
        /// Column names.
        headers: Vec<String>,
        /// Rows of cells.
        rows: Vec<Vec<String>>,
    },
}

/// What [`ingest`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ingested {
    /// Stored (new or changed) with this many chunks.
    Indexed {
        /// The doc's id.
        doc_id: String,
        /// Chunks written.
        chunks: usize,
    },
    /// Same content hash as the stored version; nothing written.
    Unchanged {
        /// The doc's id.
        doc_id: String,
    },
    /// Refused by the denylist; nothing written.
    Denied {
        /// Why.
        reason: String,
    },
}

/// [`add_source_on`] workspace Main.
pub async fn add_source(
    store: &Store,
    kind: &str,
    name: &str,
    config: serde_json::Value,
) -> Result<String> {
    add_source_on(store, WorkspaceId::MAIN, kind, name, config).await
}

/// Adds a knowledge source on `workspace` and returns its id.
/// `config.max_age_days` (a whole number, `0` = never) sets when its docs are
/// flagged stale (default [`DEFAULT_MAX_AGE_DAYS`]).
pub async fn add_source_on(
    store: &Store,
    workspace: WorkspaceId,
    kind: &str,
    name: &str,
    config: serde_json::Value,
) -> Result<String> {
    if let Some(days) = config.get("max_age_days")
        && days.as_u64().is_none_or(|d| d > 36_500)
    {
        return Err(invalid(
            "max_age_days must be a whole number of days from 0 to 36500".into(),
        ));
    }
    let id = ulid::Ulid::generate().to_string();
    let mut tx = store.begin().await?;
    tx.insert_knowledge_source(
        &KnowledgeSource {
            id: id.clone(),
            kind: field("kind", kind)?.to_owned(),
            name: field("name", name)?.to_owned(),
            config,
        },
        workspace,
    )
    .await?;
    tx.commit().await?;
    Ok(id)
}

/// Ingests one doc of `source_id`: denylist, version-hash skip, normalize,
/// redact (the shared log filter with every value in the secret store), chunk
/// and index — all chunks of the doc are replaced in one transaction.
pub async fn ingest(store: &Store, source_id: &str, doc: SourceDoc) -> Result<Ingested> {
    validate(&doc)?;
    if let Some(reason) = denied(&doc) {
        return Ok(Ingested::Denied { reason });
    }
    let content_hash = hash(&doc)?;
    let mut tx = store.read().await?;
    let existing = tx.doc_hash(source_id, &doc.external_id).await?;
    if let Some((doc_id, stored)) = &existing
        && *stored == content_hash
    {
        return Ok(Ingested::Unchanged {
            doc_id: doc_id.clone(),
        });
    }
    let known = tx.known_secrets().await?;
    drop(tx);
    let known: Vec<&str> = known.iter().map(String::as_str).collect();
    let clean = |text: &str| redact(text, &known);

    let chunks = match &doc.content {
        DocContent::Markdown(text) => {
            chunk::chunk_markdown(&clean(&chunk::normalize_markdown(text)))
        }
        DocContent::Sheet { headers, rows } => {
            let cells =
                |cells: &Vec<String>| -> Vec<String> { cells.iter().map(|c| clean(c)).collect() };
            let rows: Vec<Vec<String>> = rows.iter().map(cells).collect();
            chunk::chunk_sheet(&cells(headers), &rows)
        }
    };
    let new_doc = NewDoc {
        id: existing.map_or_else(|| ulid::Ulid::generate().to_string(), |(id, _)| id),
        source_id: source_id.to_owned(),
        external_id: doc.external_id,
        title: clean(&doc.title),
        version: doc.version,
        content_hash,
        url: doc.url.as_deref().map(clean),
        acl: doc.acl,
        updated_at: doc.updated_at,
    };
    let mut tx = store.begin().await?;
    let doc_id = tx.replace_doc(&new_doc, &chunks).await?;
    tx.commit().await?;
    Ok(Ingested::Indexed {
        doc_id,
        chunks: chunks.len(),
    })
}

/// Re-redacts what is already indexed after `secret` joined the secret store:
/// every doc with a chunk containing it gets its chunks rewritten (one
/// transaction per doc; FTS rows and vectors follow). Returns the docs changed.
/// The stored content hash is kept, so a re-sync of the same doc stays a skip.
// ponytail: chunks only; a secret in a doc title stays until the doc changes.
pub async fn redact_indexed(store: &Store, secret: &str) -> Result<usize> {
    let doc_ids = store.read().await?.docs_containing(secret).await?;
    let mut changed = 0;
    for doc_id in doc_ids {
        let mut tx = store.begin().await?;
        let chunks = tx.doc_chunks(&doc_id).await?;
        let clean: Vec<DocChunk> = chunks
            .iter()
            .map(|c| DocChunk {
                heading_path: redact(&c.heading_path, &[secret]),
                body: redact(&c.body, &[secret]),
                token_estimate: c.token_estimate,
            })
            .collect();
        if clean != chunks {
            tx.replace_chunks(&doc_id, &clean).await?;
            tx.commit().await?;
            changed += 1;
        }
    }
    Ok(changed)
}

/// Retires the docs a connector reports deleted (its sync's tombstones):
/// their chunks leave the index and the doc rows stay as tombstones, in one
/// transaction. Unknown or already retired ids are ignored. Returns how many
/// docs were retired.
pub async fn retire(store: &Store, source_id: &str, external_ids: &[String]) -> Result<usize> {
    for external_id in external_ids {
        field("external_id", external_id)?;
    }
    let mut tx = store.begin().await?;
    let mut retired = 0;
    for external_id in external_ids {
        retired += usize::from(tx.retire_doc(source_id, external_id).await?.is_some());
    }
    tx.commit().await?;
    Ok(retired)
}

/// Where and how [`search`] looks. `Scope::default()` = every source, BM25 only.
#[derive(Clone, Copy, Default)]
pub struct Scope<'a> {
    /// The task's selected sources; `None` = every source.
    pub sources: Option<&'a [String]>,
    /// Adds vector hits when set (opt-in). `None` sends nothing anywhere.
    pub embedder: Option<&'a dyn Embedder>,
}

/// Top `k` chunks for `query` within `scope`: BM25 over words and over
/// trigrams, plus cosine over vectors when an embedder is set, fused by
/// reciprocal rank (k = 60). Each hit cites `title > heading · version · date · url`.
///
/// With an embedder, the query (redacted with the stored secrets) is sent to
/// it; its failure is returned as [`Error::Unavailable`] and friends — retry
/// with `embedder: None` to fall back to BM25.
pub async fn search(
    store: &Store,
    query: &str,
    scope: Scope<'_>,
    k: usize,
) -> Result<Vec<KnowledgeHit>> {
    Ok(ranked(store, query, scope)
        .await?
        .into_iter()
        .take(k)
        .map(knowledge_hit)
        .collect())
}

/// P4 for [`crate::compile`]: [`retrieve`] for the step (query = step goal +
/// task title + `query`). `compile` packs the hits in order within
/// the budget (the rest become pointers) and puts their flags up front.
pub async fn retrieve_for_step(
    store: &Store,
    step: &StepBrief,
    query: &str,
    scope: Scope<'_>,
    k: usize,
) -> Result<Vec<KnowledgeHit>> {
    let text = format!("{} {} {query}", step.goal, step.title);
    retrieve(store, &text, scope, k).await
}

/// The best chunk of each of the top `k` docs for `query`, each carrying the
/// stale and conflict [`KnowledgeFlag`]s it is part of. Conflicts are looked
/// for among every candidate chunk of those docs, not just the best ones.
pub async fn retrieve(
    store: &Store,
    query: &str,
    scope: Scope<'_>,
    k: usize,
) -> Result<Vec<KnowledgeHit>> {
    let candidates = ranked(store, query, scope).await?;
    let mut seen = HashSet::new();
    let best: Vec<&ChunkHit> = candidates
        .iter()
        .filter(|hit| seen.insert(hit.doc_id.as_str()))
        .take(k)
        .collect();
    let docs: Vec<String> = best.iter().map(|h| h.doc_id.clone()).collect();
    let found = flags::flag(&candidates, &docs, chrono::Utc::now());
    Ok(best
        .into_iter()
        .map(|hit| {
            let mut out = knowledge_hit(hit.clone());
            out.flags = found
                .iter()
                .filter(|f| f.pointers.contains(&out.pointer))
                .cloned()
                .collect();
            out
        })
        .collect())
}

/// Embeds up to `limit` chunks that have no vector from `embedder` yet and
/// stores them int8-quantized. Returns how many were stored; call it after
/// ingest until it returns 0. The embedder call runs outside any transaction.
pub async fn embed_pending(store: &Store, embedder: &dyn Embedder, limit: u32) -> Result<u64> {
    let model = embedder.model();
    let chunks = store
        .read()
        .await?
        .chunks_without_vector(model, limit)
        .await?;
    if chunks.is_empty() {
        return Ok(0);
    }
    // Chunks were redacted at ingest; the heading path gives the body context.
    let texts: Vec<String> = chunks
        .iter()
        .map(|c| format!("{}\n\n{}", c.heading_path, c.body))
        .collect();
    let vectors = embedder.embed(&texts).await?;
    check_vectors(&vectors, texts.len())?;
    let pairs: Vec<_> = chunks
        .into_iter()
        .zip(vectors.iter().map(|v| quantize(v)))
        .collect();
    let mut tx = store.begin().await?;
    let stored = tx.put_chunk_vectors(model, &pairs).await?;
    tx.commit().await?;
    Ok(stored)
}

/// Every candidate, fused by reciprocal rank, best first.
async fn ranked(store: &Store, query: &str, scope: Scope<'_>) -> Result<Vec<ChunkHit>> {
    let mut tx = store.read().await?;
    let known = tx.known_secrets().await?;
    drop(tx);
    let query_vector = match scope.embedder {
        Some(embedder) => {
            let known: Vec<&str> = known.iter().map(String::as_str).collect();
            let vectors = embedder.embed(&[redact(query, &known)]).await?;
            check_vectors(&vectors, 1)?;
            Some((embedder.model(), quantize(&vectors[0])))
        }
        None => None,
    };

    let mut tx = store.read().await?;
    let mut lists = vec![
        tx.search_chunks(query, FtsIndex::Words, scope.sources, CANDIDATES)
            .await?,
        tx.search_chunks(query, FtsIndex::Trigram, scope.sources, CANDIDATES)
            .await?,
    ];
    if let Some((model, vector)) = &query_vector {
        lists.push(
            tx.search_vectors(model, vector, scope.sources, CANDIDATES)
                .await?,
        );
    }
    drop(tx);

    let mut fused: HashMap<i64, (f64, ChunkHit)> = HashMap::new();
    for list in lists {
        for (rank, hit) in list.into_iter().enumerate() {
            let score = 1.0 / (RRF_K + rank as f64 + 1.0);
            fused.entry(hit.chunk_id).or_insert((0.0, hit)).0 += score;
        }
    }
    let mut ranked: Vec<(f64, ChunkHit)> = fused.into_values().collect();
    ranked.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.chunk_id.cmp(&b.1.chunk_id)));
    Ok(ranked.into_iter().map(|(_, hit)| hit).collect())
}

fn knowledge_hit(hit: ChunkHit) -> KnowledgeHit {
    KnowledgeHit {
        citation: citation(&hit),
        text: hit.body,
        pointer: format!("doc:{}", hit.doc_id),
        flags: Vec::new(),
    }
}

/// `title > heading · version · YYYY-MM-DD · url`.
fn citation(hit: &ChunkHit) -> String {
    let mut citation = hit.title.clone();
    if !hit.heading_path.is_empty() {
        citation = format!("{citation} > {}", hit.heading_path);
    }
    citation = format!("{citation} · {}", hit.version);
    if let Some(at) = hit.updated_at {
        citation = format!("{citation} · {}", at.format("%Y-%m-%d"));
    }
    if let Some(url) = &hit.url {
        citation = format!("{citation} · {url}");
    }
    citation
}

/// The denylist: secret-bearing file names and private-key material.
fn denied(doc: &SourceDoc) -> Option<String> {
    let name = doc
        .external_id
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default()
        .to_lowercase();
    let extension = name.rsplit_once('.').map(|(_, e)| e);
    if name == ".env" || name.starts_with(".env.") || DENIED_NAMES.contains(&name.as_str()) {
        return Some(format!("{name} is on the denylist"));
    }
    if let Some(ext) = extension.filter(|e| DENIED_EXTENSIONS.contains(e)) {
        return Some(format!(".{ext} files are on the denylist"));
    }
    let has_key = |text: &str| text.contains("PRIVATE KEY-----");
    let key_inside = match &doc.content {
        DocContent::Markdown(text) => has_key(text),
        DocContent::Sheet { headers, rows } => headers
            .iter()
            .chain(rows.iter().flatten())
            .any(|c| has_key(c)),
    };
    key_inside.then(|| "contains a private key".to_owned())
}

/// Trust-boundary checks on connector input.
fn validate(doc: &SourceDoc) -> Result<()> {
    field("external_id", &doc.external_id)?;
    field("title", &doc.title)?;
    field("version", &doc.version)?;
    if let Some(url) = &doc.url {
        field("url", url)?;
    }
    let size = match &doc.content {
        DocContent::Markdown(text) => text.len(),
        DocContent::Sheet { headers, rows } => headers
            .iter()
            .chain(rows.iter().flatten())
            .map(String::len)
            .sum(),
    };
    if size > MAX_CONTENT_BYTES {
        return Err(invalid(format!(
            "doc {} is larger than {MAX_CONTENT_BYTES} bytes",
            doc.external_id
        )));
    }
    Ok(())
}

fn field<'a>(name: &str, value: &'a str) -> Result<&'a str> {
    let value = value.trim();
    if value.is_empty() || value.len() > MAX_FIELD {
        return Err(invalid(format!("{name} must be 1..={MAX_FIELD} bytes")));
    }
    Ok(value)
}

fn invalid(message: String) -> StoreError {
    StoreError::Core(Error::InvalidInput(message))
}

/// sha256 of everything ingested: any change re-indexes, the same doc skips.
fn hash(doc: &SourceDoc) -> Result<String> {
    let digest = Sha256::digest(serde_json::to_vec(doc)?);
    Ok(digest.iter().map(|b| format!("{b:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(external_id: &str, body: &str) -> SourceDoc {
        SourceDoc {
            external_id: external_id.into(),
            title: "t".into(),
            version: "1".into(),
            updated_at: None,
            url: None,
            acl: Vec::new(),
            content: DocContent::Markdown(body.into()),
        }
    }

    #[test]
    fn knowledge_denylist_blocks_secret_files_and_keys() {
        for path in [
            ".env",
            "app/.env.production",
            "deploy/server.PEM",
            "home/.ssh/id_ed25519",
            "aws/credentials",
        ] {
            assert!(denied(&doc(path, "x")).is_some(), "{path}");
        }
        assert!(denied(&doc("notes.md", "-----BEGIN OPENSSH PRIVATE KEY-----\nabc")).is_some());
        for path in ["docs/env.md", "README.md", "id_ed25519.pub", "keys.md"] {
            assert_eq!(denied(&doc(path, "x")), None, "{path}");
        }
    }
}
