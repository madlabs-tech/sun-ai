//! `member`, `login_token` and `audit`: who may sign in,
//! whether they are a deployment owner, whether they verified their email,
//! password hash and whether they are deactivated, whether they set up a second
//! step, their sign-in sessions, invite codes and 2-step challenges, and the append-only log of security-relevant actions.
//! Emails are stored lowercase; '' means a member from before invites that has not
//! given one yet (read back as `None`).

use async_trait::async_trait;
use chrono::SubsecRound;
use sqlx::Row;
use sqlx::sqlite::SqliteRow;
use sun_core::model::{AuditAction, AuditEntry, LOCAL_ISSUER, Member};
use sun_core::{AuditId, MemberId, Timestamp};

use crate::repo::{parse, parse_ts, ts};
use crate::{Result, StoreError, Tx};

/// **Repository** for `member` rows.
#[async_trait]
pub trait MemberRepo {
    /// The member signed in as `(issuer, subject)`, created in no workspace
    /// (a deployment owner when `is_owner`) when new. An existing member
    /// keeps its memberships and owner flag; its username and email are
    /// refreshed.
    async fn upsert_member(
        &mut self,
        issuer: &str,
        subject: &str,
        username: &str,
        email: Option<&str>,
        is_owner: bool,
    ) -> Result<Member>;
    /// The member, or `NotFound`.
    async fn get_member(&mut self, id: MemberId) -> Result<Member>;
    /// Every member, oldest first.
    async fn members(&mut self) -> Result<Vec<Member>>;
    /// Makes the member an owner or not, or `NotFound`.
    async fn set_member_owner(&mut self, id: MemberId, is_owner: bool) -> Result<()>;
    /// A new invited member (`issuer = local`, `subject` = the email) in no
    /// workspace, without a password. `email` must be normalized; a taken
    /// one is `InvalidInput`.
    async fn invite_member(
        &mut self,
        email: &str,
        username: &str,
        is_owner: bool,
    ) -> Result<Member>;
    /// A new member who signed up with `email` (normalized; also the
    /// username): `issuer = local`, no password, email not verified yet, in
    /// no workspace. A taken email is `InvalidInput`.
    async fn create_unverified(&mut self, email: &str) -> Result<Member>;
    /// Records that the member proved they own their email (now), unless
    /// already verified; `NotFound` for an unknown member.
    async fn verify_email(&mut self, id: MemberId) -> Result<()>;
    /// The member with this (normalized) email, if any.
    async fn member_by_email(&mut self, email: &str) -> Result<Option<Member>>;
    /// The member signed in as `(issuer, subject)`, if any.
    async fn member_by_identity(&mut self, issuer: &str, subject: &str) -> Result<Option<Member>>;
    /// Links an invited member to an OIDC identity.
    async fn link_identity(&mut self, id: MemberId, issuer: &str, subject: &str) -> Result<()>;
    /// Sets the (normalized) email; one another member has is `InvalidInput`.
    /// A different address than before is not verified until proven again.
    async fn set_member_email(&mut self, id: MemberId, email: &str) -> Result<()>;
    /// The member's Argon2id PHC string, if they set a password.
    async fn password_hash(&mut self, id: MemberId) -> Result<Option<String>>;
    /// Sets (`None`: clears) the password hash and the must-set-password flag.
    async fn set_password(
        &mut self,
        id: MemberId,
        hash: Option<&str>,
        must_set_password: bool,
    ) -> Result<()>;
    /// Reactivates the member, or deactivates them (now) and deletes every
    /// `login_token` row of theirs; `false` when there is no such member.
    async fn set_member_active(&mut self, id: MemberId, active: bool) -> Result<bool>;
}

/// **Repository** for the append-only `audit` log.
#[async_trait]
pub trait AuditRepo {
    /// Appends one entry (id and time are set here) and returns it.
    async fn append_audit(
        &mut self,
        actor: &str,
        action: AuditAction,
        target: &str,
        detail: &str,
    ) -> Result<AuditEntry>;
    /// Up to `limit` entries with an id below `before` (all when `None`),
    /// newest first.
    async fn audit_log(&mut self, before: Option<AuditId>, limit: u32) -> Result<Vec<AuditEntry>>;
}

