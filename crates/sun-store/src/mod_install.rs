//! `mod_install`: the mods installed here and what the user
//! granted them. The package files live in the mods dir;
//! this row is what the kernel loads at boot.

use async_trait::async_trait;
use sqlx::Row;
use sqlx::sqlite::SqliteRow;
use sun_core::model::ModInstall;

use crate::repo::{json, parse};
use crate::{Result, Tx};

/// **Repository** for `mod_install` rows.
#[async_trait]
pub trait ModInstallRepo {
    /// Records an install, replacing an older install of the same name.
    async fn put_mod_install(&mut self, install: &ModInstall) -> Result<()>;
    /// Every installed mod, by name.
    async fn mod_installs(&mut self) -> Result<Vec<ModInstall>>;
    /// Forgets an install; `false` when there was none.
    async fn delete_mod_install(&mut self, name: &str) -> Result<bool>;
}

#[async_trait]
impl ModInstallRepo for Tx {
    async fn put_mod_install(&mut self, install: &ModInstall) -> Result<()> {
        sqlx::query(
            "INSERT INTO mod_install (name, kind, version, manifest_json, \
             granted_permissions_json, package_digest, signed_by) VALUES ($1, $2, $3, $4, $5, $6, $7) \
             ON CONFLICT (name) DO UPDATE SET kind = excluded.kind, version = excluded.version, \
             manifest_json = excluded.manifest_json, \
             granted_permissions_json = excluded.granted_permissions_json, \
             package_digest = excluded.package_digest, signed_by = excluded.signed_by",
        )
        .bind(&install.name)
        .bind(install.kind.as_str())
        .bind(&install.version)
        .bind(serde_json::to_string(&install.manifest)?)
        .bind(serde_json::to_string(&install.granted_permissions)?)
        .bind(&install.package_digest)
        .bind(&install.signed_by)
        .execute(self.conn())
        .await?;
        Ok(())
    }

    async fn mod_installs(&mut self) -> Result<Vec<ModInstall>> {
        sqlx::query(
            "SELECT name, kind, version, manifest_json, granted_permissions_json, \
             package_digest, signed_by FROM mod_install ORDER BY name",
        )
        .fetch_all(self.conn())
        .await?
        .iter()
        .map(install_from)
        .collect()
    }

    async fn delete_mod_install(&mut self, name: &str) -> Result<bool> {
        let done = sqlx::query("DELETE FROM mod_install WHERE name = $1")
            .bind(name)
            .execute(self.conn())
            .await?;
        Ok(done.rows_affected() > 0)
    }
}

fn install_from(row: &SqliteRow) -> Result<ModInstall> {
    Ok(ModInstall {
        name: row.try_get("name")?,
        kind: parse(row.try_get("kind")?)?,
        version: row.try_get("version")?,
        manifest: json(row.try_get("manifest_json")?)?,
        granted_permissions: json(row.try_get("granted_permissions_json")?)?,
        package_digest: row.try_get("package_digest")?,
        signed_by: row.try_get("signed_by")?,
    })
}
