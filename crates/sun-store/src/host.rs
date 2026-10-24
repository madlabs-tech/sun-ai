//! `host`: enrolled runtime hosts, deployment rows a workspace attaches
//! (`workspace_host`, [`crate::WorkspaceItem::Host`]), `host_token`, the
//! unused enrolment tokens by hash, and `host_runtime`, the `(host, tool)`
//! rows projected from each host's capabilities report, and `host_share`,
//! the groups a host is shared with. A `workspace_host` link made through a
//! share counts only while the share still holds ([`live_links`]).
//! No host secret is stored here; the API derives and checks host keys
//! itself.

use async_trait::async_trait;
use serde_json::Value;
use sqlx::Row;
use sqlx::sqlite::SqliteRow;
use sun_core::model::{Host, HostRuntime, HostShare};
use sun_core::{GroupId, HostId, HostRuntimeId, MemberId, Timestamp, WorkspaceId};

use crate::member::second;
use crate::repo::{json, parse, parse_opt, parse_ts, parse_ts_opt, ts, u32_at};
use crate::{GroupRepo, Result, StoreError, Tx};

/// **Repository** for `host` rows.
#[async_trait]
pub trait HostRepo {
    /// Inserts an enrolled host.
    async fn insert_host(&mut self, host: &Host) -> Result<()>;
    /// The host, or `NotFound` (e.g. it was removed).
    async fn get_host(&mut self, id: HostId) -> Result<Host>;
    /// Every host, oldest first.
    async fn hosts(&mut self) -> Result<Vec<Host>>;
    /// The hosts attached to `workspace`, oldest first.
    async fn hosts_in(&mut self, workspace: WorkspaceId) -> Result<Vec<Host>>;
    /// Stores the capabilities the host reported in its `hello` or
    /// `capabilities` frame and rebuilds its `host_runtime` rows from them.
    async fn set_host_capabilities(
        &mut self,
        id: HostId,
        capabilities: &serde_json::Value,
    ) -> Result<()>;
    /// Saves an enrolment token by its hash, with what it `enrols`, and
    /// prunes the ones expired over a day ago.
    async fn insert_host_token(
        &mut self,
        token_hash: &str,
        enrols: &Value,
        expires_at: Timestamp,
    ) -> Result<()>;
    /// Deletes the token: what it enrols and when it expires, or `None`
    /// when it is unknown or already used.
    async fn take_host_token(&mut self, token_hash: &str) -> Result<Option<(Value, Timestamp)>>;
    /// The `(host, tool)` rows of the hosts attached to `workspace` (`None`:
    /// every host) that the latest report still names, by host then tool.
    async fn host_runtimes(&mut self, workspace: Option<WorkspaceId>) -> Result<Vec<HostRuntime>>;
    /// Attaches `host` to `workspace`: directly (`shared_via` `None`), or
    /// through a share by member `shared_via`. A direct link stays direct;
    /// a link through a share becomes direct, or is taken over by the new
    /// member (a link whose share lapsed comes back that way).
    async fn attach_host(
        &mut self,
        workspace: WorkspaceId,
        host: HostId,
        shared_via: Option<MemberId>,
    ) -> Result<()>;
    /// Shares `host` with `group`; `false` when it already was.
    async fn share_host(
        &mut self,
        host: HostId,
        group: GroupId,
        shared_by: Option<MemberId>,
    ) -> Result<bool>;
    /// Stops sharing `host` with `group`; `false` when it wasn't.
    async fn unshare_host(&mut self, host: HostId, group: GroupId) -> Result<bool>;
    /// The groups `host` is shared with, oldest first.
    async fn host_shares(&mut self, host: HostId) -> Result<Vec<HostShare>>;
    /// The hosts shared with a group `member` is in right now, by id.
    async fn hosts_shared_with(&mut self, member: MemberId) -> Result<Vec<HostId>>;
    /// Every workspace `host` is linked to, also through a share that no
    /// longer holds (which would hold again on a new share).
    async fn host_links(&mut self, host: HostId) -> Result<Vec<WorkspaceId>>;
}

