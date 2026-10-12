//! `eval_set` and `eval_run`: eval replay of past tasks.

use async_trait::async_trait;
use sqlx::Row;
use sqlx::sqlite::SqliteRow;
use sun_core::model::{EvalRun, EvalSet, EvalStatus};
use sun_core::redact::redact;
use sun_core::{EvalRunId, EvalSetId, Timestamp};

use crate::repo::{json, parse, parse_ts, ts};
use crate::{Result, SecretRepo, StoreError, Tx};

/// **Repository** for eval sets and eval runs.
#[async_trait]
pub trait EvalRepo {
    /// Inserts a saved eval set.
    async fn insert_eval_set(&mut self, set: &EvalSet) -> Result<()>;
    /// One set, or `NotFound`.
    async fn get_eval_set(&mut self, id: EvalSetId) -> Result<EvalSet>;
    /// Every set, newest first.
    async fn eval_sets(&mut self) -> Result<Vec<EvalSet>>;
    /// Inserts a run (status `running`).
    async fn insert_eval_run(&mut self, run: &EvalRun) -> Result<()>;
    /// Ends a `running` run as `done` with `report` or `failed` with
    /// `error`, both redacted. `false`: it was not running.
    async fn finish_eval_run(
        &mut self,
        id: EvalRunId,
        status: EvalStatus,
        report: Option<&serde_json::Value>,
        error: Option<&str>,
        at: Timestamp,
    ) -> Result<bool>;
    /// One run, or `NotFound`.
    async fn get_eval_run(&mut self, id: EvalRunId) -> Result<EvalRun>;
    /// The latest `limit` runs, newest first.
    async fn eval_runs(&mut self, limit: u32) -> Result<Vec<EvalRun>>;
}

#[async_trait]
impl EvalRepo for Tx {
    async fn insert_eval_set(&mut self, set: &EvalSet) -> Result<()> {
        sqlx::query(
            "INSERT INTO eval_set (id, name, task_ids_json, created_at) VALUES ($1, $2, $3, $4)",
        )
        .bind(set.id.to_string())
        .bind(&set.name)
        .bind(serde_json::to_string(&set.task_ids)?)
        .bind(ts(&set.created_at))
        .execute(self.conn())
        .await?;
        Ok(())
    }

    async fn get_eval_set(&mut self, id: EvalSetId) -> Result<EvalSet> {
        let row = sqlx::query("SELECT * FROM eval_set WHERE id = $1")
            .bind(id.to_string())
            .fetch_optional(self.conn())
            .await?
            .ok_or_else(|| StoreError::not_found("eval set", id))?;
        set_from(&row)
    }

    async fn eval_sets(&mut self) -> Result<Vec<EvalSet>> {
        sqlx::query("SELECT * FROM eval_set ORDER BY id DESC")
            .fetch_all(self.conn())
            .await?
            .iter()
            .map(set_from)
            .collect()
    }

    async fn insert_eval_run(&mut self, run: &EvalRun) -> Result<()> {
        sqlx::query(
            "INSERT INTO eval_run (id, name, spec_json, status, created_at) \
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(run.id.to_string())
        .bind(&run.name)
        .bind(serde_json::to_string(&run.spec)?)
        .bind(EvalStatus::Running.as_str())
        .bind(ts(&run.created_at))
        .execute(self.conn())
        .await?;
        Ok(())
    }

    async fn finish_eval_run(
        &mut self,
        id: EvalRunId,
        status: EvalStatus,
        report: Option<&serde_json::Value>,
        error: Option<&str>,
        at: Timestamp,
    ) -> Result<bool> {
        let known = self.known_secrets().await?;
        let known: Vec<&str> = known.iter().map(String::as_str).collect();
        let report = match report {
            Some(report) => Some(redact(&serde_json::to_string(report)?, &known)),
            None => None,
        };
        let done = sqlx::query(
            "UPDATE eval_run SET status = $1, report_json = $2, error = $3, finished_at = $4 \
             WHERE id = $5 AND status = $6",
        )
        .bind(status.as_str())
        .bind(report)
        .bind(error.map(|e| redact(e, &known)))
        .bind(ts(&at))
        .bind(id.to_string())
        .bind(EvalStatus::Running.as_str())
        .execute(self.conn())
        .await?;
        Ok(done.rows_affected() == 1)
    }

    async fn get_eval_run(&mut self, id: EvalRunId) -> Result<EvalRun> {
        let row = sqlx::query("SELECT * FROM eval_run WHERE id = $1")
            .bind(id.to_string())
            .fetch_optional(self.conn())
            .await?
            .ok_or_else(|| StoreError::not_found("eval run", id))?;
        run_from(&row)
    }

    async fn eval_runs(&mut self, limit: u32) -> Result<Vec<EvalRun>> {
        sqlx::query("SELECT * FROM eval_run ORDER BY id DESC LIMIT $1")
            .bind(i64::from(limit))
            .fetch_all(self.conn())
            .await?
            .iter()
            .map(run_from)
            .collect()
    }
}

fn set_from(row: &SqliteRow) -> Result<EvalSet> {
    Ok(EvalSet {
        id: parse(row.try_get("id")?)?,
        name: row.try_get("name")?,
        task_ids: json(row.try_get("task_ids_json")?)?,
        created_at: parse_ts(row.try_get("created_at")?)?,
    })
}

fn run_from(row: &SqliteRow) -> Result<EvalRun> {
    let report: Option<String> = row.try_get("report_json")?;
    let finished: Option<String> = row.try_get("finished_at")?;
    Ok(EvalRun {
        id: parse(row.try_get("id")?)?,
        name: row.try_get("name")?,
        spec: json(row.try_get("spec_json")?)?,
        status: parse(row.try_get("status")?)?,
        report: report.map(json).transpose()?,
        error: row.try_get("error")?,
        created_at: parse_ts(row.try_get("created_at")?)?,
        finished_at: finished.map(parse_ts).transpose()?,
    })
}
