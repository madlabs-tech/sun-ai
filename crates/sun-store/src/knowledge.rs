//! `knowledge_source`, `doc`, `chunk` and the `chunk_fts` indexes (L4).
//!
//! Ids are ULID text; `sun-core` has no knowledge id types yet. Opt-in
//! vectors live in `chunk_vec` (int8, scored by the sqlite-vec extension).

use async_trait::async_trait;
use serde_json::Value;
use sqlx::Row;
use sqlx::sqlite::SqliteRow;
use sun_core::{Error, Timestamp, WorkspaceId};

use crate::repo::{json, parse_ts, parse_ts_opt, ts, u32_at};
use crate::workspace::{WorkspaceItem, WorkspaceRepo};
use crate::{Result, Tx};

/// Most query terms sent to FTS5; the rest are ignored.
const MAX_TERMS: usize = 32;

/// A connected knowledge source (repo docs, uploads, Confluence, ...).
#[derive(Debug, Clone, PartialEq)]
pub struct KnowledgeSource {
    /// ULID text.
    pub id: String,
    /// Connector kind, e.g. `repo_markdown`, `upload`, `confluence`.
    pub kind: String,
    /// Display name.
    pub name: String,
    /// Connector settings (never secrets; those live in the secret store).
    pub config: Value,
}

/// One doc version to store; `fetched_at` is set by the store.
#[derive(Debug, Clone, PartialEq)]
pub struct NewDoc {
    /// ULID text; kept from the existing row when the doc is replaced.
    pub id: String,
    /// The owning [`KnowledgeSource`].
    pub source_id: String,
    /// The connector's id for the doc (path, page id, ...).
    pub external_id: String,
    /// Title, for citations.
    pub title: String,
    /// The connector's version label, for citations.
    pub version: String,
    /// Hash of everything ingested; unchanged hash = skip.
    pub content_hash: String,
    /// Link to the original, for citations.
    pub url: Option<String>,
    /// Principals allowed to read it (empty = whole ecosystem).
    pub acl: Vec<String>,
    /// When the doc last changed at the source.
    pub updated_at: Option<Timestamp>,
}

/// One chunk of a doc, in doc order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocChunk {
    /// Headings above the chunk, e.g. `PRD > Pricing > IDR`.
    pub heading_path: String,
    /// The chunk's markdown.
    pub body: String,
    /// Estimated tokens of `body`.
    pub token_estimate: u32,
}

/// Which FTS5 index to query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FtsIndex {
    /// `chunk_fts`: whole words, `unicode61` with diacritics folded.
    Words,
    /// `chunk_fts_trigram`: substrings of 3+ chars (affixes, partial words).
    Trigram,
}

/// A matching chunk with its doc's citation fields, best BM25 first.
#[derive(Debug, Clone, PartialEq)]
pub struct ChunkHit {
    /// `chunk.id`.
    pub chunk_id: i64,
    /// The doc's ULID.
    pub doc_id: String,
    /// Doc title.
    pub title: String,
    /// Doc version label.
    pub version: String,
    /// Doc link.
    pub url: Option<String>,
    /// When the doc last changed at the source.
    pub updated_at: Option<Timestamp>,
    /// Headings above the chunk.
    pub heading_path: String,
    /// The chunk's markdown.
    pub body: String,
    /// The source's `config.max_age_days`: older docs are flagged stale.
    pub max_age_days: Option<i64>,
}

/// A source with its index totals, for the sources list.
#[derive(Debug, Clone, PartialEq)]
pub struct SourceSummary {
    /// The source.
    pub source: KnowledgeSource,
    /// Docs stored.
    pub docs: u32,
    /// Chunks stored.
    pub chunks: u32,
    /// When a doc of it was last (re)indexed.
    pub last_indexed_at: Option<Timestamp>,
}

/// A stored doc version, without its chunks.
#[derive(Debug, Clone, PartialEq)]
pub struct DocSummary {
    /// ULID text.
    pub id: String,
    /// The owning source.
    pub source_id: String,
    /// The connector's id for the doc.
    pub external_id: String,
    /// Title.
    pub title: String,
    /// The connector's version label.
    pub version: String,
    /// Link to the original.
    pub url: Option<String>,
    /// When the doc last changed at the source.
    pub updated_at: Option<Timestamp>,
    /// When this version was indexed.
    pub fetched_at: Timestamp,
    /// Chunks stored.
    pub chunks: u32,
}