/// The `workspace_host` links of `workspace` (or of `host`) that give the
/// workspace the host now: those attached directly, and those attached
/// through a share while the host is still shared with a group the member
/// who attached it is in ([`GroupRepo::groups_of`], the one membership
/// rule). Every read of the link goes through this.
pub(crate) async fn live_links(
    tx: &mut Tx,
    workspace: Option<WorkspaceId>,
    host: Option<HostId>,
) -> Result<Vec<(WorkspaceId, HostId)>> {
    let rows: Vec<(String, String, Option<String>)> = sqlx::query_as(
        "SELECT workspace_id, host_id, shared_via FROM workspace_host \
         WHERE ($1 IS NULL OR workspace_id = $1) AND ($2 IS NULL OR host_id = $2) \
         ORDER BY workspace_id, host_id",
    )
    .bind(workspace.map(|w| w.to_string()))
    .bind(host.map(|h| h.to_string()))
    .fetch_all(tx.conn())
    .await?;
    // Per member who attached through a share: the hosts shared with them.
    let mut shared: Vec<(MemberId, Vec<HostId>)> = Vec::new();
    let mut live = Vec::with_capacity(rows.len());
    for (workspace, host, via) in rows {
        let (workspace, host): (WorkspaceId, HostId) = (parse(workspace)?, parse(host)?);
        if let Some(member) = via.map(parse::<MemberId>).transpose()? {
            let known = shared.iter().position(|(m, _)| *m == member);
            let at = match known {
                Some(at) => at,
                None => {
                    shared.push((member, tx.hosts_shared_with(member).await?));
                    shared.len() - 1
                }
            };
            if !shared[at].1.contains(&host) {
                continue;
            }
        }
        live.push((workspace, host));
    }
    Ok(live)
}

/// `ids` as a JSON array, for `IN (SELECT value FROM json_each($n))`.
fn id_list<T: ToString>(ids: impl IntoIterator<Item = T>) -> String {
    Value::from(ids.into_iter().map(|id| id.to_string()).collect::<Vec<_>>()).to_string()
}

#[async_trait]
impl HostRepo for Tx {
    async fn insert_host(&mut self, host: &Host) -> Result<()> {
        sqlx::query(
            "INSERT INTO host (id, name, capabilities_json, enrolled_by) VALUES ($1, $2, $3, $4)",
        )
        .bind(host.id.to_string())
        .bind(&host.name)
        .bind(serde_json::to_string(&host.capabilities)?)
        .bind(host.enrolled_by.map(|m| m.to_string()))
        .execute(self.conn())
        .await?;
        project_host_runtimes(self.conn(), &host.id.to_string(), &host.capabilities).await
    }

    async fn get_host(&mut self, id: HostId) -> Result<Host> {
        let row = sqlx::query(&format!("SELECT {HOST_COLUMNS} FROM host WHERE id = $1"))
            .bind(id.to_string())
            .fetch_optional(self.conn())
            .await?
            .ok_or_else(|| StoreError::not_found("host", id))?;
        host_from(&row)
    }

    async fn hosts(&mut self) -> Result<Vec<Host>> {
        sqlx::query(&format!("SELECT {HOST_COLUMNS} FROM host ORDER BY id"))
            .fetch_all(self.conn())
            .await?
            .iter()
            .map(host_from)
            .collect()
    }

    async fn hosts_in(&mut self, workspace: WorkspaceId) -> Result<Vec<Host>> {
        let live = live_links(self, Some(workspace), None).await?;
        sqlx::query(&format!(
            "SELECT {HOST_COLUMNS} FROM host \
             WHERE id IN (SELECT value FROM json_each($1)) ORDER BY id"
        ))
        .bind(id_list(live.iter().map(|(_, host)| host)))
        .fetch_all(self.conn())
        .await?
        .iter()
        .map(host_from)
        .collect()
    }

    async fn set_host_capabilities(
        &mut self,
        id: HostId,
        capabilities: &serde_json::Value,
    ) -> Result<()> {
        let done = sqlx::query("UPDATE host SET capabilities_json = $1 WHERE id = $2")
            .bind(serde_json::to_string(capabilities)?)
            .bind(id.to_string())
            .execute(self.conn())
            .await?;
        if done.rows_affected() == 0 {
            return Err(StoreError::not_found("host", id));
        }
        project_host_runtimes(self.conn(), &id.to_string(), capabilities).await
    }

    async fn insert_host_token(
        &mut self,
        token_hash: &str,
        enrols: &Value,
        expires_at: Timestamp,
    ) -> Result<()> {
        let prune_before = chrono::Utc::now() - chrono::Duration::days(1);
        sqlx::query("DELETE FROM host_token WHERE expires_at <= $1")
            .bind(second(&prune_before))
            .execute(self.conn())
            .await?;
        sqlx::query(
            "INSERT INTO host_token (token_hash, enrols_json, expires_at) VALUES ($1, $2, $3)",
        )
        .bind(token_hash)
        .bind(serde_json::to_string(enrols)?)
        .bind(second(&expires_at))
        .execute(self.conn())
        .await?;
        Ok(())
    }

