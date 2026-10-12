//! `pull_request` (PRs mirrored from the git hosts), `task_pull_request`
//! (their links to tasks) and `task_pull_request_exclusion` (PRs a person
//! removed from a task, which auto-link then leaves alone).

use async_trait::async_trait;
use sqlx::Row;
use sqlx::sqlite::SqliteRow;
use sun_core::model::{PrSnapshot, PullRequest, TaskPullRequest};
use sun_core::{MemberId, PullRequestId, TaskId, WorkspaceId};

use crate::repo::{parse, parse_opt, parse_ts, parse_ts_opt, ts};
use crate::{Result, StoreError, Tx};

/// **Repository** for mirrored pull requests and their task links.
#[async_trait]
pub trait PullRequestRepo {
    /// Inserts the PR, or updates the row with its `(workspace, owner, name,
    /// number)` unless that row's `pr_updated_at` is newer; the snapshot
    /// columns are never touched here. Returns the stored row (its own id
    /// when it existed), or `None` when the delivery was stale.
    async fn upsert_pull_request(&mut self, pr: &PullRequest) -> Result<Option<PullRequest>>;
    /// One PR, or `NotFound`.
    async fn get_pull_request(&mut self, id: PullRequestId) -> Result<PullRequest>;
    /// The workspace's PR at `html_url` (case-insensitive: a pasted owner
    /// name may differ in case), if mirrored.
    async fn pull_request_by_url(
        &mut self,
        workspace: WorkspaceId,
        html_url: &str,
    ) -> Result<Option<PullRequest>>;
    /// The workspace's PRs of `owner/name` whose head is `sha`.
    async fn pull_requests_by_head(
        &mut self,
        workspace: WorkspaceId,
        owner: &str,
        name: &str,
        sha: &str,
    ) -> Result<Vec<PullRequest>>;
    /// Stores a snapshot, only while `snapshot.head_sha` is still the PR's
    /// head; false when the head moved (or there is no such PR).
    async fn set_snapshot(&mut self, id: PullRequestId, snapshot: &PrSnapshot) -> Result<bool>;
    /// Links `pr` to `task` (`by`: the person, `None`: automatic); false
    /// when already linked.
    async fn link(&mut self, task: TaskId, pr: PullRequestId, by: Option<MemberId>)
    -> Result<bool>;
    /// Removes the link; false when there was none.
    async fn unlink(&mut self, task: TaskId, pr: PullRequestId) -> Result<bool>;
    /// The tasks `pr` was linked to automatically (not by a person).
    async fn auto_linked_tasks(&mut self, pr: PullRequestId) -> Result<Vec<TaskId>>;
    /// Every link of `pr`, oldest first.
    async fn tasks_of_pull_request(&mut self, pr: PullRequestId) -> Result<Vec<TaskPullRequest>>;
    /// The task's PRs with their links, oldest link first.
    async fn pull_requests_of_task(
        &mut self,
        task: TaskId,
    ) -> Result<Vec<(TaskPullRequest, PullRequest)>>;
    /// Marks `pr` as removed from `task` by `by`; a no-op when already so.
    async fn exclude(
        &mut self,
        task: TaskId,
        pr: PullRequestId,
        by: Option<MemberId>,
    ) -> Result<()>;
    /// Lifts the exclusion; false when there was none.
    async fn unexclude(&mut self, task: TaskId, pr: PullRequestId) -> Result<bool>;
    /// True when a person removed `pr` from `task`.
    async fn is_excluded(&mut self, task: TaskId, pr: PullRequestId) -> Result<bool>;
}

const COLUMNS: &str = "id, workspace_id, provider, installation_id, repo_owner, repo_name, \
     number, title, state, html_url, branch, author_login, author_avatar_url, merged_at, \
     closed_at, pr_created_at, pr_updated_at, head_sha, additions, deletions, changed_files, \
     mergeable, merge_state_status, checks_rollup_state, checks_total, checks_passed, \
     checks_failed, checks_pending, snapshot_head_sha, snapshot_fetched_at";