/// A chunk's text to embed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkText {
    /// `chunk.id`.
    pub chunk_id: i64,
    /// Headings above the chunk.
    pub heading_path: String,
    /// The chunk's markdown.
    pub body: String,
}

/// Knowledge sources, doc versions and chunk search.
#[async_trait]
pub trait KnowledgeRepo {
    /// Inserts a source, linked to `workspace`.
    async fn insert_knowledge_source(
        &mut self,
        source: &KnowledgeSource,
        workspace: WorkspaceId,
    ) -> Result<()>;
    /// `(doc id, content hash)` of a stored doc, if any.
    async fn doc_hash(
        &mut self,
        source_id: &str,
        external_id: &str,
    ) -> Result<Option<(String, String)>>;
    /// Upserts the doc on `(source_id, external_id)` and replaces all its
    /// chunks (the FTS indexes follow by trigger). Returns the doc's id.
    async fn replace_doc(&mut self, doc: &NewDoc, chunks: &[DocChunk]) -> Result<String>;
    /// The sources linked to `workspace` (every source when `None`) with their
    /// totals (live docs), by name.
    async fn knowledge_sources(
        &mut self,
        workspace: Option<WorkspaceId>,
    ) -> Result<Vec<SourceSummary>>;
    /// Ids of the sources linked to `workspace`.
    async fn source_ids_of_workspace(&mut self, workspace: WorkspaceId) -> Result<Vec<String>>;
    /// One source, or `NotFound`.
    async fn get_knowledge_source(&mut self, id: &str) -> Result<KnowledgeSource>;
    /// Replaces a source's settings (e.g. a connector's sync cursor).
    async fn set_knowledge_source_config(&mut self, id: &str, config: &Value) -> Result<()>;
    /// Deletes a source; its docs, chunks, vectors and FTS rows follow.
    /// `false` when it did not exist.
    async fn delete_knowledge_source(&mut self, id: &str) -> Result<bool>;
    /// A source's docs, by title.
    async fn docs_of_source(&mut self, source_id: &str) -> Result<Vec<DocSummary>>;
    /// One doc, or `NotFound`.
    async fn get_doc(&mut self, doc_id: &str) -> Result<DocSummary>;
    /// Ids of docs with a chunk whose heading path or body contains `needle`
    /// (exact bytes).
    async fn docs_containing(&mut self, needle: &str) -> Result<Vec<String>>;
    /// Replaces a doc's chunks, keeping the doc row (FTS follows by trigger;
    /// vectors of the old chunks go with them).
    async fn replace_chunks(&mut self, doc_id: &str, chunks: &[DocChunk]) -> Result<()>;
    /// Tombstones a doc the connector reports deleted: deletes its chunks
    /// (FTS rows and vectors follow) and sets `deleted_at`; the next
    /// [`KnowledgeRepo::replace_doc`] of it revives the row. Returns the doc's
    /// id, or `None` when it is unknown or already retired.
    async fn retire_doc(&mut self, source_id: &str, external_id: &str) -> Result<Option<String>>;
    /// A doc's chunks in order.
    async fn doc_chunks(&mut self, doc_id: &str) -> Result<Vec<DocChunk>>;
    /// Top `limit` chunks for free-text `query` by BM25 (heading path
    /// weighted 2×). The query is reduced to quoted terms joined by `OR`, so
    /// no FTS5 syntax from the caller reaches `MATCH`.
    ///
    /// `sources` limits hits to those source ids; `None` searches all.
    async fn search_chunks(
        &mut self,
        query: &str,
        index: FtsIndex,
        sources: Option<&[String]>,
        limit: u32,
    ) -> Result<Vec<ChunkHit>>;
    /// Up to `limit` chunks that have no vector from `model` yet, by id.
    async fn chunks_without_vector(&mut self, model: &str, limit: u32) -> Result<Vec<ChunkText>>;
    /// Stores int8 vectors from `model` in `chunk_vec`, replacing older ones.
    /// A vector whose chunk is gone or has another body by now is skipped.
    /// Returns how many were stored.
    async fn put_chunk_vectors(
        &mut self,
        model: &str,
        vectors: &[(ChunkText, Vec<i8>)],
    ) -> Result<u64>;
    /// Top `limit` chunks by cosine distance to `query` (sqlite-vec), among
    /// vectors from `model` with the same dimensions. `sources` as in
    /// [`KnowledgeRepo::search_chunks`].
    async fn search_vectors(
        &mut self,
        model: &str,
        query: &[i8],
        sources: Option<&[String]>,
        limit: u32,
    ) -> Result<Vec<ChunkHit>>;
}

