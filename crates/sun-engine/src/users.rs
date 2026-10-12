//! Members for the API, part of the read **Facade**: sign-up with an
//! emailed code (as the deployment's [`SignupPolicy`] allows, or with a
//! pending invitation), email verification (the first verified person
//! becomes the deployment owner and owner of Main), invitations into a
//! workspace with a role (accept, decline, revoke), members an admin adds
//! with a one-time code or a password, re-invites, code and password
//! sign-in, passwords and a member's own email, the first-run owner's
//! member row, OIDC linking to an invited member, deactivation, and the
//! SMTP settings. Each change
//! commits together with its audit row. The API hashes: codes arrive as
//! `sha256(email ':' code)`, passwords as Argon2id PHC strings, sessions as
//! `sha256(token)`; nothing here sees a plaintext credential except the SMTP
//! password, which goes in and out of the sealed secret store.

use serde_json::json;
use sun_core::model::{
    AuditAction, FIRST_ADMIN_EMAIL, LOCAL_ISSUER, Member, OWNER_ISSUER, Role, SignupPolicy,
    SmtpSettings, WorkspaceInvitation, WorkspaceMember, normalize_email,
};
use sun_core::{Error, InvitationId, MemberId, Result, Timestamp, WorkspaceId};
use sun_store::{
    AuditRepo, InvitationRepo, LoginRepo, MemberRepo, MembershipRepo, SecretRepo, SettingRepo, Tx,
    WorkspaceRepo,
};

use crate::Engine;
use crate::knowledge::ecosystem_scope;

/// Ecosystem secret holding the SMTP password.
pub const SMTP_PASSWORD: &str = "SMTP_PASSWORD";
/// `kernel_setting` key of the [`SmtpSettings`].
const SMTP: &str = "smtp";

async fn audit(tx: &mut Tx, actor: &str, target: &str, detail: &str) -> Result<()> {
    tx.append_audit(actor, AuditAction::MemberChange, target, detail)
        .await?;
    Ok(())
}

async fn login(tx: &mut Tx, member: &Member, how: &str) -> Result<()> {
    let actor = format!("user:{}", member.username);
    tx.append_audit(&actor, AuditAction::Login, &member.username, how)
        .await?;
    Ok(())
}

/// Records that `id` proved their email. The first time, when no
/// deployment owner has a real email yet (a fresh deployment: only the
/// token holder, still on the placeholder), they become the deployment
/// owner and owner of Main; `true` then. The token holder's own row is
/// never promoted this way.
async fn verified(tx: &mut Tx, id: MemberId) -> Result<bool> {
    let me = tx.get_member(id).await?;
    if me.email_verified_at.is_some() {
        return Ok(false);
    }
    tx.verify_email(id).await?;
    let human_owner = tx.members().await?.iter().any(|m| {
        m.is_owner && m.id != id && m.email.as_deref().is_some_and(|e| e != FIRST_ADMIN_EMAIL)
    });
    if human_owner || me.is_owner || me.issuer == OWNER_ISSUER {
        return Ok(false);
    }
    tx.set_member_owner(id, true).await?;
    match tx.role_of(WorkspaceId::MAIN, id).await? {
        Some(_) => tx.set_role(WorkspaceId::MAIN, id, Role::Owner).await?,
        None => {
            tx.add_member(WorkspaceId::MAIN, id, Role::Owner).await?;
        }
    }
    tx.append_audit(
        &format!("user:{}", me.username),
        AuditAction::RoleChange,
        &me.username,
        "first verified member: deployment owner and owner of Main",
    )
    .await?;
    Ok(true)
}

/// `Forbidden` unless `member` is the verified owner of `invitation`'s email.
fn invited(member: &Member, invitation: &WorkspaceInvitation) -> Result<()> {
    if member
        .email
        .as_deref()
        .is_none_or(|e| !e.eq_ignore_ascii_case(&invitation.email))
    {
        return Err(Error::Forbidden(
            "This invitation is for another email address.".into(),
        ));
    }
    if member.email_verified_at.is_none() {
        return Err(Error::Forbidden("Verify your email first.".into()));
    }
    Ok(())
}

fn not_pending() -> Error {
    Error::InvalidInput("This invitation was already used, declined or has expired.".into())
}

