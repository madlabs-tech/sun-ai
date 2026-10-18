//! A workspace's credentials (TRD BLO-117 item 7): who pays for a session,
//! its own API key (`byo_key`) or Sun credits, for the API, part of the read
//! **Facade** (`sun-api` may not reach `sun-store`). Each change commits
//! together with its audit row. A key's value stays in `secret`; here only
//! the secret's name is checked.

use serde_json::json;
use sun_core::entitlement::{Entitlement, Quota};
use sun_core::model::{AuditAction, Credential, RuntimeSource, SecretScope};
use sun_core::{CredentialId, Error, Result, WorkspaceId};
use sun_store::{AgentRepo, AuditRepo, CredentialRepo, SecretRepo, SettingRepo, WorkspaceRepo};

use crate::Engine;

/// What [`Engine::delete_credential`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredentialRemoval {
    /// Deleted.
    Removed,
    /// The workspace has no credential of that name.
    Missing,
    /// Kept: these active agents' chains name it.
    NamedBy(Vec<String>),
}

impl Engine {
    /// The credentials of `workspace` (`None`: of every workspace), by name.
    pub async fn credentials(&self, workspace: Option<WorkspaceId>) -> Result<Vec<Credential>> {
        Ok(self.store.read().await?.credentials(workspace).await?)
    }

    /// `workspace`'s credential named `name`, or `NotFound`.
    pub async fn credential(&self, workspace: WorkspaceId, name: &str) -> Result<Credential> {
        self.store
            .read()
            .await?
            .credential_by_name(workspace, name)
            .await?
            .ok_or_else(|| Error::NotFound(format!("credential {name}")))
    }

    /// Stores a credential (`byo_key`, or `credits` with the gateway `model`
    /// OpenCode needs) on its workspace after [`Credential::validate`],
    /// audited as `actor`; `false` when the workspace has the name. `update`
    /// replaces the settings of the workspace's credential of that name
    /// instead (`false`: no such credential). `NotFound` for a missing
    /// workspace; `PlanLimit` when `plan` has no room for one more
    /// credential (a new one) or BYO key (a new one, or one turned into it).
    pub async fn save_credential(
        &self,
        credential: &Credential,
        model: Option<&str>,
        update: bool,
        actor: &str,
        plan: &Entitlement,
    ) -> Result<bool> {
        credential.validate()?;
        let model = credits_model(credential, model)?;
        let mut tx = self.store.begin().await?;
        tx.get_workspace(credential.workspace_id).await?;
        let stored = tx.credentials(Some(credential.workspace_id)).await?;
        let before = stored.iter().find(|c| c.name == credential.name);
        let id = match (update, before) {
            (false, _) => credential.id,
            (true, Some(before)) => before.id,
            (true, None) => return Ok(false),
        };
        if !update {
            plan.check(Quota::Runtimes, stored.len())?;
        }
        let new_key = credential.source == RuntimeSource::ByoKey
            && before.is_none_or(|b| b.source != RuntimeSource::ByoKey);
        if new_key {
            let keys = stored.iter().filter(|c| c.source == RuntimeSource::ByoKey);
            plan.check(Quota::ByoKeys, keys.count())?;
        }
        let saved = if update {
            tx.update_credential(credential).await?
        } else {
            tx.insert_credential(credential).await?
        };
        if saved && credential.source == RuntimeSource::Credits {
            tx.put_setting(&credits_model_key(id), &json!(model))
                .await?;
        }
        if saved {
            let source = credential.source;
            let detail = match (&credential.byo_key, model) {
                (Some(byo), _) => format!("{source} {} {}", byo.endpoint, byo.model),
                (None, Some(model)) => format!("{source} {model}"),
                (None, None) => source.to_string(),
            };
            tx.append_audit(actor, AuditAction::PolicyChange, &credential.name, &detail)
                .await?;
            tx.commit().await?;
        }
        Ok(saved)
    }