const LINK_COLUMNS: &str = "task_id, pull_request_id, linked_by, linked_at";

fn pull_request_from(row: &SqliteRow) -> Result<PullRequest> {
    Ok(PullRequest {
        id: parse(row.try_get("id")?)?,
        workspace_id: parse(row.try_get("workspace_id")?)?,
        provider: parse(row.try_get("provider")?)?,
        installation_id: row.try_get("installation_id")?,
        repo_owner: row.try_get("repo_owner")?,
        repo_name: row.try_get("repo_name")?,
        number: row.try_get("number")?,
        title: row.try_get("title")?,
        state: parse(row.try_get("state")?)?,
        html_url: row.try_get("html_url")?,
        branch: row.try_get("branch")?,
        author_login: row.try_get("author_login")?,
        author_avatar_url: row.try_get("author_avatar_url")?,
        merged_at: parse_ts_opt(row.try_get("merged_at")?)?,
        closed_at: parse_ts_opt(row.try_get("closed_at")?)?,
        pr_created_at: parse_ts(row.try_get("pr_created_at")?)?,
        pr_updated_at: parse_ts(row.try_get("pr_updated_at")?)?,
        head_sha: row.try_get("head_sha")?,
        additions: row.try_get("additions")?,
        deletions: row.try_get("deletions")?,
        changed_files: row.try_get("changed_files")?,
        mergeable: row.try_get("mergeable")?,
        merge_state_status: row.try_get("merge_state_status")?,
        checks_rollup_state: row.try_get("checks_rollup_state")?,
        checks_total: row.try_get("checks_total")?,
        checks_passed: row.try_get("checks_passed")?,
        checks_failed: row.try_get("checks_failed")?,
        checks_pending: row.try_get("checks_pending")?,
        snapshot_head_sha: row.try_get("snapshot_head_sha")?,
        snapshot_fetched_at: parse_ts_opt(row.try_get("snapshot_fetched_at")?)?,
    })
}

fn link_from(row: &SqliteRow) -> Result<TaskPullRequest> {
    Ok(TaskPullRequest {
        task_id: parse(row.try_get("task_id")?)?,
        pull_request_id: parse(row.try_get("pull_request_id")?)?,
        linked_by: parse_opt(row.try_get("linked_by")?)?,
        linked_at: parse_ts(row.try_get("linked_at")?)?,
    })
}

