//! 2-step sign-in for the API, part of the read **Facade**: members'
//! second steps (an authenticator, an emailed code or a passkey), the sign-in
//! challenges between the password and the second step, an admin's reset,
//! and the ecosystem's [`SecurityPolicy`]. Each change commits with its
//! audit row. The API does the TOTP and WebAuthn math and hashes: challenges
//! and sessions arrive as `sha256(token)`; a challenge's data (a mailed
//! code's hash, a passkey ceremony) and a passkey's credential are the API's
//! JSON.

use serde_json::json;
use sun_core::model::{AuditAction, Factor, FactorKind, Member, SecurityPolicy};
use sun_core::{Error, FactorId, MemberId, Result, Timestamp};
use sun_store::{AuditRepo, FactorRepo, LoginRepo, MemberRepo, SettingRepo, TotpSecret, Tx};

use crate::Engine;

/// `kernel_setting` key of the [`SecurityPolicy`].
const SECURITY: &str = "security";

async fn audit(tx: &mut Tx, member: &Member, actor: &str, detail: &str) -> Result<()> {
    tx.append_audit(actor, AuditAction::MemberChange, &member.username, detail)
        .await?;
    Ok(())
}

/// The saved 2-step rules (all off until an admin saves them).
async fn policy_of(tx: &mut Tx) -> Result<SecurityPolicy> {
    match tx.setting(SECURITY).await? {
        Some(v) => serde_json::from_value(v).map_err(|e| Error::InvalidInput(e.to_string())),
        None => Ok(SecurityPolicy::default()),
    }
}

fn own(member: &Member) -> String {
    format!("user:{}", member.username)
}

impl Engine {
    /// The ecosystem's 2-step rules (all off until an admin saves them).
    pub async fn security_policy(&self) -> Result<SecurityPolicy> {
        policy_of(&mut self.store.read().await?).await
    }

    /// Changes the saved 2-step rules with `change` in one transaction (so
    /// a rule it leaves alone keeps its value), audited as `actor`; the new
    /// rules.
    pub async fn set_security_policy(
        &self,
        change: impl FnOnce(&mut SecurityPolicy) + Send,
        actor: &str,
    ) -> Result<SecurityPolicy> {
        let mut tx = self.store.begin().await?;
        let mut policy = policy_of(&mut tx).await?;
        change(&mut policy);
        tx.put_setting(SECURITY, &json!(policy)).await?;
        let detail = format!(
            "2-step required: {}, for password changes: {}",
            policy.require_two_step, policy.two_step_for_password_change
        );
        tx.append_audit(actor, AuditAction::SettingsChange, SECURITY, &detail)
            .await?;
        tx.commit().await?;
        Ok(policy)
    }

    /// The member's second steps (authenticators being added left out).
    pub async fn factors(&self, member: MemberId) -> Result<Vec<Factor>> {
        Ok(self.store.read().await?.factors(member).await?)
    }

    /// Starts adding an authenticator with `secret` (base32 text, sealed by
    /// the store); it counts once [`Self::confirm_totp`] saw a code of it.
    pub async fn start_totp(&self, member: MemberId, label: &str, secret: &str) -> Result<Factor> {
        let mut tx = self.store.begin().await?;
        let factor = tx
            .insert_factor(member, FactorKind::Totp, label, Some(secret), "")
            .await?;
        tx.commit().await?;
        Ok(factor)
    }

    /// Every authenticator of the member with its secret (to check a code).
    pub async fn totp_secrets(&self, member: MemberId) -> Result<Vec<TotpSecret>> {
        Ok(self.store.read().await?.totp_secrets(member).await?)
    }

    /// Uses time `step` of authenticator `id`; `false` when it (or a later
    /// one) was used already.
    pub async fn use_totp_step(&self, id: FactorId, step: i64) -> Result<bool> {
        let mut tx = self.store.begin().await?;
        let used = tx.use_totp_step(id, step).await?;
        tx.commit().await?;
        Ok(used)
    }

    /// Confirms the member's pending authenticator `id` with the time step
    /// of a code it showed, audited; `false` when that step was used.
    pub async fn confirm_totp(&self, member: MemberId, id: FactorId, step: i64) -> Result<bool> {
        let mut tx = self.store.begin().await?;
        let member = tx.get_member(member).await?;
        if !tx.use_totp_step(id, step).await? {
            return Ok(false);
        }
        audit(
            &mut tx,
            &member,
            &own(&member),
            "2-step: authenticator added",
        )
        .await?;
        tx.commit().await?;
        Ok(true)
    }

    /// Turns on emailed codes as the member's second step, audited. Already
    /// on is `InvalidInput`.
    pub async fn add_email_factor(&self, member: MemberId) -> Result<Factor> {
        let mut tx = self.store.begin().await?;
        let member = tx.get_member(member).await?;
        if tx
            .factors(member.id)
            .await?
            .iter()
            .any(|f| f.kind == FactorKind::Email)
        {
            return Err(Error::InvalidInput("Email codes are on already.".into()));
        }
        let factor = tx
            .insert_factor(member.id, FactorKind::Email, "Email", None, "")
            .await?;
        audit(&mut tx, &member, &own(&member), "2-step: email codes on").await?;
        tx.commit().await?;
        Ok(factor)
    }

