//! `task_quality`: how a task's merged PR held up.

use async_trait::async_trait;
use sqlx::Row;
use sqlx::sqlite::SqliteRow;
use sun_core::TaskId;
use sun_core::model::TaskQuality;

use crate::repo::{json, parse, parse_ts, parse_ts_opt, ts, u32_at};
use crate::{Result, Tx};

/// **Repository** for `task_quality` rows; `save_quality` upserts the whole row.
#[async_trait]
pub trait QualityRepo {
    /// Inserts the record, or replaces the task's existing one.
    async fn save_quality(&mut self, quality: &TaskQuality) -> Result<()>;
    /// The task's record, if its PR merged.
    async fn quality_of(&mut self, task_id: TaskId) -> Result<Option<TaskQuality>>;
    /// Every record, newest merge first.
    async fn quality_records(&mut self) -> Result<Vec<TaskQuality>>;
}

#[async_trait]
impl QualityRepo for Tx {
    async fn save_quality(&mut self, q: &TaskQuality) -> Result<()> {
        sqlx::query(
            "INSERT INTO task_quality (task_id, mod_name, repo, pr_number, base, merge_sha, \
             merged_at, window_days, ci_state, reverted_by, fix_commits_json, checked_at, \
             bug_issues_json) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13) \
             ON CONFLICT (task_id) DO UPDATE SET mod_name = excluded.mod_name, \
             repo = excluded.repo, pr_number = excluded.pr_number, base = excluded.base, \
             merge_sha = excluded.merge_sha, merged_at = excluded.merged_at, \
             window_days = excluded.window_days, ci_state = excluded.ci_state, \
             reverted_by = excluded.reverted_by, fix_commits_json = excluded.fix_commits_json, \
             checked_at = excluded.checked_at, bug_issues_json = excluded.bug_issues_json",
        )
        .bind(q.task_id.to_string())
        .bind(&q.mod_name)
        .bind(&q.repo)
        .bind(q.pr_number)
        .bind(&q.base)
        .bind(&q.merge_sha)
        .bind(ts(&q.merged_at))
        .bind(i64::from(q.window_days))
        .bind(q.ci_state.as_deref())
        .bind(q.reverted_by.as_deref())
        .bind(serde_json::to_string(&q.fix_commits)?)
        .bind(q.checked_at.as_ref().map(ts))
        .bind(serde_json::to_string(&q.bug_issues)?)
        .execute(self.conn())
        .await?;
        Ok(())
    }

    async fn quality_of(&mut self, task_id: TaskId) -> Result<Option<TaskQuality>> {
        sqlx::query("SELECT * FROM task_quality WHERE task_id = $1")
            .bind(task_id.to_string())
            .fetch_optional(self.conn())
            .await?
            .as_ref()
            .map(quality_from)
            .transpose()
    }

    async fn quality_records(&mut self) -> Result<Vec<TaskQuality>> {
        sqlx::query("SELECT * FROM task_quality ORDER BY merged_at DESC, task_id")
            .fetch_all(self.conn())
            .await?
            .iter()
            .map(quality_from)
            .collect()
    }
}

fn quality_from(row: &SqliteRow) -> Result<TaskQuality> {
    Ok(TaskQuality {
        task_id: parse(row.try_get("task_id")?)?,
        mod_name: row.try_get("mod_name")?,
        repo: row.try_get("repo")?,
        pr_number: row.try_get("pr_number")?,
        base: row.try_get("base")?,
        merge_sha: row.try_get("merge_sha")?,
        merged_at: parse_ts(row.try_get("merged_at")?)?,
        window_days: u32_at(row, "window_days")?,
        ci_state: row.try_get("ci_state")?,
        reverted_by: row.try_get("reverted_by")?,
        fix_commits: json(row.try_get("fix_commits_json")?)?,
        checked_at: parse_ts_opt(row.try_get("checked_at")?)?,
        bug_issues: json(row.try_get("bug_issues_json")?)?,
    })
}
