//! `member`, `login_token` and `audit`: who may sign in,
//! whether they are an owner, their groups (`member_group`), password
//! hash and whether they are deactivated, whether they set up a second
//! step, their sign-in sessions, invite codes and 2-step challenges, and the append-only log of security-relevant actions.
//! Emails are stored lowercase; '' means a member from before invites that has not
//! given one yet (read back as `None`).

use async_trait::async_trait;
use chrono::SubsecRound;
use sqlx::Row;
use sqlx::sqlite::SqliteRow;
use sun_core::model::{AuditAction, AuditEntry, LOCAL_ISSUER, Member};
use sun_core::{AuditId, GroupId, MemberId, Timestamp};

use crate::repo::{parse, parse_ts, ts};
use crate::{Result, StoreError, Tx};

/// **Repository** for `member` rows.
#[async_trait]
pub trait MemberRepo {
    /// The member signed in as `(issuer, subject)`, created in no group
    /// (an owner when `is_owner`) when new. An existing member keeps its
    /// groups and owner flag; its username and email are refreshed.
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
    /// A new invited member (`issuer = local`, `subject` = the email) in
    /// `groups`, without a password. `email` must be normalized; a taken one
    /// is `InvalidInput`, an unknown group `NotFound`.
    async fn invite_member(
        &mut self,
        email: &str,
        username: &str,
        is_owner: bool,
        groups: &[GroupId],
    ) -> Result<Member>;
    /// The member with this (normalized) email, if any.
    async fn member_by_email(&mut self, email: &str) -> Result<Option<Member>>;
    /// The member signed in as `(issuer, subject)`, if any.
    async fn member_by_identity(&mut self, issuer: &str, subject: &str) -> Result<Option<Member>>;
    /// Links an invited member to an OIDC identity.
    async fn link_identity(&mut self, id: MemberId, issuer: &str, subject: &str) -> Result<()>;
    /// Sets the (normalized) email; one another member has is `InvalidInput`.
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
                             must_set_password, deactivated_at";

/// `members` with their group ids filled in (one query for all of them).
async fn with_groups(tx: &mut Tx, mut members: Vec<Member>) -> Result<Vec<Member>> {
    if members.is_empty() {
        return Ok(members);
    }
    let one = (members.len() == 1).then(|| members[0].id.to_string());
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT member_id, group_id FROM member_group
         WHERE $1 IS NULL OR member_id = $1 ORDER BY group_id",
    )
    .bind(one.clone())
    .fetch_all(tx.conn())
    .await?;
    for (member, group) in rows {
        if let Some(m) = members.iter_mut().find(|m| m.id.to_string() == member) {
            m.groups.push(parse(group)?);
        }
    }
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

/// One row as a [`Member`] with its groups.
async fn one(tx: &mut Tx, row: Option<SqliteRow>) -> Result<Option<Member>> {
    let Some(row) = row else {
        return Ok(None);
    };
    let member = member_from(&row)?;
    Ok(with_groups(tx, vec![member]).await?.pop())
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
        with_groups(self, members).await
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
        groups: &[GroupId],
    ) -> Result<Member> {
        use crate::GroupRepo;
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
        let id = member_from(&row)?.id;
        self.set_member_groups(id, groups).await?;
        self.get_member(id).await
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
        sqlx::query("UPDATE member SET email = $1 WHERE id = $2")
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
        groups: Vec::new(),
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
    async fn acl_members_keep_groups_and_owner_and_audit_is_append_only() {
        use crate::GroupRepo;
        use sun_core::model::Permission;
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).await.unwrap();
        let mut tx = store.begin().await.unwrap();
        let alice = tx
            .upsert_member("https://idp", "a1", "alice", None, false)
            .await
            .unwrap();
        assert!(alice.groups.is_empty() && !alice.is_owner);
        tx.set_member_groups(alice.id, &[GroupId::ADMIN, GroupId::VIEWER])
            .await
            .unwrap();
        tx.set_member_owner(alice.id, true).await.unwrap();
        // Signing in again refreshes the profile, not the groups.
        let again = tx
            .upsert_member("https://idp", "a1", "al", Some("a@x.io"), false)
            .await
            .unwrap();
        assert_eq!(
            (again.id, again.is_owner, again.username.as_str()),
            (alice.id, true, "al")
        );
        assert_eq!(again.groups, [GroupId::VIEWER, GroupId::ADMIN]);
        assert!(
            tx.set_member_groups(alice.id, &[GroupId::generate()])
                .await
                .is_err()
        );

        // Groups: seeded three, unique names, stable keys, delete drops
        // memberships.
        let seeded: Vec<_> = tx.groups().await.unwrap();
        let keys: Vec<&str> = seeded.iter().map(|g| g.key.as_str()).collect();
        assert_eq!(keys, ["viewer", "member", "admin"]);
        assert!(seeded[0].permissions.is_empty());
        assert_eq!(
            seeded[1].permissions,
            [
                Permission::Run,
                Permission::Approve,
                Permission::CreateWorkflow,
                Permission::EditWorkflow
            ]
        );
        assert!(!seeded[1].permissions.contains(&Permission::CreateWorkspace));
        assert_eq!(seeded[2].permissions.len(), Permission::ALL.len());
        assert!(tx.insert_group("ADMIN", &[]).await.is_err());
        let owner = tx.insert_group("Owner", &[]).await.unwrap();
        assert_eq!(owner.key, "owner_2", "`owner` is reserved");
        let b = tx
            .insert_group("Group B", &[Permission::EditWorkflow])
            .await
            .unwrap();
        let b2 = tx.insert_group("group-b", &[]).await.unwrap();
        assert_eq!((b.key.as_str(), b2.key.as_str()), ("group_b", "group_b_2"));
        tx.rename_group(b.id, "Builders").await.unwrap();
        assert!(tx.rename_group(b2.id, "builders").await.is_err());
        tx.set_group_permissions(b.id, &[Permission::CreateWorkspace])
            .await
            .unwrap();
        let b = tx.get_group(b.id).await.unwrap();
        assert_eq!(
            (b.key.as_str(), b.name.as_str(), b.permissions.as_slice()),
            ("group_b", "Builders", &[Permission::CreateWorkspace][..])
        );
        tx.set_group_members(b.id, &[alice.id]).await.unwrap();
        let mine: Vec<_> = tx.groups_of_member(alice.id).await.unwrap();
        assert_eq!(mine.len(), 3);
        tx.delete_group(b.id).await.unwrap();
        assert_eq!(tx.get_member(alice.id).await.unwrap().groups.len(), 2);
        assert!(tx.delete_group(b.id).await.is_err());
        let entry = tx
            .append_audit("user:al", AuditAction::Login, "al", "oidc")
            .await
            .unwrap();
        tx.commit().await.unwrap();

        let mut tx = store.begin().await.unwrap();
        assert_eq!(tx.members().await.unwrap().len(), 1);
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
        let ann = tx
            .invite_member("ann@x.io", "ann", false, &[GroupId::MEMBER])
            .await
            .unwrap();
        assert_eq!(
            (ann.issuer.as_str(), ann.subject.as_str()),
            ("local", "ann@x.io")
        );
        assert!(!ann.has_password && !ann.must_set_password);
        assert_eq!(ann.groups, [GroupId::MEMBER]);
        // Email is unique ignoring case, for invites and for later changes.
        assert!(
            tx.invite_member("ann@x.io", "a2", false, &[])
                .await
                .is_err()
        );
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

        // Deactivating ends their sessions and keeps their groups;
        // reactivating clears the mark.
        tx.insert_login_session("s-ann", ann.id, later)
            .await
            .unwrap();
        assert!(tx.set_member_active(ann.id, false).await.unwrap());
        let ann = tx.get_member(ann.id).await.unwrap();
        assert!(ann.deactivated_at.is_some());
        assert_eq!(ann.groups, [GroupId::MEMBER]);
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
