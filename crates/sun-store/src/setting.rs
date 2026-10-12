//! `kernel_setting` (kernel-wide JSON settings by key), `used_once` (ids
//! that may be used once until they expire) and `task_context` (a task's
//! knowledge sources and context budget, and its pinned constraints).

use async_trait::async_trait;
use serde_json::Value;
use sqlx::Row;
use sun_core::{TaskId, Timestamp};

use crate::member::second;
use crate::repo::json;
use crate::{Result, Tx};

/// What a task's context packs draw from.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TaskContext {
    /// Knowledge source ids to retrieve from; `None` = every source.
    pub sources: Option<Vec<String>>,
    /// Cap on each pack's tokens; `None` = the step's own budget.
    pub budget_tokens: Option<u32>,
    /// Pinned constraints: in every pack's P0, so they survive context
    /// rotation and hand-offs.
    pub pinned: Vec<String>,
    /// Block a pack that doesn't fit the budget instead of trimming it to
    /// pointers.
    pub block_over_budget: bool,
}

/// **Repository** for kernel settings, once-only ids and task context
/// selections.
#[async_trait]
pub trait SettingRepo {
    /// The setting stored under `key`, if any.
    async fn setting(&mut self, key: &str) -> Result<Option<Value>>;
    /// Stores `value` under `key`, replacing any older one.
    async fn put_setting(&mut self, key: &str, value: &Value) -> Result<()>;
    /// Marks `key` used until `expires_at`: `true` the first time, `false`
    /// while an earlier use has not expired (a replay). Prunes expired keys.
    async fn use_once(&mut self, key: &str, expires_at: Timestamp) -> Result<bool>;
    /// Frees `key` (its use failed), so it can be used again.
    async fn release_once(&mut self, key: &str) -> Result<()>;
    /// The task's selection; the default when none was saved.
    async fn task_context(&mut self, task_id: TaskId) -> Result<TaskContext>;
    /// Saves the task's selection.
    async fn put_task_context(&mut self, task_id: TaskId, context: &TaskContext) -> Result<()>;
}

#[async_trait]
impl SettingRepo for Tx {
    async fn setting(&mut self, key: &str) -> Result<Option<Value>> {
        let text: Option<String> =
            sqlx::query_scalar("SELECT value_json FROM kernel_setting WHERE key = $1")
                .bind(key)
                .fetch_optional(self.conn())
                .await?;
        text.map(json).transpose()
    }

    async fn put_setting(&mut self, key: &str, value: &Value) -> Result<()> {
        sqlx::query(
            "INSERT INTO kernel_setting (key, value_json) VALUES ($1, $2) \
             ON CONFLICT (key) DO UPDATE SET value_json = excluded.value_json",
        )
        .bind(key)
        .bind(serde_json::to_string(value)?)
        .execute(self.conn())
        .await?;
        Ok(())
    }

    async fn use_once(&mut self, key: &str, expires_at: Timestamp) -> Result<bool> {
        sqlx::query("DELETE FROM used_once WHERE expires_at <= $1")
            .bind(second(&chrono::Utc::now()))
            .execute(self.conn())
            .await?;
        let done = sqlx::query(
            "INSERT INTO used_once (key, expires_at) VALUES ($1, $2) \
             ON CONFLICT (key) DO NOTHING",
        )
        .bind(key)
        .bind(second(&expires_at))
        .execute(self.conn())
        .await?;
        Ok(done.rows_affected() == 1)
    }

    async fn release_once(&mut self, key: &str) -> Result<()> {
        sqlx::query("DELETE FROM used_once WHERE key = $1")
            .bind(key)
            .execute(self.conn())
            .await?;
        Ok(())
    }

    async fn task_context(&mut self, task_id: TaskId) -> Result<TaskContext> {
        let Some(row) = sqlx::query(
            "SELECT sources_json, budget_tokens, pinned_json, block_over_budget FROM task_context \
                 WHERE task_id = $1",
        )
        .bind(task_id.to_string())
        .fetch_optional(self.conn())
        .await?
        else {
            return Ok(TaskContext::default());
        };
        let budget: Option<i64> = row.try_get("budget_tokens")?;
        Ok(TaskContext {
            sources: row
                .try_get::<Option<String>, _>("sources_json")?
                .map(json)
                .transpose()?,
            budget_tokens: budget.and_then(|b| u32::try_from(b).ok()),
            pinned: row
                .try_get::<Option<String>, _>("pinned_json")?
                .map(json)
                .transpose()?
                .unwrap_or_default(),
            block_over_budget: row.try_get::<i64, _>("block_over_budget")? != 0,
        })
    }

    async fn put_task_context(&mut self, task_id: TaskId, context: &TaskContext) -> Result<()> {
        sqlx::query(
            "INSERT INTO task_context \
             (task_id, sources_json, budget_tokens, pinned_json, block_over_budget) \
             VALUES ($1, $2, $3, $4, $5) \
             ON CONFLICT (task_id) DO UPDATE SET sources_json = excluded.sources_json, \
             budget_tokens = excluded.budget_tokens, pinned_json = excluded.pinned_json, \
             block_over_budget = excluded.block_over_budget",
        )
        .bind(task_id.to_string())
        .bind(
            context
                .sources
                .as_ref()
                .map(serde_json::to_string)
                .transpose()?,
        )
        .bind(context.budget_tokens.map(i64::from))
        .bind(
            (!context.pinned.is_empty())
                .then(|| serde_json::to_string(&context.pinned))
                .transpose()?,
        )
        .bind(i64::from(context.block_over_budget))
        .execute(self.conn())
        .await?;
        Ok(())
    }
}