    async fn take_host_token(&mut self, token_hash: &str) -> Result<Option<(Value, Timestamp)>> {
        let row = sqlx::query(
            "DELETE FROM host_token WHERE token_hash = $1 RETURNING enrols_json, expires_at",
        )
        .bind(token_hash)
        .fetch_optional(self.conn())
        .await?;
        row.map(|row| {
            Ok((
                json(row.try_get("enrols_json")?)?,
                parse_ts(row.try_get("expires_at")?)?,
            ))
        })
        .transpose()
    }

    async fn host_runtimes(&mut self, workspace: Option<WorkspaceId>) -> Result<Vec<HostRuntime>> {
        let hosts = match workspace {
            Some(workspace) => {
                let live = live_links(self, Some(workspace), None).await?;
                Some(id_list(live.iter().map(|(_, host)| host)))
            }
            None => None,
        };
        sqlx::query(&format!(
            "SELECT {HOST_RUNTIME_COLUMNS} FROM host_runtime WHERE removed_at IS NULL \
             AND ($1 IS NULL OR host_id IN (SELECT value FROM json_each($1))) \
             ORDER BY host_id, tool"
        ))
        .bind(hosts)
        .fetch_all(self.conn())
        .await?
        .iter()
        .map(host_runtime_from)
        .collect()
    }

    async fn attach_host(
        &mut self,
        workspace: WorkspaceId,
        host: HostId,
        shared_via: Option<MemberId>,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO workspace_host (workspace_id, host_id, shared_via) VALUES ($1, $2, $3) \
             ON CONFLICT (workspace_id, host_id) DO UPDATE SET shared_via = \
                 CASE WHEN workspace_host.shared_via IS NULL OR excluded.shared_via IS NULL \
                      THEN NULL ELSE excluded.shared_via END",
        )
        .bind(workspace.to_string())
        .bind(host.to_string())
        .bind(shared_via.map(|m| m.to_string()))
        .execute(self.conn())
        .await?;
        Ok(())
    }

    async fn share_host(
        &mut self,
        host: HostId,
        group: GroupId,
        shared_by: Option<MemberId>,
    ) -> Result<bool> {
        let done = sqlx::query(
            "INSERT INTO host_share (host_id, group_id, shared_by, created_at) \
             VALUES ($1, $2, $3, $4) ON CONFLICT DO NOTHING",
        )
        .bind(host.to_string())
        .bind(group.to_string())
        .bind(shared_by.map(|m| m.to_string()))
        .bind(ts(&chrono::Utc::now()))
        .execute(self.conn())
        .await?;
        Ok(done.rows_affected() > 0)
    }

    async fn unshare_host(&mut self, host: HostId, group: GroupId) -> Result<bool> {
        let done = sqlx::query("DELETE FROM host_share WHERE host_id = $1 AND group_id = $2")
            .bind(host.to_string())
            .bind(group.to_string())
            .execute(self.conn())
            .await?;
        Ok(done.rows_affected() > 0)
    }

    async fn host_shares(&mut self, host: HostId) -> Result<Vec<HostShare>> {
        sqlx::query(
            "SELECT host_id, group_id, shared_by, created_at FROM host_share \
             WHERE host_id = $1 ORDER BY created_at, group_id",
        )
        .bind(host.to_string())
        .fetch_all(self.conn())
        .await?
        .iter()
        .map(|row| {
            Ok(HostShare {
                host_id: parse(row.try_get("host_id")?)?,
                group_id: parse(row.try_get("group_id")?)?,
                shared_by: parse_opt(row.try_get("shared_by")?)?,
                created_at: parse_ts(row.try_get("created_at")?)?,
            })
        })
        .collect()
    }

    async fn hosts_shared_with(&mut self, member: MemberId) -> Result<Vec<HostId>> {
        let groups = self.groups_of(member).await?;
        let ids: Vec<String> = sqlx::query_scalar(
            "SELECT DISTINCT host_id FROM host_share \
             WHERE group_id IN (SELECT value FROM json_each($1)) ORDER BY host_id",
        )
        .bind(id_list(groups.iter().map(|g| g.id)))
        .fetch_all(self.conn())
        .await?;
        ids.into_iter().map(parse).collect()
    }

    async fn host_links(&mut self, host: HostId) -> Result<Vec<WorkspaceId>> {
        let ids: Vec<String> = sqlx::query_scalar(
            "SELECT workspace_id FROM workspace_host WHERE host_id = $1 ORDER BY workspace_id",
        )
        .bind(host.to_string())
        .fetch_all(self.conn())
        .await?;
        ids.into_iter().map(parse).collect()
    }
}