#[async_trait]
impl KnowledgeRepo for Tx {
    async fn insert_knowledge_source(
        &mut self,
        source: &KnowledgeSource,
        workspace: WorkspaceId,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO knowledge_source (id, kind, name, config_json) VALUES ($1, $2, $3, $4)",
        )
        .bind(&source.id)
        .bind(&source.kind)
        .bind(&source.name)
        .bind(serde_json::to_string(&source.config)?)
        .execute(self.conn())
        .await?;
        self.link(workspace, WorkspaceItem::KnowledgeSource(&source.id))
            .await
    }

    async fn doc_hash(
        &mut self,
        source_id: &str,
        external_id: &str,
    ) -> Result<Option<(String, String)>> {
        let row = sqlx::query(
            "SELECT id, content_hash FROM doc WHERE source_id = $1 AND external_id = $2",
        )
        .bind(source_id)
        .bind(external_id)
        .fetch_optional(self.conn())
        .await?;
        row.map(|r| Ok((r.try_get("id")?, r.try_get("content_hash")?)))
            .transpose()
    }

    async fn replace_doc(&mut self, doc: &NewDoc, chunks: &[DocChunk]) -> Result<String> {
        let doc_id: String = sqlx::query_scalar(
            "INSERT INTO doc (id, source_id, external_id, title, version, content_hash, url, \
                              acl_json, updated_at, fetched_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) \
             ON CONFLICT (source_id, external_id) DO UPDATE SET \
                title = excluded.title, version = excluded.version, \
                content_hash = excluded.content_hash, url = excluded.url, \
                acl_json = excluded.acl_json, updated_at = excluded.updated_at, \
                fetched_at = excluded.fetched_at, deleted_at = NULL \
             RETURNING id",
        )
        .bind(&doc.id)
        .bind(&doc.source_id)
        .bind(&doc.external_id)
        .bind(&doc.title)
        .bind(&doc.version)
        .bind(&doc.content_hash)
        .bind(&doc.url)
        .bind(serde_json::to_string(&doc.acl)?)
        .bind(doc.updated_at.as_ref().map(ts))
        .bind(ts(&chrono::Utc::now()))
        .fetch_one(self.conn())
        .await?;
        self.replace_chunks(&doc_id, chunks).await?;
        Ok(doc_id)
    }

    async fn knowledge_sources(
        &mut self,
        workspace: Option<WorkspaceId>,
    ) -> Result<Vec<SourceSummary>> {
        sqlx::query(
            "SELECT s.id, s.kind, s.name, s.config_json, \
                    COUNT(DISTINCT d.id) AS docs, COUNT(c.id) AS chunks, \
                    MAX(d.fetched_at) AS last_indexed_at \
             FROM knowledge_source s \
             LEFT JOIN doc d ON d.source_id = s.id AND d.deleted_at IS NULL \
             LEFT JOIN chunk c ON c.doc_id = d.id \
             WHERE $1 IS NULL OR s.id IN \
                (SELECT source_id FROM workspace_knowledge_source WHERE workspace_id = $1) \
             GROUP BY s.id, s.kind, s.name, s.config_json ORDER BY s.name, s.id",
        )
        .bind(workspace.map(|b| b.to_string()))
        .fetch_all(self.conn())
        .await?
        .iter()
        .map(|r| {
            Ok(SourceSummary {
                source: source_from(r)?,
                docs: u32_at(r, "docs")?,
                chunks: u32_at(r, "chunks")?,
                last_indexed_at: parse_ts_opt(r.try_get("last_indexed_at")?)?,
            })
        })
        .collect()
    }

    async fn source_ids_of_workspace(&mut self, workspace: WorkspaceId) -> Result<Vec<String>> {
        Ok(sqlx::query_scalar(
            "SELECT source_id FROM workspace_knowledge_source WHERE workspace_id = $1 ORDER BY source_id",
        )
        .bind(workspace.to_string())
        .fetch_all(self.conn())
        .await?)
    }

    async fn get_knowledge_source(&mut self, id: &str) -> Result<KnowledgeSource> {
        let row =
            sqlx::query("SELECT id, kind, name, config_json FROM knowledge_source WHERE id = $1")
                .bind(id)
                .fetch_optional(self.conn())
                .await?
                .ok_or_else(|| Error::NotFound(format!("knowledge source {id}")))?;
        source_from(&row)
    }

    async fn set_knowledge_source_config(&mut self, id: &str, config: &Value) -> Result<()> {
        let done = sqlx::query("UPDATE knowledge_source SET config_json = $1 WHERE id = $2")
            .bind(serde_json::to_string(config)?)
            .bind(id)
            .execute(self.conn())
            .await?;
        if done.rows_affected() == 0 {
            return Err(Error::NotFound(format!("knowledge source {id}")).into());
        }
        Ok(())
    }

    async fn delete_knowledge_source(&mut self, id: &str) -> Result<bool> {
        let done = sqlx::query("DELETE FROM knowledge_source WHERE id = $1")
            .bind(id)
            .execute(self.conn())
            .await?;
        Ok(done.rows_affected() > 0)
    }

    async fn docs_of_source(&mut self, source_id: &str) -> Result<Vec<DocSummary>> {
        sqlx::query(&format!(
            "{DOC_COLUMNS} WHERE d.source_id = $1 AND d.deleted_at IS NULL \
             GROUP BY {DOC_GROUP} ORDER BY d.title, d.id"
        ))
        .bind(source_id)
        .fetch_all(self.conn())
        .await?
        .iter()
        .map(doc_from)
        .collect()
    }

    async fn get_doc(&mut self, doc_id: &str) -> Result<DocSummary> {
        let row = sqlx::query(&format!(
            "{DOC_COLUMNS} WHERE d.id = $1 GROUP BY {DOC_GROUP}"
        ))
        .bind(doc_id)
        .fetch_optional(self.conn())
        .await?
        .ok_or_else(|| Error::NotFound(format!("doc {doc_id}")))?;
        doc_from(&row)
    }

    async fn docs_containing(&mut self, needle: &str) -> Result<Vec<String>> {
        if needle.is_empty() {
            return Ok(Vec::new());
        }
        Ok(sqlx::query_scalar(
            "SELECT DISTINCT doc_id FROM chunk \
             WHERE instr(body, $1) > 0 OR instr(heading_path, $1) > 0 ORDER BY doc_id",
        )
        .bind(needle)
        .fetch_all(self.conn())
        .await?)
    }

    async fn replace_chunks(&mut self, doc_id: &str, chunks: &[DocChunk]) -> Result<()> {
        sqlx::query("DELETE FROM chunk WHERE doc_id = $1")
            .bind(doc_id)
            .execute(self.conn())
            .await?;
        for (ord, chunk) in chunks.iter().enumerate() {
            sqlx::query(
                "INSERT INTO chunk (doc_id, ord, heading_path, body, token_estimate) \
                 VALUES ($1, $2, $3, $4, $5)",
            )
            .bind(doc_id)
            .bind(i64::try_from(ord).unwrap_or(i64::MAX))
            .bind(&chunk.heading_path)
            .bind(&chunk.body)
            .bind(i64::from(chunk.token_estimate))
            .execute(self.conn())
            .await?;
        }
        Ok(())
    }

    async fn retire_doc(&mut self, source_id: &str, external_id: &str) -> Result<Option<String>> {
        // The cleared hash makes a later ingest of the same content re-index.
        let doc_id: Option<String> = sqlx::query_scalar(
            "UPDATE doc SET deleted_at = $1, content_hash = '' \
             WHERE source_id = $2 AND external_id = $3 AND deleted_at IS NULL RETURNING id",
        )
        .bind(ts(&chrono::Utc::now()))
        .bind(source_id)
        .bind(external_id)
        .fetch_optional(self.conn())
        .await?;
        if let Some(id) = &doc_id {
            sqlx::query("DELETE FROM chunk WHERE doc_id = $1")
                .bind(id)
                .execute(self.conn())
                .await?;
        }
        Ok(doc_id)
    }

    async fn doc_chunks(&mut self, doc_id: &str) -> Result<Vec<DocChunk>> {
        sqlx::query(
            "SELECT heading_path, body, token_estimate FROM chunk WHERE doc_id = $1 ORDER BY ord",
        )
        .bind(doc_id)
        .fetch_all(self.conn())
        .await?
        .iter()
        .map(|r| {
            Ok(DocChunk {
                heading_path: r.try_get("heading_path")?,
                body: r.try_get("body")?,
                token_estimate: u32_at(r, "token_estimate")?,
            })
        })
        .collect()
    }

    async fn search_chunks(
        &mut self,
        query: &str,
        index: FtsIndex,
        sources: Option<&[String]>,
        limit: u32,
    ) -> Result<Vec<ChunkHit>> {
        let (table, min_chars) = match index {
            FtsIndex::Words => ("chunk_fts", 1),
            FtsIndex::Trigram => ("chunk_fts_trigram", 3),
        };
        // Alphanumeric terms only, so no search syntax from the caller gets in.
        let terms: Vec<&str> = query
            .split(|c: char| !c.is_alphanumeric())
            .filter(|t| t.chars().count() >= min_chars)
            .take(MAX_TERMS)
            .collect();
        if terms.is_empty() {
            return Ok(Vec::new());
        }
        // `table` is one of two literals above, never caller text.
        let sql = format!(
            "{HIT_COLUMNS} FROM {table} JOIN chunk c ON c.id = {table}.rowid \
             JOIN doc d ON d.id = c.doc_id \
             WHERE {table} MATCH $1 AND {IN_SOURCES} \
             ORDER BY bm25({table}, 2.0, 1.0), c.id LIMIT $3"
        );
        let terms = terms
            .iter()
            .map(|t| format!("\"{t}\""))
            .collect::<Vec<_>>()
            .join(" OR ");
        sqlx::query(&sql)
            .bind(terms)
            .bind(sources_json(sources)?)
            .bind(i64::from(limit))
            .fetch_all(self.conn())
            .await?
            .iter()
            .map(chunk_hit)
            .collect()
    }

    async fn chunks_without_vector(&mut self, model: &str, limit: u32) -> Result<Vec<ChunkText>> {
        sqlx::query(
            "SELECT c.id, c.heading_path, c.body FROM chunk c \
             WHERE NOT EXISTS (SELECT 1 FROM chunk_vec v WHERE v.chunk_id = c.id AND v.model = $1) \
             ORDER BY c.id LIMIT $2",
        )
        .bind(model)
        .bind(i64::from(limit))
        .fetch_all(self.conn())
        .await?
        .iter()
        .map(|r| {
            Ok(ChunkText {
                chunk_id: r.try_get("id")?,
                heading_path: r.try_get("heading_path")?,
                body: r.try_get("body")?,
            })
        })
        .collect()
    }

    async fn put_chunk_vectors(
        &mut self,
        model: &str,
        vectors: &[(ChunkText, Vec<i8>)],
    ) -> Result<u64> {
        let mut stored = 0;
        for (chunk, embedding) in vectors {
            // The body check skips a chunk replaced (and its id reused)
            // while it was being embedded; `vec_int8` rejects a bad blob.
            stored += sqlx::query(
                "INSERT OR REPLACE INTO chunk_vec (chunk_id, model, embedding) \
                 SELECT id, $1, vec_int8($2) FROM chunk WHERE id = $3 AND body = $4",
            )
            .bind(model)
            .bind(int8_blob(embedding))
            .bind(chunk.chunk_id)
            .bind(&chunk.body)
            .execute(self.conn())
            .await?
            .rows_affected();
        }
        Ok(stored)
    }

    async fn search_vectors(
        &mut self,
        model: &str,
        query: &[i8],
        sources: Option<&[String]>,
        limit: u32,
    ) -> Result<Vec<ChunkHit>> {
        if query.is_empty() {
            return Ok(Vec::new());
        }
        // ponytail: brute-force scan (fine at SME scale); move to
        // a `vec0` table if knowledge outgrows ~100k chunks.
        let sql = format!(
            "{HIT_COLUMNS} FROM chunk_vec v JOIN chunk c ON c.id = v.chunk_id \
             JOIN doc d ON d.id = c.doc_id \
             WHERE v.model = $1 AND length(v.embedding) = $4 AND {IN_SOURCES} \
             ORDER BY vec_distance_cosine(vec_int8(v.embedding), vec_int8($5)), c.id LIMIT $3"
        );
        sqlx::query(&sql)
            .bind(model)
            .bind(sources_json(sources)?)
            .bind(i64::from(limit))
            .bind(i64::try_from(query.len()).unwrap_or(i64::MAX))
            .bind(int8_blob(query))
            .fetch_all(self.conn())
            .await?
            .iter()
            .map(chunk_hit)
            .collect()
    }
}