/// How a new member signs in the first time.
pub enum FirstSignIn<'a> {
    /// The emailed invite code (`sha256(email ':' code)`) until `expires_at`.
    Code {
        /// The code's hash.
        hash: &'a str,
        /// When it stops working.
        expires_at: Timestamp,
    },
    /// A password an admin set (Argon2id `hash`); `must_change` makes them
    /// replace it at their first sign-in.
    Password {
        /// The password's hash.
        hash: &'a str,
        /// Force a change at the next sign-in.
        must_change: bool,
    },
}

impl Engine {
    /// Adds `email` (normalized) as `username` (a deployment owner when
    /// `is_owner`) to `workspace` as `role`, signing in first with `first`
    /// (an invite code, or a password an admin set, which also counts as a
    /// verified email), audited as `actor`, in one transaction.
    #[allow(clippy::too_many_arguments)]
    pub async fn invite_member(
        &self,
        email: &str,
        username: &str,
        is_owner: bool,
        workspace: WorkspaceId,
        role: Role,
        first: FirstSignIn<'_>,
        actor: &str,
    ) -> Result<Member> {
        let mut tx = self.store.begin().await?;
        let name = tx.get_workspace(workspace).await?.name;
        let member = tx.invite_member(email, username, is_owner).await?;
        tx.add_member(workspace, member.id, role).await?;
        let how = match first {
            FirstSignIn::Code { hash, expires_at } => {
                tx.insert_otp(member.id, hash, expires_at).await?;
                "invited"
            }
            FirstSignIn::Password { hash, must_change } => {
                tx.set_password(member.id, Some(hash), must_change).await?;
                tx.verify_email(member.id).await?;
                "created with a password"
            }
        };
        let owner = if is_owner { " as deployment owner" } else { "" };
        audit(
            &mut tx,
            actor,
            username,
            &format!("{how} {email}{owner} into {name} as {role}"),
        )
        .await?;
        let member = tx.get_member(member.id).await?;
        tx.commit().await?;
        Ok(member)
    }

    /// Signs up `email` (normalized here): a new member (no password, email
    /// not verified, in no workspace) and their emailed code `code_hash`
    /// (`sha256(email ':' code)`) until `expires_at`, which
    /// [`Engine::sign_in_with_code`] takes to verify them. Admitted when
    /// `policy` allows the email or a pending invitation names it;
    /// otherwise `Forbidden`. Someone who signed up before and never
    /// verified gets a new code. `None` (nothing written) when the email
    /// already belongs to an account that signed in, so the caller can
    /// answer alike either way.
    pub async fn signup(
        &self,
        email: &str,
        policy: &SignupPolicy,
        code_hash: &str,
        expires_at: Timestamp,
    ) -> Result<Option<Member>> {
        let email = normalize_email(email)?;
        let mut tx = self.store.begin().await?;
        let invited = !tx.invitations_by_email(&email).await?.is_empty();
        if !invited && !policy.admits(&email) {
            return Err(Error::Forbidden(match policy.allow_signup {
                false => "Sign-up is closed here. Ask a workspace admin for an invitation.".into(),
                true => "Sign-up here is limited to some email domains. Ask a workspace admin \
                         for an invitation."
                    .into(),
            }));
        }
        let (member, how) = match tx.member_by_email(&email).await? {
            Some(m)
                if m.issuer == LOCAL_ISSUER
                    && !m.has_password
                    && m.email_verified_at.is_none()
                    && m.deactivated_at.is_none() =>
            {
                (m, "asked for a new sign-up code")
            }
            Some(_) => return Ok(None),
            None => (tx.create_unverified(&email).await?, "signed up"),
        };
        tx.insert_otp(member.id, code_hash, expires_at).await?;
        let actor = format!("user:{}", member.username);
        audit(&mut tx, &actor, &member.username, how).await?;
        tx.commit().await?;
        Ok(Some(member))
    }

    /// Records that member `id` proved their email (see
    /// [`Engine::sign_in_with_code`]), audited; the first verified person
    /// on a fresh deployment becomes its owner and owner of Main.
    pub async fn verify_email(&self, id: MemberId) -> Result<Member> {
        let mut tx = self.store.begin().await?;
        let member = tx.get_member(id).await?;
        if member.email_verified_at.is_none() {
            verified(&mut tx, id).await?;
            let actor = format!("user:{}", member.username);
            audit(&mut tx, &actor, &member.username, "email verified").await?;
        }
        let member = tx.get_member(id).await?;
        tx.commit().await?;
        Ok(member)
    }

