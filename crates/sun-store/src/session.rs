//! `runtime`, `account_cooldown`, `session` and `handoff_pack`:
//! the tool rows steps on a CLI's own login point at, the cooldowns of the
//! accounts that hit a limit (they survive a restart), the sessions steps run
//! in, and the hand-off packs between them.

use async_trait::async_trait;
use sqlx::Row;
use sqlx::sqlite::SqliteRow;
use sun_core::model::{HandoffPack, Runtime, RuntimeSource};
use sun_core::{HostId, RuntimeId, SessionHandle, SessionId, Timestamp, WorkspaceId};

use crate::repo::{parse, parse_ts, ts};
use crate::{Result, StoreError, Tx};

/// **Repository** for `runtime` and `account_cooldown` rows. A runtime
/// belongs to one workspace; names are unique per workspace. Only tool rows
/// (`byo_runtime`) are written: key-backed ones are [`crate::CredentialRepo`]'s.
#[async_trait]
pub trait RuntimeRepo {
    /// `workspace`'s runtime named `name` (a `runtime.chain` entry there);
    /// created as `{name, adapter, source}` when missing. An existing row is
    /// kept.
    async fn upsert_runtime(
        &mut self,
        workspace: WorkspaceId,
        name: &str,
        adapter: &str,
        source: RuntimeSource,
    ) -> Result<Runtime>;
    /// `workspace`'s runtime named `name`, if configured.
    async fn runtime_by_name(
        &mut self,
        workspace: WorkspaceId,
        name: &str,
    ) -> Result<Option<Runtime>>;
    /// One runtime, or `NotFound`.
    async fn get_runtime(&mut self, id: RuntimeId) -> Result<Runtime>;
    /// Cools `account` ([`sun_core::model::Account::key`]) down until
    /// `until` (`None`: ready again).
    async fn set_cooldown(&mut self, account: &str, until: Option<Timestamp>) -> Result<()>;
    /// `(account, until)` of every account cooling down, ended ones included.
    async fn cooldowns(&mut self) -> Result<Vec<(String, Timestamp)>>;
}

/// **Repository** for `session` and `handoff_pack` rows.
#[async_trait]
pub trait SessionRepo {
    /// A session chosen for a step on `host` (before the host call).
    async fn insert_session(&mut self, id: SessionId, host: HostId) -> Result<()>;
    /// The host started it: its native id, pid and worktree.
    async fn set_session_started(&mut self, handle: &SessionHandle, worktree: &str) -> Result<()>;
    /// The session's handle (`native_id`/`pid` once started), or `NotFound`.
    async fn get_session(&mut self, id: SessionId) -> Result<SessionHandle>;
    /// Stores a hand-off pack row (its JSON is a blob put before the `Tx`).
    async fn insert_handoff_pack(&mut self, pack: &HandoffPack) -> Result<()>;
}

#[async_trait]
impl RuntimeRepo for Tx {
    async fn upsert_runtime(
        &mut self,
        workspace: WorkspaceId,
        name: &str,
        adapter: &str,
        source: RuntimeSource,
    ) -> Result<Runtime> {
        sqlx::query(
            "INSERT INTO runtime (id, workspace_id, name, adapter, source) \
             VALUES ($1, $2, $3, $4, $5) ON CONFLICT (workspace_id, name) DO NOTHING",
        )
        .bind(RuntimeId::generate().to_string())
        .bind(workspace.to_string())
        .bind(name)
        .bind(adapter)
        .bind(source.as_str())
        .execute(self.conn())
        .await?;
        self.runtime_by_name(workspace, name)
            .await?
            .ok_or_else(|| StoreError::not_found("runtime", name))
    }

    async fn runtime_by_name(
        &mut self,
        workspace: WorkspaceId,
        name: &str,
    ) -> Result<Option<Runtime>> {
        sqlx::query(&format!(
            "SELECT {RUNTIME_COLUMNS} FROM runtime WHERE workspace_id = $1 AND name = $2"
        ))
        .bind(workspace.to_string())
        .bind(name)
        .fetch_optional(self.conn())
        .await?
        .map(|row| runtime_from(&row))
        .transpose()
    }

