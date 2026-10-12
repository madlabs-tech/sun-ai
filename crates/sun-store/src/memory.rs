//! `memory_item`: long-term memory (L3) and its proposals.

use async_trait::async_trait;
use sqlx::Row;
use sqlx::sqlite::SqliteRow;
use sun_core::model::{MemoryItem, MemoryStatus, OutboxStatus};
use sun_core::redact::redact;
use sun_core::{IdemKey, MemoryItemId};

use crate::repo::{parse, parse_ts, ts};
use crate::{Result, SecretRepo, StoreError, Tx};

const COLUMNS: &str = "m.id, m.scope, m.scope_id, m.body, m.target, m.reason, m.source_run, m.status, \
                       m.author, m.reviewed_by, m.created_at";

/// Where a repo-scope proposal's PR stands: its outbox row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryPr {
    /// Delivery state of the `open_pr` row.
    pub status: OutboxStatus,
    /// The PR's URL, once the git host answered.
    pub url: Option<String>,
}

/// **Repository** for `memory_item` rows.
#[async_trait]
pub trait MemoryRepo {
    /// Inserts `item` with its body and reason redacted. With a `dedupe_key`
    /// already stored it does nothing and returns `false`.
    async fn insert_memory(&mut self, item: &MemoryItem, dedupe_key: &str) -> Result<bool>;
    /// The status of the item stored under `dedupe_key`, if any.
    async fn memory_status(&mut self, dedupe_key: &str) -> Result<Option<MemoryStatus>>;
    /// One item. `NotFound` when missing.
    async fn get_memory(&mut self, id: MemoryItemId) -> Result<MemoryItem>;
    /// Every item, newest first, with its PR (repo scope, once enqueued).
    async fn memory_items(&mut self) -> Result<Vec<(MemoryItem, Option<MemoryPr>)>>;
    /// Every active item, oldest first.
    async fn active_memory(&mut self) -> Result<Vec<MemoryItem>>;
    /// A human's review: moves `id` to `to` when [`MemoryStatus::can_move`]
    /// allows it, else `InvalidTransition`.
    async fn review_memory(
        &mut self,
        id: MemoryItemId,
        to: MemoryStatus,
        by: &str,
    ) -> Result<MemoryItem>;
    /// Proposed items with a source run whose PR is not enqueued yet, oldest
    /// first (any scope: the caller finds the repo).
    async fn memory_prs_to_open(&mut self) -> Result<Vec<MemoryItem>>;
    /// Records the outbox row that opens the item's PR and the file it edits.
    async fn set_memory_pr(&mut self, id: MemoryItemId, key: &IdemKey, target: &str) -> Result<()>;
}

#[async_trait]
impl MemoryRepo for Tx {
    async fn insert_memory(&mut self, m: &MemoryItem, dedupe_key: &str) -> Result<bool> {
        let known = self.known_secrets().await?;
        let known: Vec<&str> = known.iter().map(String::as_str).collect();
        let done = sqlx::query(
            "INSERT INTO memory_item (id, scope, scope_id, body, reason, source_run, status, \
             author, reviewed_by, created_at, dedupe_key, target) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12) \
             ON CONFLICT (dedupe_key) DO NOTHING",
        )
        .bind(m.id.to_string())
        .bind(m.scope.as_str())
        .bind(m.scope_id.as_deref())
        .bind(redact(&m.body, &known))
        .bind(redact(&m.reason, &known))
        .bind(m.source_run.map(|r| r.to_string()))
        .bind(m.status.as_str())
        .bind(&m.author)
        .bind(m.reviewed_by.as_deref())
        .bind(ts(&m.created_at))
        .bind(dedupe_key)
        .bind(m.target.as_deref())
        .execute(self.conn())
        .await?;
        Ok(done.rows_affected() == 1)
    }

    async fn memory_status(&mut self, dedupe_key: &str) -> Result<Option<MemoryStatus>> {
        let status: Option<String> =
            sqlx::query_scalar("SELECT status FROM memory_item WHERE dedupe_key = $1")
                .bind(dedupe_key)
                .fetch_optional(self.conn())
                .await?;
        status.map(parse).transpose()
    }