    /// Invites `email` (normalized here) into `workspace` as `role`: the
    /// link token is stored as `token_hash` until `expires_at`, replacing a
    /// pending invitation to the same email; audited as `actor`. Who may
    /// grant `role` is the caller's check. `InvalidInput` when they already
    /// are a member, `NotFound` for an unknown workspace.
    #[allow(clippy::too_many_arguments)]
    pub async fn invite(
        &self,
        workspace: WorkspaceId,
        email: &str,
        role: Role,
        inviter: Option<MemberId>,
        token_hash: &str,
        expires_at: Timestamp,
        actor: &str,
    ) -> Result<WorkspaceInvitation> {
        let email = normalize_email(email)?;
        let mut tx = self.store.begin().await?;
        let name = tx.get_workspace(workspace).await?.name;
        if let Some(member) = tx.member_by_email(&email).await?
            && tx.role_of(workspace, member.id).await?.is_some()
        {
            return Err(Error::InvalidInput(format!(
                "{email} is already a member of {name}."
            )));
        }
        let invitation = WorkspaceInvitation {
            id: InvitationId::generate(),
            workspace_id: workspace,
            email,
            role,
            inviter_id: inviter,
            created_at: chrono::Utc::now(),
            expires_at,
            accepted_at: None,
            declined_at: None,
        };
        tx.insert_invitation(&invitation, token_hash).await?;
        let detail = format!("invited to {name} as {role}");
        audit(&mut tx, actor, &invitation.email, &detail).await?;
        tx.commit().await?;
        Ok(invitation)
    }

    /// The invitation whose link token hashes to `token_hash`, in any state.
    pub async fn invitation_by_token(
        &self,
        token_hash: &str,
    ) -> Result<Option<WorkspaceInvitation>> {
        Ok(self
            .store
            .read()
            .await?
            .invitation_by_token_hash(token_hash)
            .await?)
    }

    /// `workspace`'s pending invitations, newest first.
    pub async fn invitations(&self, workspace: WorkspaceId) -> Result<Vec<WorkspaceInvitation>> {
        Ok(self.store.read().await?.invitations(workspace).await?)
    }

    /// The pending invitations to member `id`'s email, newest first.
    pub async fn my_invitations(&self, id: MemberId) -> Result<Vec<WorkspaceInvitation>> {
        let mut tx = self.store.read().await?;
        match tx.get_member(id).await?.email {
            Some(email) => Ok(tx.invitations_by_email(&email).await?),
            None => Ok(Vec::new()),
        }
    }

    /// Member `id` accepts invitation `invitation`, in one transaction: it
    /// is used up and they join its workspace with its role (or keep a
    /// higher role they already have there), audited. `Forbidden` unless
    /// it names their verified email; `InvalidInput` when it isn't pending
    /// (used, declined, revoked or expired).
    pub async fn accept_invitation(
        &self,
        invitation: InvitationId,
        id: MemberId,
    ) -> Result<WorkspaceMember> {
        let mut tx = self.store.begin().await?;
        let found = tx.get_invitation(invitation).await?;
        let member = tx.get_member(id).await?;
        invited(&member, &found)?;
        if !tx.accept_invitation(invitation).await? {
            return Err(not_pending());
        }
        let workspace = found.workspace_id;
        match tx.role_of(workspace, id).await? {
            None => {
                tx.add_member(workspace, id, found.role).await?;
            }
            Some(role) if !role.covers(found.role) => {
                tx.set_role(workspace, id, found.role).await?;
            }
            Some(_) => {}
        }
        let name = tx.get_workspace(workspace).await?.name;
        let actor = format!("user:{}", member.username);
        let detail = format!("accepted the invitation to {name} as {}", found.role);
        audit(&mut tx, &actor, &member.username, &detail).await?;
        let joined = tx
            .members_of(workspace)
            .await?
            .into_iter()
            .find(|m| m.member_id == id);
        tx.commit().await?;
        joined.ok_or_else(|| Error::NotFound(format!("workspace member {id}")))
    }