    /// Deletes `workspace`'s credential `name`, audited as `actor`, unless
    /// an active agent's chain names it (a bare name or `credential:`):
    /// that chain would stop resolving. Workflow chains are not checked;
    /// one naming it fails visibly at placement.
    pub async fn delete_credential(
        &self,
        workspace: WorkspaceId,
        name: &str,
        actor: &str,
    ) -> Result<CredentialRemoval> {
        let mut tx = self.store.begin().await?;
        let naming: Vec<String> = tx
            .agents(Some(workspace))
            .await?
            .into_iter()
            .filter(|a| a.runtime_chain.iter().any(|e| names(e, name)))
            .map(|a| a.name)
            .collect();
        if !naming.is_empty() {
            return Ok(CredentialRemoval::NamedBy(naming));
        }
        if tx.delete_credential(workspace, name).await?.is_none() {
            return Ok(CredentialRemoval::Missing);
        }
        tx.append_audit(actor, AuditAction::PolicyChange, name, "credential deleted")
            .await?;
        tx.commit().await?;
        Ok(CredentialRemoval::Removed)
    }

    /// The gateway model of the `credits` credential `id`, if it has one.
    pub async fn credits_model(&self, id: CredentialId) -> Result<Option<String>> {
        let value = self
            .store
            .read()
            .await?
            .setting(&credits_model_key(id))
            .await?;
        Ok(value.and_then(|v| v.as_str().map(str::to_owned)))
    }

    /// Why `credential` can't pay for a session as configured, or `None`:
    /// it fails [`Credential::validate`], or its key secret is not stored in
    /// its workspace (checked by name; the value is never read).
    pub async fn credential_problem(&self, credential: &Credential) -> Result<Option<String>> {
        if let Err(Error::InvalidInput(why)) = credential.validate() {
            return Ok(Some(why));
        }
        let byo = credential.byo_key.as_ref();
        let Some(name) = byo.and_then(|b| b.key_secret.as_ref()) else {
            return Ok(None);
        };
        let scope = SecretScope::Workspace(credential.workspace_id);
        let stored = self.store.read().await?.list_secrets(Some(&scope)).await?;
        Ok((!stored.iter().any(|s| &s.name == name)).then(|| {
            format!(
                "key_secret: `{name}` is not set in this workspace; set it with \
                 `sun secret set {name} --workspace {}`",
                credential.workspace_id
            )
        }))
    }
}

/// Chain entry `entry` names credential `name`.
fn names(entry: &sun_core::ChainEntry, name: &str) -> bool {
    match entry {
        sun_core::ChainEntry::Name(n) => n == name,
        sun_core::ChainEntry::Spec(spec) => spec.credential.as_deref() == Some(name),
    }
}

/// `kernel_setting` key of a `credits` credential's gateway model.
pub fn credits_model_key(id: CredentialId) -> String {
    format!("credits_model:{id}")
}

/// `model` checked for `credential`: OpenCode on Sun credits needs one from
/// the gateway's price table; Claude Code and Codex pick their own, and a
/// key takes its model in its own settings.
fn credits_model<'a>(credential: &Credential, model: Option<&'a str>) -> Result<Option<&'a str>> {
    let model = model.map(str::trim).filter(|m| !m.is_empty());
    let invalid = |msg: &str| Err(Error::InvalidInput(msg.into()));
    match (credential.source, credential.default_tool.as_str(), model) {
        (RuntimeSource::Credits, "opencode", None) => {
            invalid("default_model: OpenCode on Sun credits needs a model from the price table")
        }
        (RuntimeSource::Credits, "opencode", Some(m))
            if m.len() > 200 || m.chars().any(char::is_whitespace) =>
        {
            invalid("default_model: up to 200 characters, no spaces")
        }
        (RuntimeSource::Credits, "opencode", model) => Ok(model),
        (_, _, None) => Ok(None),
        (RuntimeSource::Credits, _, Some(_)) => invalid(
            "default_model: only OpenCode takes a model; Claude Code and Codex pick their own",
        ),
        (_, _, Some(_)) => invalid("default_model: only a credits credential takes a model here"),
    }
}
