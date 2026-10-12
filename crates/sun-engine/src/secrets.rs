//! Sealed secrets for the API and the `sun secret` command: list
//! (metadata only), set and delete, each audited in the same transaction.
//! No path here returns a value. The free functions serve `sun-cli`, which
//! writes the store directly without an `Engine`.

use sun_core::Result;
use sun_core::model::{AuditAction, Secret, SecretScope};
use sun_store::{AuditRepo, SecretRepo, Store};

use crate::Engine;

fn target(scope: &SecretScope, name: &str) -> String {
    format!("{}:{}/{name}", scope.kind(), scope.id())
}

/// Seals `value` as `name` in `scope` (replacing it), audits the change as
/// `actor` and redacts the value from already-indexed knowledge.
pub async fn set_secret(
    store: &Store,
    scope: &SecretScope,
    name: &str,
    value: &str,
    actor: &str,
) -> Result<Secret> {
    let mut tx = store.begin().await?;
    let secret = tx.set_secret(scope, name, value).await?;
    tx.append_audit(
        actor,
        AuditAction::SecretChange,
        &target(scope, name),
        "set",
    )
    .await?;
    tx.commit().await?;
    sun_context::knowledge::redact_indexed(store, value).await?;
    Ok(secret)
}

/// Deletes `name` from `scope` and audits it; false when there was none.
pub async fn delete_secret(
    store: &Store,
    scope: &SecretScope,
    name: &str,
    actor: &str,
) -> Result<bool> {
    let mut tx = store.begin().await?;
    if !tx.delete_secret(scope, name).await? {
        return Ok(false);
    }
    tx.append_audit(
        actor,
        AuditAction::SecretChange,
        &target(scope, name),
        "deleted",
    )
    .await?;
    tx.commit().await?;
    Ok(true)
}

/// Records that the values of `names` in `scope` left the store for
/// `purpose` (e.g. a mod's env), as `actor`.
pub async fn audit_secret_read(
    store: &Store,
    scope: &SecretScope,
    names: &[String],
    actor: &str,
    purpose: &str,
) -> Result<()> {
    if names.is_empty() {
        return Ok(());
    }
    let mut tx = store.begin().await?;
    tx.append_audit(
        actor,
        AuditAction::SecretRead,
        &target(scope, &names.join(",")),
        purpose,
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

impl Engine {
    /// Secrets of `scope` (all when `None`): names and times, never values.
    pub async fn secrets(&self, scope: Option<&SecretScope>) -> Result<Vec<Secret>> {
        Ok(self.store.read().await?.list_secrets(scope).await?)
    }

    /// See [`set_secret`].
    pub async fn set_secret(
        &self,
        scope: &SecretScope,
        name: &str,
        value: &str,
        actor: &str,
    ) -> Result<Secret> {
        set_secret(&self.store, scope, name, value, actor).await
    }

    /// See [`delete_secret`].
    pub async fn delete_secret(
        &self,
        scope: &SecretScope,
        name: &str,
        actor: &str,
    ) -> Result<bool> {
        delete_secret(&self.store, scope, name, actor).await
    }
}