    async fn get_runtime(&mut self, id: RuntimeId) -> Result<Runtime> {
        let row = sqlx::query(&format!(
            "SELECT {RUNTIME_COLUMNS} FROM runtime WHERE id = $1"
        ))
        .bind(id.to_string())
        .fetch_optional(self.conn())
        .await?
        .ok_or_else(|| StoreError::not_found("runtime", id))?;
        runtime_from(&row)
    }

    async fn set_cooldown(&mut self, account: &str, until: Option<Timestamp>) -> Result<()> {
        let query = match until {
            Some(until) => sqlx::query(
                "INSERT INTO account_cooldown (account, until) VALUES ($1, $2) \
                 ON CONFLICT (account) DO UPDATE SET until = excluded.until",
            )
            .bind(account)
            .bind(ts(&until)),
            None => sqlx::query("DELETE FROM account_cooldown WHERE account = $1").bind(account),
        };
        query.execute(self.conn()).await?;
        Ok(())
    }

    async fn cooldowns(&mut self) -> Result<Vec<(String, Timestamp)>> {
        let rows = sqlx::query("SELECT account, until FROM account_cooldown ORDER BY account")
            .fetch_all(self.conn())
            .await?;
        rows.iter()
            .map(|row| Ok((row.try_get("account")?, parse_ts(row.try_get("until")?)?)))
            .collect()
    }
}

#[async_trait]
impl SessionRepo for Tx {
    async fn insert_session(&mut self, id: SessionId, host: HostId) -> Result<()> {
        sqlx::query("INSERT INTO session (id, host_id) VALUES ($1, $2)")
            .bind(id.to_string())
            .bind(host.to_string())
            .execute(self.conn())
            .await?;
        Ok(())
    }

    async fn set_session_started(&mut self, handle: &SessionHandle, worktree: &str) -> Result<()> {
        let done =
            sqlx::query("UPDATE session SET native_id = $1, pid = $2, worktree = $3 WHERE id = $4")
                .bind(handle.native_id.as_deref())
                .bind(handle.pid.map(i64::from))
                .bind(worktree)
                .bind(handle.session_id.to_string())
                .execute(self.conn())
                .await?;
        if done.rows_affected() == 0 {
            return Err(StoreError::not_found("session", handle.session_id));
        }
        Ok(())
    }

    async fn get_session(&mut self, id: SessionId) -> Result<SessionHandle> {
        let row = sqlx::query("SELECT native_id, pid FROM session WHERE id = $1")
            .bind(id.to_string())
            .fetch_optional(self.conn())
            .await?
            .ok_or_else(|| StoreError::not_found("session", id))?;
        let pid: Option<i64> = row.try_get("pid")?;
        Ok(SessionHandle {
            session_id: id,
            native_id: row.try_get("native_id")?,
            pid: pid.and_then(|p| u32::try_from(p).ok()),
        })
    }

    async fn insert_handoff_pack(&mut self, pack: &HandoffPack) -> Result<()> {
        sqlx::query(
            "INSERT INTO handoff_pack (id, run_id, from_session, to_session, pack_hash) \
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(pack.id.to_string())
        .bind(pack.run_id.to_string())
        .bind(pack.from_session.to_string())
        .bind(pack.to_session.map(|s| s.to_string()))
        .bind(pack.pack_hash.as_str())
        .execute(self.conn())
        .await?;
        Ok(())
    }
}

const RUNTIME_COLUMNS: &str = "id, workspace_id, name, adapter, source";

fn runtime_from(row: &SqliteRow) -> Result<Runtime> {
    Ok(Runtime {
        id: parse(row.try_get("id")?)?,
        workspace_id: parse(row.try_get("workspace_id")?)?,
        name: row.try_get("name")?,
        adapter: row.try_get("adapter")?,
        source: parse(row.try_get("source")?)?,
    })
}
