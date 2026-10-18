//! Members (their workspace roles are in `roles.rs`), installed mods and
//! the audit log, and sign-in sessions, for the API,
//! part of the read **Facade** (`sun-api` may not reach `sun-store`). Each
//! change commits together with its audit row. Tokens arrive as
//! `sha256(token)` hex; the token itself is never stored.

use sun_core::model::{AuditAction, AuditEntry, Member};
use sun_core::{AuditId, Error, MemberId, Result, Timestamp};
use sun_store::{AuditRepo, LoginRepo, MemberRepo, ModInstallRepo, RuntimeRepo, SettingRepo};

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

    /// `account -> cooldown_until` of every account still cooling down now,
    /// by [`sun_core::model::Account::key`] (internal: never in a response).
    pub async fn cooldowns(&self) -> Result<std::collections::BTreeMap<String, Timestamp>> {
        let now = chrono::Utc::now();
        let rows = self.store.read().await?.cooldowns().await?;
        Ok(rows.into_iter().filter(|(_, until)| *until > now).collect())
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