    /// Member `id` declines invitation `invitation`, audited; as
    /// [`Engine::accept_invitation`] for who may and when.
    pub async fn decline_invitation(&self, invitation: InvitationId, id: MemberId) -> Result<()> {
        let mut tx = self.store.begin().await?;
        let found = tx.get_invitation(invitation).await?;
        let member = tx.get_member(id).await?;
        invited(&member, &found)?;
        if !tx.decline_invitation(invitation).await? {
            return Err(not_pending());
        }
        let name = tx.get_workspace(found.workspace_id).await?.name;
        let actor = format!("user:{}", member.username);
        let detail = format!("declined the invitation to {name}");
        audit(&mut tx, &actor, &member.username, &detail).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Revokes a pending invitation (its link stops working), audited as
    /// `actor`; `InvalidInput` when it isn't pending.
    pub async fn revoke_invitation(&self, invitation: InvitationId, actor: &str) -> Result<()> {
        let mut tx = self.store.begin().await?;
        let found = tx.get_invitation(invitation).await?;
        if !tx.expire_invitation(invitation).await? {
            return Err(not_pending());
        }
        let name = tx.get_workspace(found.workspace_id).await?.name;
        let detail = format!("invitation to {name} revoked");
        audit(&mut tx, actor, &found.email, &detail).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Re-invites a member (a lost invite or a forgotten password): clears
    /// their password, ends their sessions and stores the new code, audited.
    /// A member without an email is `InvalidInput`.
    pub async fn reinvite_member(
        &self,
        id: MemberId,
        code_hash: &str,
        expires_at: Timestamp,
        actor: &str,
    ) -> Result<Member> {
        let mut tx = self.store.begin().await?;
        let member = tx.get_member(id).await?;
        let Some(email) = member.email.clone() else {
            return Err(Error::InvalidInput(format!(
                "{} has no email yet: they add it on their next sign-in.",
                member.username
            )));
        };
        tx.set_password(id, None, false).await?;
        tx.delete_member_tokens(id).await?;
        tx.insert_otp(id, code_hash, expires_at).await?;
        audit(
            &mut tx,
            actor,
            &member.username,
            &format!("re-invited {email}"),
        )
        .await?;
        let member = tx.get_member(id).await?;
        tx.commit().await?;
        Ok(member)
    }

    /// Reactivates a member, or deactivates them and ends their sessions,
    /// audited. The last active owner can't be deactivated (`InvalidInput`);
    /// an unknown member is `NotFound`.
    pub async fn set_member_active(
        &self,
        id: MemberId,
        active: bool,
        actor: &str,
    ) -> Result<Member> {
        let mut tx = self.store.begin().await?;
        let member = tx.get_member(id).await?;
        if !active && member.is_owner && member.deactivated_at.is_none() {
            let owners = tx
                .members()
                .await?
                .iter()
                .filter(|m| m.is_owner && m.deactivated_at.is_none())
                .count();
            if owners <= 1 {
                return Err(Error::InvalidInput(
                    "The last active owner can't be deactivated.".into(),
                ));
            }
        }
        tx.set_member_active(id, active).await?;
        let what = if active { "reactivated" } else { "deactivated" };
        audit(&mut tx, actor, &member.username, what).await?;
        let member = tx.get_member(id).await?;
        tx.commit().await?;
        Ok(member)
    }

    /// An admin sets a member's password (`hash`, Argon2id), optionally
    /// forcing a change at their next sign-in, and ends their sessions and
    /// codes, audited.
    pub async fn set_member_password(
        &self,
        id: MemberId,
        hash: &str,
        must_change: bool,
        actor: &str,
    ) -> Result<Member> {
        let mut tx = self.store.begin().await?;
        let member = tx.get_member(id).await?;
        tx.set_password(id, Some(hash), must_change).await?;
        tx.delete_member_tokens(id).await?;
        let note = if must_change {
            " (change at next sign-in)"
        } else {
            ""
        };
        audit(
            &mut tx,
            actor,
            &member.username,
            &format!("password set by an admin{note}"),
        )
        .await?;
        let member = tx.get_member(id).await?;
        tx.commit().await?;
        Ok(member)
    }

    /// The member with this email (any case), if any.
    pub async fn member_by_email(&self, email: &str) -> Result<Option<Member>> {
        Ok(self.store.read().await?.member_by_email(email).await?)
    }

    /// The member's password hash, if they set one.
    pub async fn password_hash(&self, id: MemberId) -> Result<Option<String>> {
        Ok(self.store.read().await?.password_hash(id).await?)
    }

    /// Starts session `session_hash` of `member` until `expires_at` after a
    /// password sign-in (checked by the caller), audited.
    pub async fn sign_in_password(
        &self,
        member: &Member,
        session_hash: &str,
        expires_at: Timestamp,
    ) -> Result<()> {
        let mut tx = self.store.begin().await?;
        tx.insert_login_session(session_hash, member.id, expires_at)
            .await?;
        login(&mut tx, member, "password").await?;
        tx.commit().await?;
        Ok(())
    }

    /// Signs `member` in with an emailed code (an invite or a sign-up):
    /// uses `code_hash` up, records their email as verified (the first
    /// verified person becomes the deployment owner, see
    /// [`Engine::verify_email`]), flags them to set a password before
    /// anything else, and starts the session, audited. `None` (only the
    /// failed try counted) when the code is wrong, used, expired or locked.
    pub async fn sign_in_with_code(
        &self,
        member: MemberId,
        code_hash: &str,
        session_hash: &str,
        expires_at: Timestamp,
    ) -> Result<Option<Member>> {
        let mut tx = self.store.begin().await?;
        if !tx.consume_otp(member, code_hash).await? {
            tx.commit().await?;
            return Ok(None);
        }
        tx.set_password(member, None, true).await?;
        verified(&mut tx, member).await?;
        tx.insert_login_session(session_hash, member, expires_at)
            .await?;
        let member = tx.get_member(member).await?;
        login(&mut tx, &member, "invite code").await?;
        tx.commit().await?;
        Ok(Some(member))
    }

    /// Sets the member's own password (`hash`, Argon2id) and lifts the
    /// must-set-password gate, audited.
    pub async fn set_own_password(&self, id: MemberId, hash: &str) -> Result<()> {
        let mut tx = self.store.begin().await?;
        let member = tx.get_member(id).await?;
        tx.set_password(id, Some(hash), false).await?;
        let actor = format!("user:{}", member.username);
        audit(&mut tx, &actor, &member.username, "password set").await?;
        tx.commit().await?;
        Ok(())
    }

    /// Gives a member from before invites their email (normalized), audited.
    pub async fn set_own_email(&self, id: MemberId, email: &str) -> Result<Member> {
        let mut tx = self.store.begin().await?;
        let member = tx.get_member(id).await?;
        tx.set_member_email(id, email).await?;
        let actor = format!("user:{}", member.username);
        audit(&mut tx, &actor, &member.username, &format!("email {email}")).await?;
        let member = tx.get_member(id).await?;
        tx.commit().await?;
        Ok(member)
    }

    /// The local owner's member row (the token holder `user`), if any.
    pub async fn owner_member(&self, user: &str) -> Result<Option<Member>> {
        Ok(self
            .store
            .read()
            .await?
            .member_by_identity(OWNER_ISSUER, user)
            .await?)
    }

    /// Creates or updates the local owner's member row (`user`, an owner)
    /// with `email` (normalized), audited.
    pub async fn set_owner_email(&self, user: &str, email: &str) -> Result<Member> {
        let mut tx = self.store.begin().await?;
        if let Some(other) = tx.member_by_email(email).await?
            && (other.issuer != OWNER_ISSUER || other.subject != user)
        {
            return Err(Error::InvalidInput(
                "Someone with that email is already a member.".into(),
            ));
        }
        let member = tx
            .upsert_member(OWNER_ISSUER, user, user, Some(email), true)
            .await?;
        audit(
            &mut tx,
            &format!("user:{user}"),
            user,
            &format!("email {email}"),
        )
        .await?;
        tx.commit().await?;
        Ok(member)
    }

    /// Seeds the first-run owner (the token holder `user`, a deployment
    /// owner) with password `hash` and the placeholder
    /// [`FIRST_ADMIN_EMAIL`], or gives an owner older row (made at
    /// onboarding, without a password) that password, audited; either must
    /// change it at their first sign-in. A deployment owner passes every
    /// `who: role:<key>` approval and sees every workspace, so they need no
    /// membership. The owner's email when it did either, `None` when the
    /// owner already has a password (nothing written).
    pub async fn seed_owner(&self, user: &str, hash: &str) -> Result<Option<String>> {
        let mut tx = self.store.begin().await?;
        let member = match tx.member_by_identity(OWNER_ISSUER, user).await? {
            Some(member) if member.has_password => return Ok(None),
            Some(member) => member,
            None => {
                tx.upsert_member(OWNER_ISSUER, user, user, Some(FIRST_ADMIN_EMAIL), true)
                    .await?
            }
        };
        // The token is printed (and may sit in service logs): it must be
        // replaced at the first sign-in, which rotates the token.
        tx.set_password(member.id, Some(hash), true).await?;
        audit(
            &mut tx,
            &format!("user:{user}"),
            user,
            "first-run password (the kernel token)",
        )
        .await?;
        tx.commit().await?;
        Ok(Some(
            member.email.unwrap_or_else(|| FIRST_ADMIN_EMAIL.into()),
        ))
    }

    /// Makes the member replace their password at their next sign-in,
    /// audited as `actor` with `why` (an owner whose password is still
    /// the kernel token, printed at first run).
    pub async fn force_password_change(&self, id: MemberId, actor: &str, why: &str) -> Result<()> {
        let mut tx = self.store.begin().await?;
        let member = tx.get_member(id).await?;
        let hash = tx.password_hash(id).await?;
        tx.set_password(id, hash.as_deref(), true).await?;
        audit(&mut tx, actor, &member.username, why).await?;
        tx.commit().await?;
        Ok(())
    }

    /// An OIDC sign-in: the member already linked to `(issuer, subject)`, or
    /// else the invited (not yet linked) member whose email is the provider's
    /// verified `email`, which is linked now. Their email counts as verified
    /// (the first verified person becomes the deployment owner, see
    /// [`Engine::verify_email`]). Starts session `session_hash`
    /// until `expires_at` and audits the login (detail `how`, e.g. `signed in
    /// with sso:google`), in one transaction. `None` (nothing written): nobody
    /// invited them. A deactivated member is `Forbidden` (nothing written).
    pub async fn sign_in_oidc(
        &self,
        issuer: &str,
        subject: &str,
        verified_email: Option<&str>,
        session_hash: &str,
        expires_at: Timestamp,
        how: &str,
    ) -> Result<Option<Member>> {
        let mut tx = self.store.begin().await?;
        let member = match tx.member_by_identity(issuer, subject).await? {
            Some(member) => member,
            None => {
                let invited = match verified_email {
                    Some(email) => tx.member_by_email(email).await?,
                    None => None,
                };
                match invited.filter(|m| m.issuer == LOCAL_ISSUER) {
                    Some(member) => {
                        tx.link_identity(member.id, issuer, subject).await?;
                        member
                    }
                    None => return Ok(None),
                }
            }
        };
        if member.deactivated_at.is_some() {
            return Err(Error::Forbidden(format!(
                "{} is deactivated",
                member.username
            )));
        }
        verified(&mut tx, member.id).await?;
        tx.insert_sso_session(session_hash, member.id, expires_at)
            .await?;
        login(&mut tx, &member, how).await?;
        tx.commit().await?;
        Ok(Some(member))
    }

    /// The SMTP settings (`None` until an admin saves them) and whether the
    /// SMTP password is stored.
    pub async fn smtp_settings(&self) -> Result<(Option<SmtpSettings>, bool)> {
        let mut tx = self.store.read().await?;
        let settings = match tx.setting(SMTP).await? {
            Some(v) => {
                Some(serde_json::from_value(v).map_err(|e| Error::InvalidInput(e.to_string()))?)
            }
            None => None,
        };
        let has_password = tx
            .list_secrets(Some(&ecosystem_scope()?))
            .await?
            .iter()
            .any(|s| s.name == SMTP_PASSWORD);
        Ok((settings, has_password))
    }

    /// Saves the SMTP settings after [`SmtpSettings::validate`]; `password`
    /// `Some("")` clears the stored one, `Some(p)` seals it as the ecosystem
    /// secret [`SMTP_PASSWORD`]. Audited as `actor` (never the password).
    pub async fn set_smtp_settings(
        &self,
        settings: &SmtpSettings,
        password: Option<&str>,
        actor: &str,
    ) -> Result<()> {
        settings.validate()?;
        let mut tx = self.store.begin().await?;
        tx.put_setting(SMTP, &json!(settings)).await?;
        let note = match password {
            Some("") => ", password cleared",
            Some(_) => ", password set",
            None => "",
        };
        let detail = format!(
            "smtp {}:{} {}{note}",
            settings.host, settings.port, settings.tls
        );
        tx.append_audit(actor, AuditAction::SettingsChange, SMTP, &detail)
            .await?;
        match password {
            Some("") => {
                tx.delete_secret(&ecosystem_scope()?, SMTP_PASSWORD).await?;
            }
            Some(password) => {
                tx.set_secret(&ecosystem_scope()?, SMTP_PASSWORD, password)
                    .await?;
            }
            None => {}
        }
        tx.commit().await?;
        Ok(())
    }

    /// The stored SMTP password, if any (for the mailer only).
    pub async fn smtp_password(&self) -> Result<Option<String>> {
        let mut tx = self.store.read().await?;
        match tx.secret_value(&ecosystem_scope()?, SMTP_PASSWORD).await {
            Ok(value) => Ok(Some(value)),
            Err(e) => match Error::from(e) {
                Error::NotFound(_) => Ok(None),
                e => Err(e),
            },
        }
    }
}