    /// Adds passkey `credential` (the API's JSON) named `label`, ending the
    /// registration challenge `challenge_hash` in the same transaction,
    /// audited; `None` when that challenge was used already. `InvalidInput`
    /// when any member holds its credential id already (`id` in the JSON,
    /// unique in the store).
    pub async fn add_passkey(
        &self,
        member: MemberId,
        label: &str,
        credential: &str,
        challenge_hash: &str,
    ) -> Result<Option<Factor>> {
        let mut tx = self.store.begin().await?;
        let member = tx.get_member(member).await?;
        if !tx.end_two_step(challenge_hash).await? {
            return Ok(None);
        }
        let factor = tx
            .insert_factor(member.id, FactorKind::Passkey, label, None, credential)
            .await?;
        audit(&mut tx, &member, &own(&member), "2-step: passkey added").await?;
        tx.commit().await?;
        Ok(Some(factor))
    }

    /// The member's passkeys: each id and its credential.
    pub async fn passkeys(&self, member: MemberId) -> Result<Vec<(FactorId, String)>> {
        Ok(self.store.read().await?.passkeys(member).await?)
    }

    /// Saves passkey `id`'s credential after a sign-in (its new sign count).
    pub async fn use_passkey(&self, id: FactorId, credential: &str) -> Result<()> {
        let mut tx = self.store.begin().await?;
        tx.set_factor_data(id, credential).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Removes one of the member's own second steps, audited; `NotFound`
    /// when it isn't theirs.
    pub async fn delete_factor(&self, member: MemberId, id: FactorId) -> Result<()> {
        let mut tx = self.store.begin().await?;
        let member = tx.get_member(member).await?;
        if !tx.delete_factor(member.id, id).await? {
            return Err(Error::NotFound(format!("2-step method {id}")));
        }
        audit(&mut tx, &member, &own(&member), "2-step: method removed").await?;
        tx.commit().await?;
        Ok(())
    }

    /// An admin removes every second step of a member (a lost phone) and
    /// ends their sessions and codes, audited as `actor`.
    pub async fn reset_two_step(&self, member: MemberId, actor: &str) -> Result<Member> {
        let mut tx = self.store.begin().await?;
        let member = tx.get_member(member).await?;
        tx.delete_factors(member.id).await?;
        tx.delete_member_tokens(member.id).await?;
        audit(&mut tx, &member, actor, "2-step reset by an admin").await?;
        let member = tx.get_member(member.id).await?;
        tx.commit().await?;
        Ok(member)
    }

    /// Stores a challenge `challenge_hash` of `member` with `data` until
    /// `expires_at`: a sign-in one, or a passkey being added.
    pub async fn start_two_step(
        &self,
        challenge_hash: &str,
        member: MemberId,
        expires_at: Timestamp,
        data: &str,
    ) -> Result<()> {
        let mut tx = self.store.begin().await?;
        tx.insert_two_step(challenge_hash, member, expires_at)
            .await?;
        tx.set_two_step_data(challenge_hash, data).await?;
        tx.commit().await?;
        Ok(())
    }

    /// The live challenge's member and data ('' or the API's JSON).
    pub async fn two_step(&self, challenge_hash: &str) -> Result<Option<(MemberId, String)>> {
        Ok(self.store.read().await?.two_step(challenge_hash).await?)
    }

    /// Replaces the challenge's `data` (with a mailed code's hash or a
    /// passkey ceremony); `try_used`: it counts as a try (a mail).
    pub async fn set_two_step_data(
        &self,
        challenge_hash: &str,
        data: &str,
        try_used: bool,
    ) -> Result<()> {
        let mut tx = self.store.begin().await?;
        tx.set_two_step_data(challenge_hash, data).await?;
        if try_used {
            tx.fail_two_step(challenge_hash).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Counts a wrong second step; enough of them end the challenge.
    pub async fn fail_two_step(&self, challenge_hash: &str) -> Result<()> {
        let mut tx = self.store.begin().await?;
        tx.fail_two_step(challenge_hash).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Ends a challenge that was answered without signing in (a password
    /// change); `false` when it was used already.
    pub async fn end_two_step(&self, challenge_hash: &str) -> Result<bool> {
        let mut tx = self.store.begin().await?;
        let ended = tx.end_two_step(challenge_hash).await?;
        tx.commit().await?;
        Ok(ended)
    }

    /// Ends the answered challenge and starts session `session_hash` of
    /// `member` until `expires_at`, audited as a login by password and
    /// `method`, in one transaction; `false` (no session) when the challenge
    /// was used already.
    pub async fn finish_two_step(
        &self,
        challenge_hash: &str,
        member: &Member,
        method: FactorKind,
        session_hash: &str,
        expires_at: Timestamp,
    ) -> Result<bool> {
        let mut tx = self.store.begin().await?;
        if !tx.end_two_step(challenge_hash).await? {
            return Ok(false);
        }
        tx.insert_login_session(session_hash, member.id, expires_at)
            .await?;
        tx.append_audit(
            &own(member),
            AuditAction::Login,
            &member.username,
            &format!("password + {method}"),
        )
        .await?;
        tx.commit().await?;
        Ok(true)
    }
}