/// **Repository** for `login_token` rows: members' sign-in sessions and
/// invite codes, keyed by `sha256(token)` (the caller hashes; the token
/// itself is never stored). Expired rows are ignored, and pruned on every
/// insert.
#[async_trait]
pub trait LoginRepo {
    /// Stores a session of `member` until `expires_at` (a password or code
    /// sign-in).
    async fn insert_login_session(
        &mut self,
        token_hash: &str,
        member: MemberId,
        expires_at: Timestamp,
    ) -> Result<()>;
    /// Stores a session of `member` an SSO sign-in started (these skip
    /// Sun's 2-step).
    async fn insert_sso_session(
        &mut self,
        token_hash: &str,
        member: MemberId,
        expires_at: Timestamp,
    ) -> Result<()>;
    /// The live session's member and whether SSO started it; `None` when
    /// unknown or expired.
    async fn login_session(&mut self, token_hash: &str) -> Result<Option<(MemberId, bool)>>;
    /// Ends a session; unknown is a no-op.
    async fn delete_login_session(&mut self, token_hash: &str) -> Result<()>;
    /// Stores `member`'s invite code until `expires_at`, replacing any
    /// earlier one. `code_hash` is `sha256(email ':' code)`.
    async fn insert_otp(
        &mut self,
        member: MemberId,
        code_hash: &str,
        expires_at: Timestamp,
    ) -> Result<()>;
    /// Checks `member`'s live invite code against `code_hash` (constant
    /// time). A match uses it up (`true`); a miss counts an attempt, and the
    /// [`MAX_OTP_ATTEMPTS`]th miss deletes the code.
    async fn consume_otp(&mut self, member: MemberId, code_hash: &str) -> Result<bool>;
    /// Ends every session and invite code of `member`.
    async fn delete_member_tokens(&mut self, member: MemberId) -> Result<()>;
    /// Stores a sign-in challenge of `member` (the password was right,
    /// the second step is still due) until `expires_at`.
    async fn insert_two_step(
        &mut self,
        token_hash: &str,
        member: MemberId,
        expires_at: Timestamp,
    ) -> Result<()>;
    /// The live challenge's member and data (the API's JSON: a mailed
    /// code's hash, a passkey ceremony's state; or '').
    async fn two_step(&mut self, token_hash: &str) -> Result<Option<(MemberId, String)>>;
    /// Replaces the challenge's data.
    async fn set_two_step_data(&mut self, token_hash: &str, data: &str) -> Result<()>;
    /// Counts a failed try (or a mailed code); the [`MAX_OTP_ATTEMPTS`]th
    /// deletes the challenge.
    async fn fail_two_step(&mut self, token_hash: &str) -> Result<()>;
    /// Deletes the challenge (it was answered); `false` when it was gone
    /// already (answered twice at once, or expired).
    async fn end_two_step(&mut self, token_hash: &str) -> Result<bool>;
}

/// `login_token.data` of a session an SSO sign-in started.
const SSO: &str = "sso";

/// Wrong guesses an invite code survives before it is deleted.
pub const MAX_OTP_ATTEMPTS: i64 = 5;

/// `at` to the second, so stored `expires_at` text sorts as time.
pub(crate) fn second(at: &Timestamp) -> String {
    ts(&at.trunc_subsecs(0))
}

