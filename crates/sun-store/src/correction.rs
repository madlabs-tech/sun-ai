//! `correction`: human corrections of agent work, labelled by cause.

use async_trait::async_trait;
use sqlx::Row;
use sqlx::sqlite::SqliteRow;
use sun_core::model::{Correction, CorrectionCause};
use sun_core::redact::redact;
use sun_core::{CorrectionId, StepKey, TaskId, WorkflowId};

use crate::repo::{parse, parse_ts, ts};
use crate::{Result, SecretRepo, StoreError, Tx};

const COLUMNS: &str = "id, task_id, run_id, workflow_id, step_key, runtime, kind, cause_label, \
                       classified_by, body, created_at";

/// **Repository** for `correction` rows. Insert in the same `Tx` as the
/// change that caused the correction.
#[async_trait]
pub trait CorrectionRepo {
    /// Inserts `correction` with its body redacted. With a `dedupe_key`
    /// already stored it does nothing and returns `false`.
    async fn insert_correction(
        &mut self,
        correction: &Correction,
        dedupe_key: Option<&str>,
    ) -> Result<bool>;
    /// One correction. `NotFound` when missing.
    async fn get_correction(&mut self, id: CorrectionId) -> Result<Correction>;
    /// The task's corrections, oldest first.
    async fn corrections_of(&mut self, task_id: TaskId) -> Result<Vec<Correction>>;
    /// The latest `limit` corrections of every task, newest first.
    async fn corrections(&mut self, limit: u32) -> Result<Vec<Correction>>;
    /// How many corrections of `workflow_id`'s runs have `cause` (the
    /// "same cause ≥ 3 times" proposal trigger).
    async fn count_corrections(
        &mut self,
        workflow_id: WorkflowId,
        cause: CorrectionCause,
    ) -> Result<u32>;
    /// Relabels a correction: the classifier hook. A later classifier (the
    /// decision engine) overrides the rule-based label and names itself in
    /// `classified_by`. `NotFound` when missing.
    async fn set_correction_cause(
        &mut self,
        id: CorrectionId,
        cause: CorrectionCause,
        classified_by: &str,
    ) -> Result<()>;
}

#[async_trait]
impl CorrectionRepo for Tx {
    async fn insert_correction(
        &mut self,
        c: &Correction,
        dedupe_key: Option<&str>,
    ) -> Result<bool> {
        let known = self.known_secrets().await?;
        let known: Vec<&str> = known.iter().map(String::as_str).collect();
        let done = sqlx::query(&format!(
            "INSERT INTO correction ({COLUMNS}, dedupe_key) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12) \
             ON CONFLICT (dedupe_key) DO NOTHING"
        ))
        .bind(c.id.to_string())
        .bind(c.task_id.to_string())
        .bind(c.run_id.to_string())
        .bind(c.workflow_id.to_string())
        .bind(c.step_key.as_ref().map(StepKey::as_str))
        .bind(c.runtime.as_deref())
        .bind(c.kind.as_str())
        .bind(c.cause_label.as_str())
        .bind(&c.classified_by)
        .bind(redact(&c.body, &known))
        .bind(ts(&c.created_at))
        .bind(dedupe_key)
        .execute(self.conn())
        .await?;
        Ok(done.rows_affected() == 1)
    }

    async fn get_correction(&mut self, id: CorrectionId) -> Result<Correction> {
        let row = sqlx::query(&format!("SELECT {COLUMNS} FROM correction WHERE id = $1"))
            .bind(id.to_string())
            .fetch_optional(self.conn())
            .await?
            .ok_or_else(|| StoreError::not_found("correction", id))?;
        correction_from(&row)
    }

    async fn corrections_of(&mut self, task_id: TaskId) -> Result<Vec<Correction>> {
        sqlx::query(&format!(
            "SELECT {COLUMNS} FROM correction WHERE task_id = $1 ORDER BY created_at, id"
        ))
        .bind(task_id.to_string())
        .fetch_all(self.conn())
        .await?
        .iter()
        .map(correction_from)
        .collect()
    }

    async fn corrections(&mut self, limit: u32) -> Result<Vec<Correction>> {
        sqlx::query(&format!(
            "SELECT {COLUMNS} FROM correction ORDER BY created_at DESC, id DESC LIMIT $1"
        ))
        .bind(i64::from(limit))
        .fetch_all(self.conn())
        .await?
        .iter()
        .map(correction_from)
        .collect()
    }

    async fn count_corrections(
        &mut self,
        workflow_id: WorkflowId,
        cause: CorrectionCause,
    ) -> Result<u32> {
        let n: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM correction WHERE workflow_id = $1 AND cause_label = $2",
        )
        .bind(workflow_id.to_string())
        .bind(cause.as_str())
        .fetch_one(self.conn())
        .await?;
        Ok(u32::try_from(n).unwrap_or(u32::MAX))
    }

    async fn set_correction_cause(
        &mut self,
        id: CorrectionId,
        cause: CorrectionCause,
        classified_by: &str,
    ) -> Result<()> {
        let done =
            sqlx::query("UPDATE correction SET cause_label = $1, classified_by = $2 WHERE id = $3")
                .bind(cause.as_str())
                .bind(classified_by)
                .bind(id.to_string())
                .execute(self.conn())
                .await?;
        if done.rows_affected() == 0 {
            return Err(StoreError::not_found("correction", id));
        }
        Ok(())
    }
}

fn correction_from(row: &SqliteRow) -> Result<Correction> {
    let step_key: Option<String> = row.try_get("step_key")?;
    Ok(Correction {
        id: parse(row.try_get("id")?)?,
        task_id: parse(row.try_get("task_id")?)?,
        run_id: parse(row.try_get("run_id")?)?,
        workflow_id: parse(row.try_get("workflow_id")?)?,
        step_key: step_key.map(StepKey::new).transpose()?,
        runtime: row.try_get("runtime")?,
        kind: parse(row.try_get("kind")?)?,
        cause_label: parse(row.try_get("cause_label")?)?,
        classified_by: row.try_get("classified_by")?,
        body: row.try_get("body")?,
        created_at: parse_ts(row.try_get("created_at")?)?,
    })
}
