//! `workspace`: named workspaces. A workspace owns its workflows
//! (`workflow.workspace_id`), tasks (`task.workspace_id`) and repos
//! (`repo.workspace_id`). Knowledge sources are linked to workspaces
//! (`workspace_knowledge_source`): a source imported into another workspace
//! is the same row. So are hosts (`workspace_host`: a workspace's runs go only
//! to its hosts) and catalog installs (`workspace_catalog_install`: the files
//! are shared on disk). Issue prefixes are unique in the ecosystem, ignoring
//! case.

use async_trait::async_trait;
use sha2::{Digest, Sha256};
use sqlx::Row;
use sqlx::sqlite::{SqliteConnection, SqliteRow};
use sun_core::model::Workspace;
use sun_core::{HostId, MemberId, WorkspaceId};

use crate::repo::{json, parse, parse_ts, ts};
use crate::{BlobStore, Result, StoreError, Tx};

/// **Repository** for `workspace` rows. Fields are checked with
/// [`Workspace`]'s `clean_*` helpers by the caller; here names must be unique
/// ignoring case, and slugs unique (and never changed).
#[async_trait]
pub trait WorkspaceRepo {
    /// Inserts a workspace; `InvalidInput` when another workspace has the
    /// name, the slug or the issue prefix.
    async fn insert_workspace(&mut self, workspace: &Workspace) -> Result<()>;
    /// `base`, or the first free `base-2`, `base-3`, … for a new workspace.
    async fn free_slug(&mut self, base: &str) -> Result<String>;
    /// `base`, or the first free `base2`, `base3`, … issue prefix (ignoring case).
    async fn free_prefix(&mut self, base: &str) -> Result<String>;
    /// Sets the workspace's logo, `(content_type, bytes)` already checked by
    /// [`Workspace::check_logo`], or removes it (`None`). The image lives in
    /// its own row (`workspace_logo`), gone with the workspace; `NotFound`
    /// for a missing workspace.
    async fn set_logo(&mut self, workspace: WorkspaceId, logo: Option<(&str, &[u8])>)
    -> Result<()>;
    /// The logo's content type and bytes, if it has one.
    async fn logo(&mut self, workspace: WorkspaceId) -> Result<Option<(String, Vec<u8>)>>;
    /// One workspace, or `NotFound`.
    async fn get_workspace(&mut self, id: WorkspaceId) -> Result<Workspace>;
    /// Every workspace, oldest first (Main first).
    async fn workspaces(&mut self) -> Result<Vec<Workspace>>;
    /// Saves a workspace's name, description, context, issue prefix, default
    /// workflow and git settings (never its slug, counter or logo);
    /// `NotFound` or `InvalidInput` (name or prefix taken).
    async fn update_workspace(&mut self, workspace: &Workspace) -> Result<()>;
    /// Deletes a workspace with no workflows and no tasks; `InvalidInput`
    /// when it has some, `NotFound` when missing.
    async fn delete_workspace(&mut self, id: WorkspaceId) -> Result<()>;
    /// Links `item` to `workspace`; a no-op when it already is.
    async fn link(&mut self, workspace: WorkspaceId, item: WorkspaceItem<'_>) -> Result<()>;
    /// Unlinks `item` from `workspace`; `false` when it wasn't linked.
    async fn unlink(&mut self, workspace: WorkspaceId, item: WorkspaceItem<'_>) -> Result<bool>;
    /// The workspaces `item` is linked to, oldest first.
    async fn workspaces_of(&mut self, item: WorkspaceItem<'_>) -> Result<Vec<WorkspaceId>>;
    /// The workspaces `member` is a member of (`workspace_member`), by id.
    async fn workspaces_of_member(&mut self, member: MemberId) -> Result<Vec<WorkspaceId>>;
    /// The catalog ids `workspace` installed, sorted.
    async fn catalog_installs(&mut self, workspace: WorkspaceId) -> Result<Vec<String>>;
}

/// An item a workspace links to (shared rows, unlike its own repos).
#[derive(Debug, Clone, Copy)]
pub enum WorkspaceItem<'a> {
    /// A `knowledge_source` row.
    KnowledgeSource(&'a str),
    /// A `host` row: the workspace's runs may go to it.
    Host(HostId),
    /// A catalog entry's install (its files are shared on disk).
    CatalogInstall(&'a str),
}

impl WorkspaceItem<'_> {
    /// The link table, its item column and the item's id.
    fn link(self) -> (&'static str, &'static str, String) {
        match self {
            WorkspaceItem::KnowledgeSource(id) => {
                ("workspace_knowledge_source", "source_id", id.to_owned())
            }
            WorkspaceItem::Host(id) => ("workspace_host", "host_id", id.to_string()),
            WorkspaceItem::CatalogInstall(id) => {
                ("workspace_catalog_install", "catalog_id", id.to_owned())
            }
        }
    }
}

#[async_trait]
impl WorkspaceRepo for Tx {
    async fn insert_workspace(&mut self, workspace: &Workspace) -> Result<()> {
        name_free(self, None, &workspace.name).await?;
        if slug_taken(self, &workspace.slug).await? {
            return Err(sun_core::Error::InvalidInput(format!(
                "A workspace with the slug `{}` already exists.",
                workspace.slug
            ))
            .into());
        }
        prefix_free(self, None, &workspace.issue_prefix).await?;
        sqlx::query(&format!(
            "INSERT INTO workspace ({WORKSPACE_COLUMNS}) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)"
        ))
        .bind(workspace.id.to_string())
        .bind(&workspace.name)
        .bind(&workspace.slug)
        .bind(&workspace.description)
        .bind(&workspace.context)
        .bind(&workspace.issue_prefix)
        .bind(workspace.issue_counter)
        .bind(workspace.default_workflow_id.map(|w| w.to_string()))
        .bind(serde_json::to_string(&workspace.git_settings)?)
        .bind(ts(&workspace.created_at))
        .bind(workspace.created_by.map(|m| m.to_string()))
        .execute(self.conn())
        .await?;
        Ok(())
    }

    async fn free_slug(&mut self, base: &str) -> Result<String> {
        let mut slug = base.to_owned();
        let mut n = 1;
        while slug_taken(self, &slug).await? {
            n += 1;
            slug = Workspace::slug_with_suffix(base, n);
        }
        Ok(slug)
    }

    async fn free_prefix(&mut self, base: &str) -> Result<String> {
        let mut prefix = base.to_owned();
        let mut n = 1;
        while prefix_owner(self, &prefix).await?.is_some() {
            n += 1;
            prefix = Workspace::prefix_with_suffix(base, n);
        }
        Ok(prefix)
    }

    async fn set_logo(
        &mut self,
        workspace: WorkspaceId,
        logo: Option<(&str, &[u8])>,
    ) -> Result<()> {
        self.get_workspace(workspace).await?;
        let Some((content_type, bytes)) = logo else {
            sqlx::query("DELETE FROM workspace_logo WHERE workspace_id = $1")
                .bind(workspace.to_string())
                .execute(self.conn())
                .await?;
            return Ok(());
        };
        put_logo(self.conn(), workspace, content_type, bytes).await
    }

    async fn logo(&mut self, workspace: WorkspaceId) -> Result<Option<(String, Vec<u8>)>> {
        Ok(
            sqlx::query_as(
                "SELECT content_type, bytes FROM workspace_logo WHERE workspace_id = $1",
            )
            .bind(workspace.to_string())
            .fetch_optional(self.conn())
            .await?,
        )
    }

    async fn get_workspace(&mut self, id: WorkspaceId) -> Result<Workspace> {
        let row = sqlx::query(&format!(
            "SELECT {WORKSPACE_READ} FROM workspace WHERE id = $1"
        ))
        .bind(id.to_string())
        .fetch_optional(self.conn())
        .await?
        .ok_or_else(|| StoreError::not_found("workspace", id))?;
        workspace_from(&row)
    }

    async fn workspaces(&mut self) -> Result<Vec<Workspace>> {
        sqlx::query(&format!(
            "SELECT {WORKSPACE_READ} FROM workspace ORDER BY id"
        ))
        .fetch_all(self.conn())
        .await?
        .iter()
        .map(workspace_from)
        .collect()
    }

    async fn update_workspace(&mut self, workspace: &Workspace) -> Result<()> {
        name_free(self, Some(workspace.id), &workspace.name).await?;
        prefix_free(self, Some(workspace.id), &workspace.issue_prefix).await?;
        let done = sqlx::query(
            "UPDATE workspace SET name = $1, description = $2, context = $3, \
             issue_prefix = $4, default_workflow_id = $5, git_settings_json = $7 WHERE id = $6",
        )
        .bind(&workspace.name)
        .bind(&workspace.description)
        .bind(&workspace.context)
        .bind(&workspace.issue_prefix)
        .bind(workspace.default_workflow_id.map(|w| w.to_string()))
        .bind(workspace.id.to_string())
        .bind(serde_json::to_string(&workspace.git_settings)?)
        .execute(self.conn())
        .await?;
        if done.rows_affected() == 0 {
            return Err(StoreError::not_found("workspace", workspace.id));
        }
        Ok(())
    }

    async fn delete_workspace(&mut self, id: WorkspaceId) -> Result<()> {
        let workflows: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM workflow WHERE workspace_id = $1 AND system_agent_id IS NULL",
        )
        .bind(id.to_string())
        .fetch_one(self.conn())
        .await?;
        if workflows > 0 {
            return Err(sun_core::Error::InvalidInput(format!(
                "The workspace still has {workflows} workflow(s). Move or delete them first."
            ))
            .into());
        }
        let tasks: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM task WHERE workspace_id = $1")
            .bind(id.to_string())
            .fetch_one(self.conn())
            .await?;
        if tasks > 0 {
            return Err(sun_core::Error::InvalidInput(format!(
                "The workspace still has {tasks} task(s)."
            ))
            .into());
        }
        // Agents' system workflows here ran no task (there is none): they go too.
        let system =
            "SELECT id FROM workflow WHERE workspace_id = $1 AND system_agent_id IS NOT NULL";
        // So do its agents, runtimes (their cooldowns and gateway models) and
        // memory.
        let runtimes = "SELECT id FROM runtime WHERE workspace_id = $1";
        for sql in [
            format!("DELETE FROM workflow_version WHERE workflow_id IN ({system})"),
            format!("DELETE FROM workflow WHERE id IN ({system})"),
            "DELETE FROM agent WHERE workspace_id = $1".to_owned(),
            format!("DELETE FROM runtime_state WHERE runtime_id IN ({runtimes})"),
            format!(
                "DELETE FROM kernel_setting \
                 WHERE key IN (SELECT 'credits_model:' || id FROM ({runtimes}))"
            ),
            "DELETE FROM runtime WHERE workspace_id = $1".to_owned(),
            "DELETE FROM memory_item WHERE workspace_id = $1".to_owned(),
        ] {
            sqlx::query(&sql)
                .bind(id.to_string())
                .execute(self.conn())
                .await?;
        }
        let done = sqlx::query("DELETE FROM workspace WHERE id = $1")
            .bind(id.to_string())
            .execute(self.conn())
            .await?;
        if done.rows_affected() == 0 {
            return Err(StoreError::not_found("workspace", id));
        }
        // Its git connections went with it (cascade); so do their tokens.
        sqlx::query("DELETE FROM secret WHERE scope_kind = 'workspace' AND scope_id = $1")
            .bind(id.to_string())
            .execute(self.conn())
            .await?;
        Ok(())
    }

    async fn link(&mut self, workspace: WorkspaceId, item: WorkspaceItem<'_>) -> Result<()> {
        // `table` and `column` are literals from `WorkspaceItem::link`.
        let (table, column, id) = item.link();
        sqlx::query(&format!(
            "INSERT INTO {table} (workspace_id, {column}) VALUES ($1, $2) ON CONFLICT DO NOTHING"
        ))
        .bind(workspace.to_string())
        .bind(id)
        .execute(self.conn())
        .await?;
        Ok(())
    }

    async fn unlink(&mut self, workspace: WorkspaceId, item: WorkspaceItem<'_>) -> Result<bool> {
        let (table, column, id) = item.link();
        let done = sqlx::query(&format!(
            "DELETE FROM {table} WHERE workspace_id = $1 AND {column} = $2"
        ))
        .bind(workspace.to_string())
        .bind(id)
        .execute(self.conn())
        .await?;
        Ok(done.rows_affected() > 0)
    }

    async fn workspaces_of(&mut self, item: WorkspaceItem<'_>) -> Result<Vec<WorkspaceId>> {
        let (table, column, id) = item.link();
        let ids: Vec<String> = sqlx::query_scalar(&format!(
            "SELECT workspace_id FROM {table} WHERE {column} = $1 ORDER BY workspace_id"
        ))
        .bind(id)
        .fetch_all(self.conn())
        .await?;
        ids.into_iter().map(parse).collect()
    }

    async fn workspaces_of_member(&mut self, member: MemberId) -> Result<Vec<WorkspaceId>> {
        let ids: Vec<String> = sqlx::query_scalar(
            "SELECT workspace_id FROM workspace_member WHERE member_id = $1 ORDER BY workspace_id",
        )
        .bind(member.to_string())
        .fetch_all(self.conn())
        .await?;
        ids.into_iter().map(parse).collect()
    }

    async fn catalog_installs(&mut self, workspace: WorkspaceId) -> Result<Vec<String>> {
        Ok(sqlx::query_scalar(
            "SELECT catalog_id FROM workspace_catalog_install WHERE workspace_id = $1 \
             ORDER BY catalog_id",
        )
        .bind(workspace.to_string())
        .fetch_all(self.conn())
        .await?)
    }
}

/// `InvalidInput` when a workspace other than `except` is named `name` (any case).
async fn name_free(tx: &mut Tx, except: Option<WorkspaceId>, name: &str) -> Result<()> {
    let taken: Option<String> =
        sqlx::query_scalar("SELECT id FROM workspace WHERE lower(name) = lower($1)")
            .bind(name)
            .fetch_optional(tx.conn())
            .await?;
    match taken {
        Some(id) if Some(id.as_str()) != except.map(|e| e.to_string()).as_deref() => Err(
            sun_core::Error::InvalidInput(format!("A workspace named `{name}` already exists."))
                .into(),
        ),
        _ => Ok(()),
    }
}

/// The workspace whose issue prefix is `prefix`, ignoring case.
async fn prefix_owner(tx: &mut Tx, prefix: &str) -> Result<Option<String>> {
    Ok(
        sqlx::query_scalar("SELECT id FROM workspace WHERE upper(issue_prefix) = upper($1)")
            .bind(prefix)
            .fetch_optional(tx.conn())
            .await?,
    )
}

/// `InvalidInput` when a workspace other than `except` has the issue prefix
/// `prefix` (any case): task keys name one task in the whole ecosystem.
async fn prefix_free(tx: &mut Tx, except: Option<WorkspaceId>, prefix: &str) -> Result<()> {
    match prefix_owner(tx, prefix).await? {
        Some(id) if Some(id.as_str()) != except.map(|e| e.to_string()).as_deref() => {
            Err(sun_core::Error::InvalidInput(format!(
                "Another workspace already uses the prefix {}.",
                prefix.to_ascii_uppercase()
            ))
            .into())
        }
        _ => Ok(()),
    }
}

async fn slug_taken(tx: &mut Tx, slug: &str) -> Result<bool> {
    let taken: Option<String> = sqlx::query_scalar("SELECT id FROM workspace WHERE slug = $1")
        .bind(slug)
        .fetch_optional(tx.conn())
        .await?;
    Ok(taken.is_some())
}

const WORKSPACE_COLUMNS: &str = "id, name, slug, description, context, issue_prefix, \
     issue_counter, default_workflow_id, git_settings_json, created_at, created_by";

/// What a read selects: the columns and the logo's hash.
const WORKSPACE_READ: &str = "id, name, slug, description, context, issue_prefix, issue_counter, \
     default_workflow_id, git_settings_json, created_at, created_by, \
     (SELECT hash FROM workspace_logo WHERE workspace_id = workspace.id) AS logo_hash";

fn workspace_from(row: &SqliteRow) -> Result<Workspace> {
    let default_workflow: Option<String> = row.try_get("default_workflow_id")?;
    Ok(Workspace {
        id: parse(row.try_get("id")?)?,
        name: row.try_get("name")?,
        slug: row.try_get("slug")?,
        description: row.try_get("description")?,
        context: row.try_get("context")?,
        logo_hash: row.try_get("logo_hash")?,
        issue_prefix: row.try_get("issue_prefix")?,
        issue_counter: row.try_get("issue_counter")?,
        default_workflow_id: default_workflow.map(parse).transpose()?,
        git_settings: json(row.try_get("git_settings_json")?)?,
        created_at: parse_ts(row.try_get("created_at")?)?,
        created_by: row
            .try_get::<Option<String>, _>("created_by")?
            .map(parse)
            .transpose()?,
    })
}

/// Upserts the workspace's logo row, its hash the sha256 of `bytes`.
async fn put_logo(
    conn: &mut SqliteConnection,
    workspace: WorkspaceId,
    content_type: &str,
    bytes: &[u8],
) -> Result<()> {
    let hash: String = Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    sqlx::query(
        "INSERT INTO workspace_logo (workspace_id, content_type, hash, bytes)
         VALUES ($1, $2, $3, $4)
         ON CONFLICT (workspace_id) DO UPDATE SET
           content_type = excluded.content_type, hash = excluded.hash, bytes = excluded.bytes",
    )
    .bind(workspace.to_string())
    .bind(content_type)
    .bind(hash)
    .bind(bytes)
    .execute(conn)
    .await?;
    Ok(())
}

/// Data step of the migration that drops `workspace.logo_blob`
/// ([`crate::DATA_STEPS`]): moves each logo from before `workspace_logo`
/// (a blob-store file the column names) into its row. One whose file is gone
/// or isn't an image is dropped. In the migration's transaction, at open,
/// before anything else uses the store.
pub(crate) async fn move_old_logos(conn: &mut SqliteConnection, blobs: &BlobStore) -> Result<()> {
    let old: Vec<(String, String)> = sqlx::query_as(
        "SELECT id, logo_blob FROM workspace WHERE logo_blob IS NOT NULL ORDER BY id",
    )
    .fetch_all(&mut *conn)
    .await?;
    for (id, hash) in old {
        let bytes = match sun_core::BlobHash::new(hash) {
            Ok(hash) => blobs.get(&hash).await.ok(),
            Err(_) => None,
        };
        let logo = bytes.and_then(|b| Workspace::logo_type(&b).map(|kind| (kind, b)));
        if let Some((kind, bytes)) = logo {
            put_logo(&mut *conn, parse(id)?, kind, &bytes).await?;
        }
    }
    Ok(())
}

/// Data step of `20261103090200_catalog_install_main` ([`crate::DATA_STEPS`]):
/// every catalog install from before (`<data>/catalog/installed.json`, id ->
/// version) is Main's. No file: nothing was installed.
pub(crate) async fn seed_main_catalog_installs(
    conn: &mut SqliteConnection,
    data_dir: &std::path::Path,
) -> Result<()> {
    let path = data_dir.join("catalog").join("installed.json");
    let raw = match std::fs::read(&path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    let installed: std::collections::BTreeMap<String, String> = serde_json::from_slice(&raw)
        .map_err(|e| sun_core::Error::InvalidInput(format!("{}: {e}", path.display())))?;
    for id in installed.keys() {
        sqlx::query(
            "INSERT INTO workspace_catalog_install (workspace_id, catalog_id) VALUES ($1, $2) \
             ON CONFLICT DO NOTHING",
        )
        .bind(WorkspaceId::MAIN.to_string())
        .bind(id)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}