const HOST_RUNTIME_COLUMNS: &str = "id, host_id, tool, family, version, installed, auth, detail, \
                                    max_sessions, probed_at, removed_at";

fn host_runtime_from(row: &SqliteRow) -> Result<HostRuntime> {
    Ok(HostRuntime {
        id: parse(row.try_get("id")?)?,
        host_id: parse(row.try_get("host_id")?)?,
        tool: row.try_get("tool")?,
        family: row.try_get("family")?,
        version: row.try_get("version")?,
        installed: row.try_get("installed")?,
        auth: row.try_get("auth")?,
        detail: row.try_get("detail")?,
        max_sessions: match row.try_get::<Option<i64>, _>("max_sessions")? {
            Some(_) => Some(u32_at(row, "max_sessions")?),
            None => None,
        },
        probed_at: parse_ts(row.try_get("probed_at")?)?,
        removed_at: parse_ts_opt(row.try_get("removed_at")?)?,
    })
}

/// Rebuilds a host's `host_runtime` rows from one capabilities report, in
/// the caller's transaction: every tool the report names is upserted
/// (`removed_at` cleared, same row id), every row it no longer names gets
/// `removed_at` now. Reads the report leniently (`runtimes[].adapter`,
/// `readiness`, and `family` / `max_sessions` when a host sends them), so an
/// older or empty report (`{}`) projects to what it says.
pub(crate) async fn project_host_runtimes(
    conn: &mut sqlx::SqliteConnection,
    host_id: &str,
    capabilities: &Value,
) -> Result<()> {
    let now = ts(&chrono::Utc::now());
    sqlx::query(
        "UPDATE host_runtime SET removed_at = $1 WHERE host_id = $2 AND removed_at IS NULL",
    )
    .bind(&now)
    .bind(host_id)
    .execute(&mut *conn)
    .await?;
    let runtimes = capabilities
        .get("runtimes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten();
    for runtime in runtimes {
        let Some(tool) = runtime.get("adapter").and_then(Value::as_str) else {
            continue;
        };
        let readiness = runtime.get("readiness");
        let flag = |key: &str| readiness.and_then(|r| r.get(key)) == Some(&Value::Bool(true));
        let text = |key: &str| {
            readiness
                .and_then(|r| r.get(key))
                .and_then(Value::as_str)
                .map(str::to_owned)
        };
        let installed = flag("installed");
        let auth = match (installed, flag("logged_in")) {
            (false, _) => "unknown",
            (true, true) => "logged_in",
            (true, false) => "logged_out",
        };
        sqlx::query(
            "INSERT INTO host_runtime (id, host_id, tool, family, version, installed, auth, \
                                       detail, max_sessions, probed_at, removed_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, NULL) \
             ON CONFLICT (host_id, tool) DO UPDATE SET \
                 family = excluded.family, version = excluded.version, \
                 installed = excluded.installed, auth = excluded.auth, \
                 detail = excluded.detail, max_sessions = excluded.max_sessions, \
                 probed_at = excluded.probed_at, removed_at = NULL",
        )
        .bind(HostRuntimeId::generate().to_string())
        .bind(host_id)
        .bind(tool)
        .bind(runtime.get("family").and_then(Value::as_str))
        .bind(text("version"))
        .bind(installed)
        .bind(auth)
        .bind(text("detail"))
        .bind(runtime.get("max_sessions").and_then(Value::as_i64))
        .bind(&now)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

/// `20261104090300_host_runtime_backfill`: projects every host's stored
/// `capabilities_json` into `host_runtime` once, when the table is new.
pub(crate) async fn backfill_host_runtimes(conn: &mut sqlx::SqliteConnection) -> Result<()> {
    let hosts: Vec<(String, String)> =
        sqlx::query_as("SELECT id, capabilities_json FROM host ORDER BY id")
            .fetch_all(&mut *conn)
            .await?;
    for (host_id, capabilities) in hosts {
        let capabilities: Value = json(capabilities)?;
        project_host_runtimes(conn, &host_id, &capabilities).await?;
    }
    Ok(())
}

const HOST_COLUMNS: &str = "id, name, capabilities_json, enrolled_by";

fn host_from(row: &SqliteRow) -> Result<Host> {
    Ok(Host {
        id: parse(row.try_get("id")?)?,
        name: row.try_get("name")?,
        capabilities: json(row.try_get("capabilities_json")?)?,
        enrolled_by: parse_opt(row.try_get("enrolled_by")?)?,
    })
}