#[async_trait]
impl PullRequestRepo for Tx {
    async fn upsert_pull_request(&mut self, pr: &PullRequest) -> Result<Option<PullRequest>> {
        // Timestamps are RFC 3339 text in one format (`ts`), so they compare
        // as text; the snapshot columns keep what the last fetch stored.
        let row = sqlx::query(&format!(
            "INSERT INTO pull_request ({COLUMNS})
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17,
                     $18, $19, $20, $21, $22, $23, $24, $25, $26, $27, $28, $29, $30)
             ON CONFLICT (workspace_id, repo_owner, repo_name, number) DO UPDATE SET
               provider = excluded.provider, installation_id = excluded.installation_id,
               title = excluded.title, state = excluded.state, html_url = excluded.html_url,
               branch = excluded.branch, author_login = excluded.author_login,
               author_avatar_url = excluded.author_avatar_url, merged_at = excluded.merged_at,
               closed_at = excluded.closed_at, pr_created_at = excluded.pr_created_at,
               pr_updated_at = excluded.pr_updated_at, head_sha = excluded.head_sha,
               additions = excluded.additions, deletions = excluded.deletions,
               changed_files = excluded.changed_files
             WHERE excluded.pr_updated_at >= pull_request.pr_updated_at
             RETURNING {COLUMNS}"
        ))
        .bind(pr.id.to_string())
        .bind(pr.workspace_id.to_string())
        .bind(pr.provider.as_str())
        .bind(pr.installation_id)
        .bind(&pr.repo_owner)
        .bind(&pr.repo_name)
        .bind(pr.number)
        .bind(&pr.title)
        .bind(pr.state.as_str())
        .bind(&pr.html_url)
        .bind(&pr.branch)
        .bind(&pr.author_login)
        .bind(&pr.author_avatar_url)
        .bind(pr.merged_at.as_ref().map(ts))
        .bind(pr.closed_at.as_ref().map(ts))
        .bind(ts(&pr.pr_created_at))
        .bind(ts(&pr.pr_updated_at))
        .bind(&pr.head_sha)
        .bind(pr.additions)
        .bind(pr.deletions)
        .bind(pr.changed_files)
        .bind(&pr.mergeable)
        .bind(&pr.merge_state_status)
        .bind(&pr.checks_rollup_state)
        .bind(pr.checks_total)
        .bind(pr.checks_passed)
        .bind(pr.checks_failed)
        .bind(pr.checks_pending)
        .bind(&pr.snapshot_head_sha)
        .bind(pr.snapshot_fetched_at.as_ref().map(ts))
        .fetch_optional(self.conn())
        .await?;
        row.as_ref().map(pull_request_from).transpose()
    }

    async fn get_pull_request(&mut self, id: PullRequestId) -> Result<PullRequest> {
        let row = sqlx::query(&format!("SELECT {COLUMNS} FROM pull_request WHERE id = $1"))
            .bind(id.to_string())
            .fetch_optional(self.conn())
            .await?
            .ok_or_else(|| StoreError::not_found("pull request", id))?;
        pull_request_from(&row)
    }

    async fn pull_request_by_url(
        &mut self,
        workspace: WorkspaceId,
        html_url: &str,
    ) -> Result<Option<PullRequest>> {
        sqlx::query(&format!(
            "SELECT {COLUMNS} FROM pull_request WHERE workspace_id = $1 AND lower(html_url) = lower($2)"
        ))
        .bind(workspace.to_string())
        .bind(html_url)
        .fetch_optional(self.conn())
        .await?
        .as_ref()
        .map(pull_request_from)
        .transpose()
    }

    async fn pull_requests_by_head(
        &mut self,
        workspace: WorkspaceId,
        owner: &str,
        name: &str,
        sha: &str,
    ) -> Result<Vec<PullRequest>> {
        sqlx::query(&format!(
            "SELECT {COLUMNS} FROM pull_request
             WHERE workspace_id = $1 AND repo_owner = $2 AND repo_name = $3 AND head_sha = $4
             ORDER BY id"
        ))
        .bind(workspace.to_string())
        .bind(owner)
        .bind(name)
        .bind(sha)
        .fetch_all(self.conn())
        .await?
        .iter()
        .map(pull_request_from)
        .collect()
    }

    async fn set_snapshot(&mut self, id: PullRequestId, snapshot: &PrSnapshot) -> Result<bool> {
        let done = sqlx::query(
            "UPDATE pull_request SET mergeable = $1, merge_state_status = $2,
               checks_rollup_state = $3, checks_total = $4, checks_passed = $5,
               checks_failed = $6, checks_pending = $7, snapshot_head_sha = $8,
               snapshot_fetched_at = $9
             WHERE id = $10 AND head_sha = $8",
        )
        .bind(&snapshot.mergeable)
        .bind(&snapshot.merge_state_status)
        .bind(&snapshot.checks_rollup_state)
        .bind(snapshot.checks_total)
        .bind(snapshot.checks_passed)
        .bind(snapshot.checks_failed)
        .bind(snapshot.checks_pending)
        .bind(&snapshot.head_sha)
        .bind(ts(&chrono::Utc::now()))
        .bind(id.to_string())
        .execute(self.conn())
        .await?;
        Ok(done.rows_affected() > 0)
    }

    async fn link(
        &mut self,
        task: TaskId,
        pr: PullRequestId,
        by: Option<MemberId>,
    ) -> Result<bool> {
        let done = sqlx::query(&format!(
            "INSERT INTO task_pull_request ({LINK_COLUMNS}) VALUES ($1, $2, $3, $4)
             ON CONFLICT DO NOTHING"
        ))
        .bind(task.to_string())
        .bind(pr.to_string())
        .bind(by.map(|m| m.to_string()))
        .bind(ts(&chrono::Utc::now()))
        .execute(self.conn())
        .await?;
        Ok(done.rows_affected() > 0)
    }

    async fn unlink(&mut self, task: TaskId, pr: PullRequestId) -> Result<bool> {
        let done = sqlx::query(
            "DELETE FROM task_pull_request WHERE task_id = $1 AND pull_request_id = $2",
        )
        .bind(task.to_string())
        .bind(pr.to_string())
        .execute(self.conn())
        .await?;
        Ok(done.rows_affected() > 0)
    }

    async fn auto_linked_tasks(&mut self, pr: PullRequestId) -> Result<Vec<TaskId>> {
        let ids: Vec<String> = sqlx::query_scalar(
            "SELECT task_id FROM task_pull_request
             WHERE pull_request_id = $1 AND linked_by IS NULL ORDER BY linked_at, task_id",
        )
        .bind(pr.to_string())
        .fetch_all(self.conn())
        .await?;
        ids.into_iter().map(parse).collect()
    }

    async fn tasks_of_pull_request(&mut self, pr: PullRequestId) -> Result<Vec<TaskPullRequest>> {
        sqlx::query(&format!(
            "SELECT {LINK_COLUMNS} FROM task_pull_request
             WHERE pull_request_id = $1 ORDER BY linked_at, task_id"
        ))
        .bind(pr.to_string())
        .fetch_all(self.conn())
        .await?
        .iter()
        .map(link_from)
        .collect()
    }

    async fn pull_requests_of_task(
        &mut self,
        task: TaskId,
    ) -> Result<Vec<(TaskPullRequest, PullRequest)>> {
        // No column name is shared by the two tables, so `p.*` reads by name.
        sqlx::query(
            "SELECT l.task_id, l.pull_request_id, l.linked_by, l.linked_at, p.*
             FROM task_pull_request l JOIN pull_request p ON p.id = l.pull_request_id
             WHERE l.task_id = $1 ORDER BY l.linked_at, p.id",
        )
        .bind(task.to_string())
        .fetch_all(self.conn())
        .await?
        .iter()
        .map(|row| Ok((link_from(row)?, pull_request_from(row)?)))
        .collect()
    }

    async fn exclude(
        &mut self,
        task: TaskId,
        pr: PullRequestId,
        by: Option<MemberId>,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO task_pull_request_exclusion (task_id, pull_request_id, excluded_by, created_at)
             VALUES ($1, $2, $3, $4) ON CONFLICT DO NOTHING",
        )
        .bind(task.to_string())
        .bind(pr.to_string())
        .bind(by.map(|m| m.to_string()))
        .bind(ts(&chrono::Utc::now()))
        .execute(self.conn())
        .await?;
        Ok(())
    }

    async fn unexclude(&mut self, task: TaskId, pr: PullRequestId) -> Result<bool> {
        let done = sqlx::query(
            "DELETE FROM task_pull_request_exclusion WHERE task_id = $1 AND pull_request_id = $2",
        )
        .bind(task.to_string())
        .bind(pr.to_string())
        .execute(self.conn())
        .await?;
        Ok(done.rows_affected() > 0)
    }

    async fn is_excluded(&mut self, task: TaskId, pr: PullRequestId) -> Result<bool> {
        Ok(sqlx::query(
            "SELECT 1 FROM task_pull_request_exclusion WHERE task_id = $1 AND pull_request_id = $2",
        )
        .bind(task.to_string())
        .bind(pr.to_string())
        .fetch_optional(self.conn())
        .await?
        .is_some())
    }
}
