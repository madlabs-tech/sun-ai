//! `runtime`, `runtime_state`, `session` and `handoff_pack`:
//! which runtime a chain name means, its cooldown (survives a restart), the
//! sessions steps run in, and the hand-off packs between them.

use async_trait::async_trait;
use sqlx::Row;
use sqlx::sqlite::SqliteRow;
use sun_core::model::{HandoffPack, Runtime, RuntimeSource, RuntimeStatus};
use sun_core::{HostId, RuntimeId, SessionHandle, SessionId, Timestamp};

use crate::repo::{json, parse, parse_ts, ts};
use crate::{Result, StoreError, Tx};

/// **Repository** for `runtime` and `runtime_state` rows.
#[async_trait]
pub trait RuntimeRepo {
    /// The runtime named `name` (a workflow `runtime.chain` entry); created
    /// as `{name, adapter, source}` when missing. An existing row is kept.
    async fn upsert_runtime(
        &mut self,
        name: &str,
        adapter: &str,
        source: RuntimeSource,
    ) -> Result<Runtime>;
    /// The runtime named `name`, if configured.
    async fn runtime_by_name(&mut self, name: &str) -> Result<Option<Runtime>>;
    /// Every runtime row, by name.
    async fn runtimes(&mut self) -> Result<Vec<Runtime>>;
    /// Stores a configured runtime (validated by the caller); `false` when
    /// the name is taken.
    async fn insert_runtime(&mut self, runtime: &Runtime) -> Result<bool>;
    /// Replaces the adapter, source and config of the runtime named
    /// `runtime.name`; `false` when there is none.
    async fn update_runtime(&mut self, runtime: &Runtime) -> Result<bool>;
    /// One runtime, or `NotFound`.
    async fn get_runtime(&mut self, id: RuntimeId) -> Result<Runtime>;
    /// Cools runtime `id` down until `until` (`None`: ready again).
    async fn set_cooldown(&mut self, id: RuntimeId, until: Option<Timestamp>) -> Result<()>;
    /// `(runtime name, cooldown_until)` of every runtime cooling down.
    async fn cooldowns(&mut self) -> Result<Vec<(String, Timestamp)>>;
    /// Sets the permission mode sessions of `adapter` get (`runtime_policy`).
    async fn set_permission_mode(&mut self, adapter: &str, mode: &str) -> Result<()>;
    /// `(adapter, permission mode)` of every adapter with one set.
    async fn permission_modes(&mut self) -> Result<Vec<(String, String)>>;
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
        name: &str,
        adapter: &str,
        source: RuntimeSource,
    ) -> Result<Runtime> {
        sqlx::query(
            "INSERT INTO runtime (id, name, adapter, source) VALUES ($1, $2, $3, $4) \
             ON CONFLICT (name) DO NOTHING",
        )
        .bind(RuntimeId::generate().to_string())
        .bind(name)
        .bind(adapter)
        .bind(source.as_str())
        .execute(self.conn())
        .await?;
        self.runtime_by_name(name)
            .await?
            .ok_or_else(|| StoreError::not_found("runtime", name))
    }

    async fn runtime_by_name(&mut self, name: &str) -> Result<Option<Runtime>> {
        sqlx::query(&format!(
            "SELECT {RUNTIME_COLUMNS} FROM runtime WHERE name = $1"
        ))
        .bind(name)
        .fetch_optional(self.conn())
        .await?
        .map(|row| runtime_from(&row))
        .transpose()
    }

    async fn runtimes(&mut self) -> Result<Vec<Runtime>> {
        let rows = sqlx::query(&format!(
            "SELECT {RUNTIME_COLUMNS} FROM runtime ORDER BY name"
        ))
        .fetch_all(self.conn())
        .await?;
        rows.iter().map(runtime_from).collect()
    }

    async fn insert_runtime(&mut self, runtime: &Runtime) -> Result<bool> {
        let done = sqlx::query(&format!(
            "INSERT INTO runtime ({RUNTIME_COLUMNS}) VALUES ($1, $2, $3, $4, $5) \
             ON CONFLICT (name) DO NOTHING"
        ))
        .bind(runtime.id.to_string())
        .bind(runtime.name.as_str())
        .bind(runtime.adapter.as_str())
        .bind(runtime.source.as_str())
        .bind(config_json(runtime)?)
        .execute(self.conn())
        .await?;
        Ok(done.rows_affected() > 0)
    }

    async fn update_runtime(&mut self, runtime: &Runtime) -> Result<bool> {
        let done = sqlx::query(
            "UPDATE runtime SET adapter = $1, source = $2, config_json = $3 WHERE name = $4",
        )
        .bind(runtime.adapter.as_str())
        .bind(runtime.source.as_str())
        .bind(config_json(runtime)?)
        .bind(runtime.name.as_str())
        .execute(self.conn())
        .await?;
        Ok(done.rows_affected() > 0)
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

    async fn set_cooldown(&mut self, id: RuntimeId, until: Option<Timestamp>) -> Result<()> {
        let status = match until {
            Some(_) => RuntimeStatus::CoolingDown,
            None => RuntimeStatus::Ready,
        };
        sqlx::query(
            "INSERT INTO runtime_state (runtime_id, status, cooldown_until) VALUES ($1, $2, $3) \
             ON CONFLICT (runtime_id) DO UPDATE SET status = excluded.status, \
             cooldown_until = excluded.cooldown_until",
        )
        .bind(id.to_string())
        .bind(status.as_str())
        .bind(until.as_ref().map(ts))
        .execute(self.conn())
        .await?;
        Ok(())
    }

    async fn cooldowns(&mut self) -> Result<Vec<(String, Timestamp)>> {
        let rows = sqlx::query(
            "SELECT r.name, s.cooldown_until FROM runtime_state s \
             JOIN runtime r ON r.id = s.runtime_id \
             WHERE s.status = $1 AND s.cooldown_until IS NOT NULL ORDER BY r.name",
        )
        .bind(RuntimeStatus::CoolingDown.as_str())
        .fetch_all(self.conn())
        .await?;
        rows.iter()
            .map(|row| {
                Ok((
                    row.try_get("name")?,
                    parse_ts(row.try_get("cooldown_until")?)?,
                ))
            })
            .collect()
    }

    async fn set_permission_mode(&mut self, adapter: &str, mode: &str) -> Result<()> {
        sqlx::query(
            "INSERT INTO runtime_policy (adapter, permission_mode) VALUES (?, ?) \
             ON CONFLICT (adapter) DO UPDATE SET permission_mode = excluded.permission_mode",
        )
        .bind(adapter)
        .bind(mode)
        .execute(self.conn())
        .await?;
        Ok(())
    }

    async fn permission_modes(&mut self) -> Result<Vec<(String, String)>> {
        let rows =
            sqlx::query("SELECT adapter, permission_mode FROM runtime_policy ORDER BY adapter")
                .fetch_all(self.conn())
                .await?;
        rows.iter()
            .map(|row| Ok((row.try_get("adapter")?, row.try_get("permission_mode")?)))
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

const RUNTIME_COLUMNS: &str = "id, name, adapter, source, config_json";

fn config_json(runtime: &Runtime) -> Result<Option<String>> {
    Ok(runtime
        .byo_key
        .as_ref()
        .map(serde_json::to_string)
        .transpose()?)
}

fn runtime_from(row: &SqliteRow) -> Result<Runtime> {
    let config: Option<String> = row.try_get("config_json")?;
    Ok(Runtime {
        id: parse(row.try_get("id")?)?,
        name: row.try_get("name")?,
        adapter: row.try_get("adapter")?,
        source: parse(row.try_get("source")?)?,
        byo_key: config.map(json).transpose()?,
    })
}