    async fn get_memory(&mut self, id: MemoryItemId) -> Result<MemoryItem> {
        let row = sqlx::query(&format!(
            "SELECT {COLUMNS} FROM memory_item m WHERE m.id = $1"
        ))
        .bind(id.to_string())
        .fetch_optional(self.conn())
        .await?
        .ok_or_else(|| StoreError::not_found("memory item", id))?;
        memory_from(&row)
    }

    async fn memory_items(&mut self) -> Result<Vec<(MemoryItem, Option<MemoryPr>)>> {
        sqlx::query(&format!(
            "SELECT {COLUMNS}, o.status AS pr_status, o.result_json AS pr_result \
             FROM memory_item m LEFT JOIN outbox o ON o.idem_key = m.pr_key \
             ORDER BY m.created_at DESC, m.id DESC"
        ))
        .fetch_all(self.conn())
        .await?
        .iter()
        .map(|row| {
            let status: Option<String> = row.try_get("pr_status")?;
            let result: Option<String> = row.try_get("pr_result")?;
            let pr = match status {
                Some(status) => Some(MemoryPr {
                    status: parse(status)?,
                    url: result
                        .and_then(|r| serde_json::from_str::<serde_json::Value>(&r).ok())
                        .and_then(|r| r["url"].as_str().map(str::to_owned)),
                }),
                None => None,
            };
            Ok((memory_from(row)?, pr))
        })
        .collect()
    }

    async fn active_memory(&mut self) -> Result<Vec<MemoryItem>> {
        sqlx::query(&format!(
            "SELECT {COLUMNS} FROM memory_item m WHERE m.status = $1 ORDER BY m.created_at, m.id"
        ))
        .bind(MemoryStatus::Active.as_str())
        .fetch_all(self.conn())
        .await?
        .iter()
        .map(memory_from)
        .collect()
    }

    async fn review_memory(
        &mut self,
        id: MemoryItemId,
        to: MemoryStatus,
        by: &str,
    ) -> Result<MemoryItem> {
        let mut item = self.get_memory(id).await?;
        if !item.status.can_move(to) {
            return Err(sun_core::Error::InvalidTransition {
                machine: "MemoryStatus",
                from: item.status.as_str(),
                to: to.as_str(),
            }
            .into());
        }
        // Guarded by the status read, so a concurrent review can't double-move.
        let done = sqlx::query(
            "UPDATE memory_item SET status = $1, reviewed_by = $2 WHERE id = $3 AND status = $4",
        )
        .bind(to.as_str())
        .bind(by)
        .bind(id.to_string())
        .bind(item.status.as_str())
        .execute(self.conn())
        .await?;
        if done.rows_affected() == 0 {
            return Err(StoreError::not_found("memory item", id));
        }
        item.status = to;
        item.reviewed_by = Some(by.to_owned());
        Ok(item)
    }

    async fn memory_prs_to_open(&mut self) -> Result<Vec<MemoryItem>> {
        sqlx::query(&format!(
            "SELECT {COLUMNS} FROM memory_item m \
             WHERE m.status = $1 AND m.source_run IS NOT NULL AND m.pr_key IS NULL \
             ORDER BY m.created_at, m.id"
        ))
        .bind(MemoryStatus::Proposed.as_str())
        .fetch_all(self.conn())
        .await?
        .iter()
        .map(memory_from)
        .collect()
    }

    async fn set_memory_pr(&mut self, id: MemoryItemId, key: &IdemKey, target: &str) -> Result<()> {
        sqlx::query("UPDATE memory_item SET pr_key = $1, target = $2 WHERE id = $3")
            .bind(key.as_str())
            .bind(target)
            .bind(id.to_string())
            .execute(self.conn())
            .await?;
        Ok(())
    }
}

fn memory_from(row: &SqliteRow) -> Result<MemoryItem> {
    let source_run: Option<String> = row.try_get("source_run")?;
    Ok(MemoryItem {
        id: parse(row.try_get("id")?)?,
        scope: parse(row.try_get("scope")?)?,
        scope_id: row.try_get("scope_id")?,
        body: row.try_get("body")?,
        target: row.try_get("target")?,
        reason: row.try_get("reason")?,
        source_run: source_run.map(parse).transpose()?,
        status: parse(row.try_get("status")?)?,
        author: row.try_get("author")?,
        reviewed_by: row.try_get("reviewed_by")?,
        created_at: parse_ts(row.try_get("created_at")?)?,
    })
}