/// Prunes expired `login_token` rows, then inserts one.
async fn insert_login_token(
    tx: &mut Tx,
    token_hash: &str,
    kind: &str,
    member: MemberId,
    expires_at: Timestamp,
    data: &str,
) -> Result<()> {
    sqlx::query("DELETE FROM login_token WHERE expires_at <= $1")
        .bind(second(&chrono::Utc::now()))
        .execute(tx.conn())
        .await?;
    sqlx::query(
        "INSERT INTO login_token (token_hash, kind, member_id, expires_at, data)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(token_hash)
    .bind(kind)
    .bind(member.to_string())
    .bind(second(&expires_at))
    .bind(data)
    .execute(tx.conn())
    .await?;
    Ok(())
}

#[async_trait]
impl LoginRepo for Tx {
    async fn insert_login_session(
        &mut self,
        token_hash: &str,
        member: MemberId,
        expires_at: Timestamp,
    ) -> Result<()> {
        insert_login_token(self, token_hash, "session", member, expires_at, "").await
    }

    async fn insert_sso_session(
        &mut self,
        token_hash: &str,
        member: MemberId,
        expires_at: Timestamp,
    ) -> Result<()> {
        insert_login_token(self, token_hash, "session", member, expires_at, SSO).await
    }

    async fn login_session(&mut self, token_hash: &str) -> Result<Option<(MemberId, bool)>> {
        let row = sqlx::query(
            "SELECT member_id, data FROM login_token WHERE token_hash = $1 AND kind = 'session'
             AND member_id IS NOT NULL AND expires_at > $2",
        )
        .bind(token_hash)
        .bind(second(&chrono::Utc::now()))
        .fetch_optional(self.conn())
        .await?;
        row.map(|row| {
            let sso = row.try_get::<String, _>("data")? == SSO;
            Ok((parse(row.try_get::<String, _>("member_id")?)?, sso))
        })
        .transpose()
    }

    async fn delete_login_session(&mut self, token_hash: &str) -> Result<()> {
        sqlx::query("DELETE FROM login_token WHERE token_hash = $1 AND kind = 'session'")
            .bind(token_hash)
            .execute(self.conn())
            .await?;
        Ok(())
    }

    async fn insert_otp(
        &mut self,
        member: MemberId,
        code_hash: &str,
        expires_at: Timestamp,
    ) -> Result<()> {
        sqlx::query("DELETE FROM login_token WHERE member_id = $1 AND kind = 'otp'")
            .bind(member.to_string())
            .execute(self.conn())
            .await?;
        insert_login_token(self, code_hash, "otp", member, expires_at, "").await
    }

    async fn consume_otp(&mut self, member: MemberId, code_hash: &str) -> Result<bool> {
        let row = sqlx::query(
            "SELECT token_hash, attempts FROM login_token
             WHERE member_id = $1 AND kind = 'otp' AND expires_at > $2",
        )
        .bind(member.to_string())
        .bind(second(&chrono::Utc::now()))
        .fetch_optional(self.conn())
        .await?;
        let Some(row) = row else {
            return Ok(false);
        };
        let stored: String = row.try_get("token_hash")?;
        let attempts: i64 = row.try_get("attempts")?;
        let matches = stored.len() == code_hash.len()
            && stored
                .bytes()
                .zip(code_hash.bytes())
                .fold(0u8, |acc, (a, b)| acc | (a ^ b))
                == 0;
        let sql = if matches || attempts + 1 >= MAX_OTP_ATTEMPTS {
            "DELETE FROM login_token WHERE token_hash = $1"
        } else {
            "UPDATE login_token SET attempts = attempts + 1 WHERE token_hash = $1"
        };
        sqlx::query(sql).bind(&stored).execute(self.conn()).await?;
        Ok(matches)
    }

    async fn delete_member_tokens(&mut self, member: MemberId) -> Result<()> {
        sqlx::query("DELETE FROM login_token WHERE member_id = $1")
            .bind(member.to_string())
            .execute(self.conn())
            .await?;
        Ok(())
    }

    async fn insert_two_step(
        &mut self,
        token_hash: &str,
        member: MemberId,
        expires_at: Timestamp,
    ) -> Result<()> {
        insert_login_token(self, token_hash, "two_step", member, expires_at, "").await
    }

    async fn two_step(&mut self, token_hash: &str) -> Result<Option<(MemberId, String)>> {
        let row = sqlx::query(
            "SELECT member_id, data FROM login_token WHERE token_hash = $1 AND kind = 'two_step'
             AND member_id IS NOT NULL AND expires_at > $2",
        )
        .bind(token_hash)
        .bind(second(&chrono::Utc::now()))
        .fetch_optional(self.conn())
        .await?;
        row.map(|row| Ok((parse(row.try_get("member_id")?)?, row.try_get("data")?)))
            .transpose()
    }

    async fn set_two_step_data(&mut self, token_hash: &str, data: &str) -> Result<()> {
        sqlx::query("UPDATE login_token SET data = $1 WHERE token_hash = $2 AND kind = 'two_step'")
            .bind(data)
            .bind(token_hash)
            .execute(self.conn())
            .await?;
        Ok(())
    }

    async fn fail_two_step(&mut self, token_hash: &str) -> Result<()> {
        sqlx::query(
            "UPDATE login_token SET attempts = attempts + 1 WHERE token_hash = $1 AND kind = 'two_step'",
        )
        .bind(token_hash)
        .execute(self.conn())
        .await?;
        sqlx::query(
            "DELETE FROM login_token WHERE token_hash = $1 AND kind = 'two_step' AND attempts >= $2",
        )
        .bind(token_hash)
        .bind(MAX_OTP_ATTEMPTS)
        .execute(self.conn())
        .await?;
        Ok(())
    }

    async fn end_two_step(&mut self, token_hash: &str) -> Result<bool> {
        let done =
            sqlx::query("DELETE FROM login_token WHERE token_hash = $1 AND kind = 'two_step'")
                .bind(token_hash)
                .execute(self.conn())
                .await?;
        Ok(done.rows_affected() == 1)
    }
}

const MEMBER_COLUMNS: &str = "id, issuer, subject, username, email, is_owner";
/// [`MEMBER_COLUMNS`] plus what [`member_from`] derives the flags from.
const MEMBER_SELECT: &str = "id, issuer, subject, username, email, is_owner, password_hash, \
                             must_set_password, deactivated_at, email_verified_at";

/// `members` with their 2-step flag filled in (one query for all of them).
async fn with_two_step(tx: &mut Tx, mut members: Vec<Member>) -> Result<Vec<Member>> {
    if members.is_empty() {
        return Ok(members);
    }
    let one = (members.len() == 1).then(|| members[0].id.to_string());
    let with_two_step: Vec<String> = sqlx::query_scalar(&format!(
        "SELECT DISTINCT member_id FROM member_factor
         WHERE ($1 IS NULL OR member_id = $1) AND {}",
        crate::factor::CONFIRMED
    ))
    .bind(one)
    .fetch_all(tx.conn())
    .await?;
    for m in &mut members {
        m.has_two_step = with_two_step.contains(&m.id.to_string());
    }
    Ok(members)
}

/// One row as a [`Member`] with its 2-step flag.
async fn one(tx: &mut Tx, row: Option<SqliteRow>) -> Result<Option<Member>> {
    let Some(row) = row else {
        return Ok(None);
    };
    let member = member_from(&row)?;
    Ok(with_two_step(tx, vec![member]).await?.pop())
}

/// `InvalidInput` unless no member other than `except` has `email`.
async fn email_free(tx: &mut Tx, email: &str, except: Option<MemberId>) -> Result<()> {
    if let Some(other) = tx.member_by_email(email).await?
        && Some(other.id) != except
    {
        return Err(sun_core::Error::InvalidInput(
            "Someone with that email is already a member.".into(),
        )
        .into());
    }
    Ok(())
}

#[async_trait]
impl MemberRepo for Tx {
    async fn upsert_member(
        &mut self,
        issuer: &str,
        subject: &str,
        username: &str,
        email: Option<&str>,
        is_owner: bool,
    ) -> Result<Member> {
        let row = sqlx::query(&format!(
            "INSERT INTO member ({MEMBER_COLUMNS}) VALUES ($1, $2, $3, $4, $5, $6)
             ON CONFLICT (issuer, subject)
             DO UPDATE SET username = excluded.username, email = excluded.email
             RETURNING {MEMBER_SELECT}"
        ))
        .bind(MemberId::generate().to_string())
        .bind(issuer)
        .bind(subject)
        .bind(username)
        .bind(email.unwrap_or_default().to_ascii_lowercase())
        .bind(i64::from(is_owner))
        .fetch_one(self.conn())
        .await?;
        let member = one(self, Some(row)).await?;
        member.ok_or_else(|| StoreError::not_found("member", subject))
    }

    async fn get_member(&mut self, id: MemberId) -> Result<Member> {
        let row = sqlx::query(&format!("SELECT {MEMBER_SELECT} FROM member WHERE id = $1"))
            .bind(id.to_string())
            .fetch_optional(self.conn())
            .await?
            .ok_or_else(|| StoreError::not_found("member", id))?;
        let member = one(self, Some(row)).await?;
        member.ok_or_else(|| StoreError::not_found("member", id))
    }

    async fn members(&mut self) -> Result<Vec<Member>> {
        let members = sqlx::query(&format!("SELECT {MEMBER_SELECT} FROM member ORDER BY id"))
            .fetch_all(self.conn())
            .await?
            .iter()
            .map(member_from)
            .collect::<Result<Vec<_>>>()?;
        with_two_step(self, members).await
    }

    async fn set_member_owner(&mut self, id: MemberId, is_owner: bool) -> Result<()> {
        let done = sqlx::query("UPDATE member SET is_owner = $1 WHERE id = $2")
            .bind(i64::from(is_owner))
            .bind(id.to_string())
            .execute(self.conn())
            .await?;
        match done.rows_affected() {
            0 => Err(StoreError::not_found("member", id)),
            _ => Ok(()),
        }
    }

    async fn invite_member(
        &mut self,
        email: &str,
        username: &str,
        is_owner: bool,
    ) -> Result<Member> {
        email_free(self, email, None).await?;
        let row = sqlx::query(&format!(
            "INSERT INTO member ({MEMBER_COLUMNS}) VALUES ($1, $2, $3, $4, $5, $6)
             RETURNING {MEMBER_SELECT}"
        ))
        .bind(MemberId::generate().to_string())
        .bind(LOCAL_ISSUER)
        .bind(email)
        .bind(username)
        .bind(email)
        .bind(i64::from(is_owner))
        .fetch_one(self.conn())
        .await?;
        let member = one(self, Some(row)).await?;
        member.ok_or_else(|| StoreError::not_found("member", email))
    }

    async fn create_unverified(&mut self, email: &str) -> Result<Member> {
        self.invite_member(email, email, false).await
    }

    async fn verify_email(&mut self, id: MemberId) -> Result<()> {
        let done = sqlx::query(
            "UPDATE member SET email_verified_at = coalesce(email_verified_at, $1) WHERE id = $2",
        )
        .bind(ts(&chrono::Utc::now()))
        .bind(id.to_string())
        .execute(self.conn())
        .await?;
        match done.rows_affected() {
            0 => Err(StoreError::not_found("member", id)),
            _ => Ok(()),
        }
    }

    async fn member_by_email(&mut self, email: &str) -> Result<Option<Member>> {
        if email.is_empty() {
            return Ok(None);
        }
        let row = sqlx::query(&format!(
            "SELECT {MEMBER_SELECT} FROM member WHERE lower(email) = $1"
        ))
        .bind(email.to_ascii_lowercase())
        .fetch_optional(self.conn())
        .await?;
        one(self, row).await
    }

    async fn member_by_identity(&mut self, issuer: &str, subject: &str) -> Result<Option<Member>> {
        let row = sqlx::query(&format!(
            "SELECT {MEMBER_SELECT} FROM member WHERE issuer = $1 AND subject = $2"
        ))
        .bind(issuer)
        .bind(subject)
        .fetch_optional(self.conn())
        .await?;
        one(self, row).await
    }

    async fn link_identity(&mut self, id: MemberId, issuer: &str, subject: &str) -> Result<()> {
        sqlx::query("UPDATE member SET issuer = $1, subject = $2 WHERE id = $3")
            .bind(issuer)
            .bind(subject)
            .bind(id.to_string())
            .execute(self.conn())
            .await?;
        Ok(())
    }

    async fn set_member_email(&mut self, id: MemberId, email: &str) -> Result<()> {
        email_free(self, email, Some(id)).await?;
        // A new address is not proven yet: a verified domain must not carry
        // over to an inbox nobody showed they own.
        sqlx::query(
            "UPDATE member SET email = $1, email_verified_at = CASE \
                 WHEN lower(email) = lower($1) THEN email_verified_at ELSE NULL END \
             WHERE id = $2",
        )
        .bind(email)
        .bind(id.to_string())
        .execute(self.conn())
        .await?;
        Ok(())
    }

    async fn password_hash(&mut self, id: MemberId) -> Result<Option<String>> {
        Ok(
            sqlx::query("SELECT password_hash FROM member WHERE id = $1")
                .bind(id.to_string())
                .fetch_optional(self.conn())
                .await?
                .map(|row| row.try_get::<Option<String>, _>("password_hash"))
                .transpose()?
                .flatten(),
        )
    }

    async fn set_password(
        &mut self,
        id: MemberId,
        hash: Option<&str>,
        must_set_password: bool,
    ) -> Result<()> {
        sqlx::query("UPDATE member SET password_hash = $1, must_set_password = $2 WHERE id = $3")
            .bind(hash)
            .bind(i64::from(must_set_password))
            .bind(id.to_string())
            .execute(self.conn())
            .await?;
        Ok(())
    }

    async fn set_member_active(&mut self, id: MemberId, active: bool) -> Result<bool> {
        let at = (!active).then(|| ts(&chrono::Utc::now()));
        let done = sqlx::query("UPDATE member SET deactivated_at = $1 WHERE id = $2")
            .bind(at)
            .bind(id.to_string())
            .execute(self.conn())
            .await?;
        if !active {
            self.delete_member_tokens(id).await?;
        }
        Ok(done.rows_affected() > 0)
    }
}

#[async_trait]
impl AuditRepo for Tx {
    async fn append_audit(
        &mut self,
        actor: &str,
        action: AuditAction,
        target: &str,
        detail: &str,
    ) -> Result<AuditEntry> {
        let entry = AuditEntry {
            id: AuditId::generate(),
            actor: actor.to_owned(),
            action,
            target: target.to_owned(),
            detail: detail.to_owned(),
            at: chrono::Utc::now(),
        };
        sqlx::query(
            "INSERT INTO audit (id, actor, action, target, detail, at) VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(entry.id.to_string())
        .bind(&entry.actor)
        .bind(entry.action.as_str())
        .bind(&entry.target)
        .bind(&entry.detail)
        .bind(ts(&entry.at))
        .execute(self.conn())
        .await?;
        Ok(entry)
    }

    async fn audit_log(&mut self, before: Option<AuditId>, limit: u32) -> Result<Vec<AuditEntry>> {
        // ULIDs sort as text, so `id < before` pages back in time.
        let before = before.map_or_else(|| "~".to_owned(), |id| id.to_string());
        sqlx::query(
            "SELECT id, actor, action, target, detail, at FROM audit
             WHERE id < $1 ORDER BY id DESC LIMIT $2",
        )
        .bind(before)
        .bind(i64::from(limit))
        .fetch_all(self.conn())
        .await?
        .iter()
        .map(|row| {
            Ok(AuditEntry {
                id: parse(row.try_get("id")?)?,
                actor: row.try_get("actor")?,
                // Append-only: rows from before workspaces were renamed from
                // boards keep `board_change`.
                action: match row.try_get::<String, _>("action")?.as_str() {
                    "board_change" => AuditAction::WorkspaceChange,
                    action => parse(action.to_owned())?,
                },
                target: row.try_get("target")?,
                detail: row.try_get("detail")?,
                at: parse_ts(row.try_get("at")?)?,
            })
        })
        .collect()
    }
}

fn member_from(row: &SqliteRow) -> Result<Member> {
    let email: String = row.try_get("email")?;
    let hash: Option<String> = row.try_get("password_hash")?;
    Ok(Member {
        id: parse(row.try_get("id")?)?,
        issuer: row.try_get("issuer")?,
        subject: row.try_get("subject")?,
        username: row.try_get("username")?,
        email: (!email.is_empty()).then_some(email),
        is_owner: row.try_get::<i64, _>("is_owner")? != 0,
        email_verified_at: row
            .try_get::<Option<String>, _>("email_verified_at")?
            .map(parse_ts)
            .transpose()?,
        has_password: hash.is_some(),
        must_set_password: row.try_get::<i64, _>("must_set_password")? != 0,
        deactivated_at: row
            .try_get::<Option<String>, _>("deactivated_at")?
            .map(parse_ts)
            .transpose()?,
        has_two_step: false,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::Store;

    #[tokio::test]
    async fn members_keep_owner_flag_verify_once_and_audit_is_append_only() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).await.unwrap();
        let mut tx = store.begin().await.unwrap();
        let alice = tx
            .upsert_member("https://idp", "a1", "alice", None, false)
            .await
            .unwrap();
        assert!(!alice.is_owner && alice.email_verified_at.is_none());
        tx.set_member_owner(alice.id, true).await.unwrap();
        tx.verify_email(alice.id).await.unwrap();
        let verified = tx.get_member(alice.id).await.unwrap().email_verified_at;
        assert!(verified.is_some());
        // Signing in again refreshes the profile, not the owner flag; a
        // second verify keeps the first time.
        let again = tx
            .upsert_member("https://idp", "a1", "al", Some("a@x.io"), false)
            .await
            .unwrap();
        assert_eq!(
            (again.id, again.is_owner, again.username.as_str()),
            (alice.id, true, "al")
        );
        tx.verify_email(alice.id).await.unwrap();
        assert_eq!(again.email_verified_at, verified);
        assert!(tx.verify_email(MemberId::generate()).await.is_err());

        // Signing up: local, no password, unverified, the email as username.
        let bob = tx.create_unverified("bob@x.io").await.unwrap();
        assert_eq!(
            (
                bob.issuer.as_str(),
                bob.subject.as_str(),
                bob.username.as_str()
            ),
            (LOCAL_ISSUER, "bob@x.io", "bob@x.io")
        );
        assert!(!bob.has_password && bob.email_verified_at.is_none() && !bob.is_owner);
        assert!(tx.create_unverified("a@x.io").await.is_err(), "email taken");
        let entry = tx
            .append_audit("user:al", AuditAction::Login, "al", "oidc")
            .await
            .unwrap();
        tx.commit().await.unwrap();

        let mut tx = store.begin().await.unwrap();
        assert_eq!(tx.members().await.unwrap().len(), 2);
        assert_eq!(tx.audit_log(None, 10).await.unwrap(), vec![entry.clone()]);
        assert!(tx.audit_log(Some(entry.id), 10).await.unwrap().is_empty());
        for sql in ["UPDATE audit SET actor = 'x'", "DELETE FROM audit"] {
            let error = sqlx::query(sql).execute(tx.conn()).await.unwrap_err();
            assert!(error.to_string().contains("append-only"), "{error}");
        }
    }

    #[tokio::test]
    async fn login_sessions_expire_and_end() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).await.unwrap();
        let id = MemberId::generate();
        let later = chrono::Utc::now() + chrono::Duration::hours(1);
        let past = chrono::Utc::now() - chrono::Duration::seconds(1);
        let mut tx = store.begin().await.unwrap();
        tx.insert_login_session("s1", id, later).await.unwrap();
        tx.insert_login_session("old", id, past).await.unwrap();
        tx.insert_otp(id, "code", later).await.unwrap();
        tx.commit().await.unwrap();

        let mut tx = store.begin().await.unwrap();
        assert_eq!(tx.login_session("s1").await.unwrap(), Some((id, false)));
        tx.insert_sso_session("sso", id, later).await.unwrap();
        assert_eq!(tx.login_session("sso").await.unwrap(), Some((id, true)));
        assert_eq!(tx.login_session("old").await.unwrap(), None);
        // A code is not a session.
        assert_eq!(tx.login_session("code").await.unwrap(), None);
        tx.delete_login_session("s1").await.unwrap();
        assert_eq!(tx.login_session("s1").await.unwrap(), None);
        // The next insert prunes the expired rows.
        tx.insert_login_session("s2", id, later).await.unwrap();
        let left: i64 = sqlx::query("SELECT COUNT(*) AS n FROM login_token")
            .fetch_one(tx.conn())
            .await
            .unwrap()
            .try_get("n")
            .unwrap();
        assert_eq!(left, 3); // `code`, `sso` and `s2`
    }

    #[tokio::test]
    async fn auth_invites_emails_passwords_and_otp_lockout() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).await.unwrap();
        let mut tx = store.begin().await.unwrap();
        let ann = tx.invite_member("ann@x.io", "ann", false).await.unwrap();
        assert_eq!(
            (ann.issuer.as_str(), ann.subject.as_str()),
            ("local", "ann@x.io")
        );
        assert!(!ann.has_password && !ann.must_set_password);
        // Email is unique ignoring case, for invites and for later changes.
        assert!(tx.invite_member("ann@x.io", "a2", false).await.is_err());
        let old = tx
            .upsert_member("https://idp", "o1", "old", None, false)
            .await
            .unwrap();
        assert_eq!(old.email, None, "a pre-invite member without an email");
        assert!(tx.set_member_email(old.id, "ann@x.io").await.is_err());
        let dup = sqlx::query("UPDATE member SET email = 'ANN@x.io' WHERE id = $1")
            .bind(old.id.to_string())
            .execute(tx.conn())
            .await;
        assert!(dup.is_err(), "the unique index ignores case");
        tx.set_member_email(old.id, "old@x.io").await.unwrap();
        assert_eq!(
            tx.member_by_email("OLD@x.io").await.unwrap().unwrap().id,
            old.id
        );

        tx.set_password(ann.id, Some("$argon2id$fake"), true)
            .await
            .unwrap();
        let ann = tx.get_member(ann.id).await.unwrap();
        assert!(ann.has_password && ann.must_set_password);
        assert_eq!(
            tx.password_hash(ann.id).await.unwrap().as_deref(),
            Some("$argon2id$fake")
        );
        tx.link_identity(ann.id, "https://idp", "s-ann")
            .await
            .unwrap();
        let linked = tx.member_by_identity("https://idp", "s-ann").await.unwrap();
        assert_eq!(linked.unwrap().id, ann.id);

        // A code: wrong guesses count, a right one works once.
        let later = chrono::Utc::now() + chrono::Duration::minutes(15);
        tx.insert_otp(ann.id, "h1", later).await.unwrap();
        assert!(!tx.consume_otp(ann.id, "nope").await.unwrap());
        assert!(
            !tx.consume_otp(old.id, "h1").await.unwrap(),
            "another member's code"
        );
        assert!(tx.consume_otp(ann.id, "h1").await.unwrap());
        assert!(!tx.consume_otp(ann.id, "h1").await.unwrap());
        // The fifth miss kills the code, even for the right guess after it.
        tx.insert_otp(ann.id, "h2", later).await.unwrap();
        for _ in 0..MAX_OTP_ATTEMPTS {
            assert!(!tx.consume_otp(ann.id, "bad").await.unwrap());
        }
        assert!(!tx.consume_otp(ann.id, "h2").await.unwrap());
        // A new code replaces the old one; an expired one is refused.
        tx.insert_otp(ann.id, "h3", later).await.unwrap();
        tx.insert_otp(ann.id, "h4", later).await.unwrap();
        assert!(!tx.consume_otp(ann.id, "h3").await.unwrap());
        let past = chrono::Utc::now() - chrono::Duration::seconds(1);
        tx.insert_otp(ann.id, "h5", past).await.unwrap();
        assert!(!tx.consume_otp(ann.id, "h5").await.unwrap());

        // Deactivating ends their sessions;
        // reactivating clears the mark.
        tx.insert_login_session("s-ann", ann.id, later)
            .await
            .unwrap();
        assert!(tx.set_member_active(ann.id, false).await.unwrap());
        let ann = tx.get_member(ann.id).await.unwrap();
        assert!(ann.deactivated_at.is_some());
        assert_eq!(tx.login_session("s-ann").await.unwrap(), None);
        assert!(tx.set_member_active(ann.id, true).await.unwrap());
        assert!(
            tx.get_member(ann.id)
                .await
                .unwrap()
                .deactivated_at
                .is_none()
        );
        assert!(
            !tx.set_member_active(MemberId::generate(), false)
                .await
                .unwrap()
        );
    }
}