/// A [`DocSummary`] query up to its `WHERE`; group by [`DOC_GROUP`].
const DOC_COLUMNS: &str = "SELECT d.id, d.source_id, d.external_id, d.title, d.version, d.url, \
                           d.updated_at, d.fetched_at, COUNT(c.id) AS chunks \
                           FROM doc d LEFT JOIN chunk c ON c.doc_id = d.id";
const DOC_GROUP: &str = "d.id, d.source_id, d.external_id, d.title, d.version, d.url, \
                         d.updated_at, d.fetched_at";

fn doc_from(r: &SqliteRow) -> Result<DocSummary> {
    Ok(DocSummary {
        id: r.try_get("id")?,
        source_id: r.try_get("source_id")?,
        external_id: r.try_get("external_id")?,
        title: r.try_get("title")?,
        version: r.try_get("version")?,
        url: r.try_get("url")?,
        updated_at: parse_ts_opt(r.try_get("updated_at")?)?,
        fetched_at: parse_ts(r.try_get("fetched_at")?)?,
        chunks: u32_at(r, "chunks")?,
    })
}

fn source_from(r: &SqliteRow) -> Result<KnowledgeSource> {
    Ok(KnowledgeSource {
        id: r.try_get("id")?,
        kind: r.try_get("kind")?,
        name: r.try_get("name")?,
        config: json(r.try_get("config_json")?)?,
    })
}

