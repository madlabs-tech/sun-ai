//! Members (their workspace roles are in `roles.rs`), runtime permission modes and configured runtimes
//! (`byo_key`), installed mods and the audit
//! log, and sign-in sessions, for the API,
//! part of the read **Facade** (`sun-api` may not reach `sun-store`). Each
//! change commits together with its audit row. Tokens arrive as
//! `sha256(token)` hex; the token itself is never stored.

use serde_json::json;
use sun_core::model::SecretScope;
use sun_core::model::{AuditAction, AuditEntry, Member, Runtime, RuntimeSource};
use sun_core::{AuditId, Error, MemberId, Result, RuntimeId, WorkspaceId};
use sun_store::{
    AuditRepo, LoginRepo, MemberRepo, ModInstallRepo, RuntimeRepo, SecretRepo, SettingRepo,
    WorkspaceRepo,
};

use crate::Engine;

/// `kernel_setting` key of `user`'s role view.
fn view_role_key(user: &str) -> String {
    format!("view_role.{user}")
}

impl Engine {
    /// The live session's member and whether an SSO sign-in started it;
    /// `None` when unknown or expired.
    pub async fn login_session(&self, session_hash: &str) -> Result<Option<(MemberId, bool)>> {
        Ok(self.store.read().await?.login_session(session_hash).await?)
    }

    /// Ends a session (sign-out); unknown is a no-op.
    pub async fn end_login_session(&self, session_hash: &str) -> Result<()> {
        let mut tx = self.store.begin().await?;
        tx.delete_login_session(session_hash).await?;
        tx.commit().await?;
        Ok(())
    }

    /// One member, or `NotFound`.
    pub async fn member(&self, id: MemberId) -> Result<Member> {
        Ok(self.store.read().await?.get_member(id).await?)
    }

    /// Every member, oldest first.
    pub async fn members(&self) -> Result<Vec<Member>> {
        Ok(self.store.read().await?.members().await?)
    }

    /// The role view `user` picked, if any: a template role such
    /// as `marketing`, which hides repo jargon. Stored per user, so it
    /// follows them across browsers.
    pub async fn view_role(&self, user: &str) -> Result<Option<String>> {
        let value = self
            .store
            .read()
            .await?
            .setting(&view_role_key(user))
            .await?;
        Ok(value.and_then(|v| v.as_str().map(str::to_owned)))
    }

    /// Stores (`None`: clears) `user`'s role view; it must be a role some
    /// template is for.
    pub async fn set_view_role(&self, user: &str, role: Option<&str>) -> Result<()> {
        if let Some(role) = role
            && !sun_workflow::TEMPLATES.iter().any(|t| t.role == role)
        {
            return Err(Error::InvalidInput(format!("`{role}` is not a role view")));
        }
        let mut tx = self.store.begin().await?;
        tx.put_setting(&view_role_key(user), &serde_json::json!(role))
            .await?;
        Ok(tx.commit().await?)
    }

    /// The member a chat user acts as: the member with the same `email`
    /// (case-insensitive, as the chat reported it). `None`: unknown or
    /// deactivated, so chat refuses both alike without saying which.
    pub async fn chat_member(&self, email: &str) -> Result<Option<Member>> {
        if !email.contains('@') {
            return Ok(None);
        }
        Ok(self
            .store
            .read()
            .await?
            .members()
            .await?
            .into_iter()
            .find(|m| {
                m.deactivated_at.is_none()
                    && m.email
                        .as_deref()
                        .is_some_and(|mine| mine.eq_ignore_ascii_case(email))
            }))
    }

    /// Appends one audit entry on its own, for actions that change no
    /// stored state here (a token sign-in, a secret read, a mod install).
    pub async fn audit(
        &self,
        actor: &str,
        action: AuditAction,
        target: &str,
        detail: &str,
    ) -> Result<AuditEntry> {
        let mut tx = self.store.begin().await?;
        let entry = tx.append_audit(actor, action, target, detail).await?;
        tx.commit().await?;
        Ok(entry)
    }

    /// Sets the permission mode every new session of runtime `adapter` gets
    /// (validated by the caller) and audits it as `actor`, in one transaction.
    pub async fn set_permission_mode(&self, adapter: &str, mode: &str, actor: &str) -> Result<()> {
        let mut tx = self.store.begin().await?;
        tx.set_permission_mode(adapter, mode).await?;
        tx.append_audit(actor, AuditAction::PolicyChange, adapter, mode)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    /// `(adapter, permission mode)` of every runtime with one set.
    pub async fn permission_modes(&self) -> Result<Vec<(String, String)>> {
        Ok(self.store.read().await?.permission_modes().await?)
    }

    /// The `runtime` rows of `workspace` (`None`: of every workspace), by name.
    pub async fn runtimes(&self, workspace: Option<WorkspaceId>) -> Result<Vec<Runtime>> {
        Ok(self.store.read().await?.runtimes(workspace).await?)
    }

    /// `workspace`'s runtime named `name`, or `NotFound`.
    pub async fn runtime(&self, workspace: WorkspaceId, name: &str) -> Result<Runtime> {
        self.store
            .read()
            .await?
            .runtime_by_name(workspace, name)
            .await?
            .ok_or_else(|| Error::NotFound(format!("runtime {name}")))
    }

    /// Stores a configured runtime (`byo_key`, or `credits` with the
    /// gateway `model` OpenCode needs) on its workspace after
    /// [`Runtime::validate`], audited as `actor`; `false` when the workspace
    /// has the name. `update` replaces the config of the workspace's runtime
    /// of that name instead (`false`: no such runtime). `NotFound` for a
    /// missing workspace.
    pub async fn save_runtime(
        &self,
        runtime: &Runtime,
        model: Option<&str>,
        update: bool,
        actor: &str,
    ) -> Result<bool> {
        runtime.validate()?;
        let model = credits_model(runtime, model)?;
        let mut tx = self.store.begin().await?;
        tx.get_workspace(runtime.workspace_id).await?;
        let id = match update {
            false => runtime.id,
            true => match tx
                .runtime_by_name(runtime.workspace_id, &runtime.name)
                .await?
            {
                Some(stored) => stored.id,
                None => return Ok(false),
            },
        };
        let saved = if update {
            tx.update_runtime(runtime).await?
        } else {
            tx.insert_runtime(runtime).await?
        };
        if saved && runtime.source == RuntimeSource::Credits {
            tx.put_setting(&credits_model_key(id), &json!(model))
                .await?;
        }
        if saved {
            let detail = match (&runtime.byo_key, model) {
                (Some(byo), _) => format!("{} {} {}", runtime.source, byo.endpoint, byo.model),
                (None, Some(model)) => format!("{} {model}", runtime.source),
                (None, None) => runtime.source.to_string(),
            };
            tx.append_audit(actor, AuditAction::PolicyChange, &runtime.name, &detail)
                .await?;
            tx.commit().await?;
        }
        Ok(saved)
    }

    /// The gateway model of the `credits` runtime `id`, if it has one.
    pub async fn credits_model(&self, id: RuntimeId) -> Result<Option<String>> {
        let value = self
            .store
            .read()
            .await?
            .setting(&credits_model_key(id))
            .await?;
        Ok(value.and_then(|v| v.as_str().map(str::to_owned)))
    }

    /// Why `runtime` can't start a session as configured, or `None`: its
    /// config fails [`Runtime::validate`], or its key secret is not stored
    /// in its workspace (checked by name; the value is never read).
    pub async fn runtime_problem(&self, runtime: &Runtime) -> Result<Option<String>> {
        if let Err(Error::InvalidInput(why)) = runtime.validate() {
            return Ok(Some(why));
        }
        let Some(name) = runtime.byo_key.as_ref().and_then(|b| b.key_secret.as_ref()) else {
            return Ok(None);
        };
        let scope = SecretScope::Workspace(runtime.workspace_id);
        let stored = self.store.read().await?.list_secrets(Some(&scope)).await?;
        Ok((!stored.iter().any(|s| &s.name == name)).then(|| {
            format!(
                "key_secret: `{name}` is not set in this workspace; set it with \
                 `sun secret set {name} --workspace {}`",
                runtime.workspace_id
            )
        }))
    }

    /// Every installed mod (`mod_install` rows), by name.
    pub async fn mod_installs(&self) -> Result<Vec<sun_core::model::ModInstall>> {
        Ok(self.store.read().await?.mod_installs().await?)
    }

    /// Up to `limit` audit entries older than `before`, newest first.
    pub async fn audit_log(&self, before: Option<AuditId>, limit: u32) -> Result<Vec<AuditEntry>> {
        Ok(self.store.read().await?.audit_log(before, limit).await?)
    }
}

/// `kernel_setting` key of a `credits` runtime's gateway model.
pub fn credits_model_key(id: RuntimeId) -> String {
    format!("credits_model:{id}")
}

/// `model` checked for `runtime`: OpenCode on Sun credits needs one from the
/// gateway's price table; Claude Code and Codex pick their own, and other
/// sources take none.
fn credits_model<'a>(runtime: &Runtime, model: Option<&'a str>) -> Result<Option<&'a str>> {
    let model = model.map(str::trim).filter(|m| !m.is_empty());
    let invalid = |msg: &str| Err(Error::InvalidInput(msg.into()));
    match (runtime.source, runtime.adapter.as_str(), model) {
        (RuntimeSource::Credits, "opencode", None) => {
            invalid("model: OpenCode on Sun credits needs a model from the price table")
        }
        (RuntimeSource::Credits, "opencode", Some(m))
            if m.len() > 200 || m.chars().any(char::is_whitespace) =>
        {
            invalid("model: up to 200 characters, no spaces")
        }
        (RuntimeSource::Credits, "opencode", model) => Ok(model),
        (_, _, None) => Ok(None),
        (RuntimeSource::Credits, _, Some(_)) => {
            invalid("model: only OpenCode takes a model; Claude Code and Codex pick their own")
        }
        (_, _, Some(_)) => invalid("model: only a credits runtime takes a model here"),
    }
}