/// Columns of a [`ChunkHit`]; the query aliases `chunk c` and `doc d`.
const HIT_COLUMNS: &str = "SELECT c.id, c.doc_id, c.heading_path, c.body, \
                           d.title, d.version, d.url, d.updated_at, \
                           (SELECT CAST(json_extract(s.config_json, '$.max_age_days') AS TEXT) \
                            FROM knowledge_source s WHERE s.id = d.source_id) AS max_age_days";
/// Source filter on `$2`: a JSON array of source ids, or NULL for all.
const IN_SOURCES: &str = "($2 IS NULL OR d.source_id IN (SELECT value FROM json_each($2)))";

fn sources_json(sources: Option<&[String]>) -> Result<Option<String>> {
    Ok(sources.map(serde_json::to_string).transpose()?)
}

fn int8_blob(vector: &[i8]) -> Vec<u8> {
    vector.iter().map(|&x| x.to_le_bytes()[0]).collect()
}

fn chunk_hit(r: &SqliteRow) -> Result<ChunkHit> {
    Ok(ChunkHit {
        chunk_id: r.try_get("id")?,
        doc_id: r.try_get("doc_id")?,
        title: r.try_get("title")?,
        version: r.try_get("version")?,
        url: r.try_get("url")?,
        updated_at: parse_ts_opt(r.try_get("updated_at")?)?,
        heading_path: r.try_get("heading_path")?,
        body: r.try_get("body")?,
        // Cast to text, so any JSON value decodes; anything but a whole number means "unset".
        max_age_days: r
            .try_get::<Option<String>, _>("max_age_days")?
            .and_then(|days| days.parse().ok()),
    })
}

/// Registers the statically linked sqlite-vec extension for every SQLite
/// connection opened after this call (process-wide, idempotent). Called by
/// `Store::open` before it connects.
#[allow(unsafe_code)]
pub(crate) fn register_sqlite_vec() {
    use libsqlite3_sys::{sqlite3, sqlite3_api_routines, sqlite3_auto_extension};
    use std::os::raw::{c_char, c_int};
    type Init =
        unsafe extern "C" fn(*mut sqlite3, *mut *mut c_char, *const sqlite3_api_routines) -> c_int;
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        // SAFETY: `sqlite3_vec_init` is sqlite-vec's C entry point and has
        // exactly the `Init` signature (the crate declares it without
        // arguments). It is compiled with SQLITE_CORE against the bundled
        // SQLite that libsqlite3-sys links; `sqlite3_auto_extension` has no
        // other preconditions. A failure surfaces as "no such function" on
        // the first vector query.
        unsafe {
            let init =
                std::mem::transmute::<unsafe extern "C" fn(), Init>(sqlite_vec::sqlite3_vec_init);
            sqlite3_auto_extension(Some(init));
        }
    });
}
