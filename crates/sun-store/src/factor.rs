//! `member_factor`: members' second sign-in steps. An authenticator's
//! secret is sealed with the master key (AAD `member_factor \0 member \0 id`)
//! and opened only to check a code; `last_step` makes each code single use.
//! A passkey keeps its public credential (JSON) in `data`; its credential id
//! (`data.id`) is unique across members (index
//! `member_factor_passkey_credential`).

use async_trait::async_trait;
use sqlx::Row;
use sun_core::model::{Factor, FactorKind};
use sun_core::{Error, FactorId, MemberId};

use crate::repo::{parse, parse_ts, ts};
use crate::{Result, StoreError, Tx};

/// The `InvalidInput` message of a passkey whose credential is added already.
pub const PASSKEY_TAKEN: &str = "That passkey is added already.";

/// An authenticator's id, secret (plaintext) and last used time step;
/// `None` while it is still being added.
pub type TotpSecret = (FactorId, String, Option<i64>);

/// **Repository** for `member_factor` rows.
#[async_trait]
pub trait FactorRepo {
    /// The member's second steps, oldest first (authenticators still being
    /// added are left out).
    async fn factors(&mut self, member: MemberId) -> Result<Vec<Factor>>;
    /// Adds a second step. A `totp` one with `secret` (sealed here) is
    /// pending until [`Self::use_totp_step`] confirms it, and replaces the
    /// member's earlier pending one. `data` is public (a passkey's
    /// credential), '' when none. A passkey whose credential id (`id` in
    /// `data`) any member holds already is `InvalidInput`
    /// ([`PASSKEY_TAKEN`]), refused by a unique index.
    async fn insert_factor(
        &mut self,
        member: MemberId,
        kind: FactorKind,
        label: &str,
        secret: Option<&str>,
        data: &str,
    ) -> Result<Factor>;
    /// The member's passkeys: each id and its `data`, oldest first.
    async fn passkeys(&mut self, member: MemberId) -> Result<Vec<(FactorId, String)>>;
    /// Replaces the `data` of second step `id` (a passkey's sign count).
    async fn set_factor_data(&mut self, id: FactorId, data: &str) -> Result<()>;
    /// Every authenticator of the member, pending ones included.
    async fn totp_secrets(&mut self, member: MemberId) -> Result<Vec<TotpSecret>>;
    /// Marks time `step` of authenticator `id` used (which also confirms a
    /// pending one); `false` when that step or a later one was used already.
    async fn use_totp_step(&mut self, id: FactorId, step: i64) -> Result<bool>;
    /// Deletes one of the member's second steps; `false` when not theirs.
    async fn delete_factor(&mut self, member: MemberId, id: FactorId) -> Result<bool>;
    /// Deletes every second step of the member (an admin's reset); how many.
    async fn delete_factors(&mut self, member: MemberId) -> Result<u64>;
}

fn aad(member: MemberId, id: FactorId) -> Vec<u8> {
    format!("member_factor\0{member}\0{id}").into_bytes()
}

/// Confirmed rows: every kind but an authenticator without a used step.
pub(crate) const CONFIRMED: &str = "(kind <> 'totp' OR last_step IS NOT NULL)";

#[async_trait]
impl FactorRepo for Tx {
    async fn factors(&mut self, member: MemberId) -> Result<Vec<Factor>> {
        sqlx::query(&format!(
            "SELECT id, kind, label, created_at FROM member_factor
             WHERE member_id = $1 AND {CONFIRMED} ORDER BY id"
        ))
        .bind(member.to_string())
        .fetch_all(self.conn())
        .await?
        .iter()
        .map(|row| {
            Ok(Factor {
                id: parse(row.try_get("id")?)?,
                kind: parse(row.try_get("kind")?)?,
                label: row.try_get("label")?,
                created_at: parse_ts(row.try_get("created_at")?)?,
            })
        })
        .collect()
    }

    async fn insert_factor(
        &mut self,
        member: MemberId,
        kind: FactorKind,
        label: &str,
        secret: Option<&str>,
        data: &str,
    ) -> Result<Factor> {
        if kind == FactorKind::Totp {
            sqlx::query(
                "DELETE FROM member_factor
                 WHERE member_id = $1 AND kind = 'totp' AND last_step IS NULL",
            )
            .bind(member.to_string())
            .execute(self.conn())
            .await?;
        }
        let factor = Factor {
            id: FactorId::generate(),
            kind,
            label: label.to_owned(),
            created_at: chrono::Utc::now(),
        };
        let sealed = secret
            .map(|s| self.key().seal(&aad(member, factor.id), s.as_bytes()))
            .transpose()?;
        sqlx::query(
            "INSERT INTO member_factor (id, member_id, kind, label, secret_sealed, data, created_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(factor.id.to_string())
        .bind(member.to_string())
        .bind(kind.as_str())
        .bind(label)
        .bind(sealed)
        .bind(data)
        .bind(ts(&factor.created_at))
        .execute(self.conn())
        .await
        .map_err(|e| match &e {
            sqlx::Error::Database(db) if db.is_unique_violation() => {
                StoreError::Core(Error::InvalidInput(PASSKEY_TAKEN.into()))
            }
            _ => e.into(),
        })?;
        Ok(factor)
    }

    async fn passkeys(&mut self, member: MemberId) -> Result<Vec<(FactorId, String)>> {
        sqlx::query(
            "SELECT id, data FROM member_factor
             WHERE member_id = $1 AND kind = 'passkey' ORDER BY id",
        )
        .bind(member.to_string())
        .fetch_all(self.conn())
        .await?
        .iter()
        .map(|row| Ok((parse(row.try_get("id")?)?, row.try_get("data")?)))
        .collect()
    }

    async fn set_factor_data(&mut self, id: FactorId, data: &str) -> Result<()> {
        sqlx::query("UPDATE member_factor SET data = $1 WHERE id = $2")
            .bind(data)
            .bind(id.to_string())
            .execute(self.conn())
            .await?;
        Ok(())
    }

    async fn totp_secrets(&mut self, member: MemberId) -> Result<Vec<TotpSecret>> {
        let rows = sqlx::query(
            "SELECT id, secret_sealed, last_step FROM member_factor
             WHERE member_id = $1 AND kind = 'totp' AND secret_sealed IS NOT NULL ORDER BY id",
        )
        .bind(member.to_string())
        .fetch_all(self.conn())
        .await?;
        rows.iter()
            .map(|row| {
                let id: FactorId = parse(row.try_get("id")?)?;
                let sealed: Vec<u8> = row.try_get("secret_sealed")?;
                let secret = self.key().open(&aad(member, id), &sealed)?;
                Ok((id, secret, row.try_get("last_step")?))
            })
            .collect()
    }

    async fn use_totp_step(&mut self, id: FactorId, step: i64) -> Result<bool> {
        let done = sqlx::query(
            "UPDATE member_factor SET last_step = $1
             WHERE id = $2 AND kind = 'totp' AND (last_step IS NULL OR last_step < $1)",
        )
        .bind(step)
        .bind(id.to_string())
        .execute(self.conn())
        .await?;
        Ok(done.rows_affected() == 1)
    }

    async fn delete_factor(&mut self, member: MemberId, id: FactorId) -> Result<bool> {
        let done = sqlx::query("DELETE FROM member_factor WHERE id = $1 AND member_id = $2")
            .bind(id.to_string())
            .bind(member.to_string())
            .execute(self.conn())
            .await?;
        Ok(done.rows_affected() > 0)
    }

    async fn delete_factors(&mut self, member: MemberId) -> Result<u64> {
        let done = sqlx::query("DELETE FROM member_factor WHERE member_id = $1")
            .bind(member.to_string())
            .execute(self.conn())
            .await?;
        Ok(done.rows_affected())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::{LoginRepo, MemberRepo, Store};

    #[tokio::test]
    async fn auth_factors_seal_confirm_and_reject_replays() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).await.unwrap();
        let mut tx = store.begin().await.unwrap();
        let ann = tx.invite_member("ann@x.io", "ann", false).await.unwrap();
        let bob = MemberId::generate();
        assert!(!ann.has_two_step);

        // A pending authenticator isn't listed; a second start replaces it.
        let first = tx
            .insert_factor(ann.id, FactorKind::Totp, "Phone", Some("SECRET1"), "")
            .await
            .unwrap();
        let totp = tx
            .insert_factor(ann.id, FactorKind::Totp, "Phone", Some("SECRET2"), "")
            .await
            .unwrap();
        assert!(tx.factors(ann.id).await.unwrap().is_empty());
        assert_eq!(
            tx.totp_secrets(ann.id).await.unwrap(),
            [(totp.id, "SECRET2".to_owned(), None)]
        );
        assert!(!tx.use_totp_step(first.id, 5).await.unwrap());
        let sealed: Vec<u8> = sqlx::query_scalar("SELECT secret_sealed FROM member_factor")
            .fetch_one(tx.conn())
            .await
            .unwrap();
        assert!(!String::from_utf8_lossy(&sealed).contains("SECRET2"));

        // Confirming uses a step; that step and older ones never work again.
        assert!(tx.use_totp_step(totp.id, 100).await.unwrap());
        assert!(!tx.use_totp_step(totp.id, 100).await.unwrap());
        assert!(!tx.use_totp_step(totp.id, 99).await.unwrap());
        assert!(tx.use_totp_step(totp.id, 101).await.unwrap());
        let email = tx
            .insert_factor(ann.id, FactorKind::Email, "Email", None, "")
            .await
            .unwrap();
        // Ids made in one millisecond sort in any order; compare as a set.
        let mut ids: Vec<_> = tx
            .factors(ann.id)
            .await
            .unwrap()
            .iter()
            .map(|f| (f.id, f.kind))
            .collect();
        ids.sort_by_key(|(id, _)| *id);
        let mut want = vec![(totp.id, FactorKind::Totp), (email.id, FactorKind::Email)];
        want.sort_by_key(|(id, _)| *id);
        assert_eq!(ids, want);
        // A passkey keeps its credential in `data`, which can change.
        let key = tx
            .insert_factor(
                ann.id,
                FactorKind::Passkey,
                "Laptop",
                None,
                r#"{"id":[1],"n":0}"#,
            )
            .await
            .unwrap();
        tx.set_factor_data(key.id, r#"{"id":[1],"n":1}"#)
            .await
            .unwrap();
        assert_eq!(
            tx.passkeys(ann.id).await.unwrap(),
            [(key.id, r#"{"id":[1],"n":1}"#.to_owned())]
        );
        assert!(tx.passkeys(bob).await.unwrap().is_empty());
        // Its credential id is nobody else's, nor the same member's again.
        for member in [bob, ann.id] {
            let again = tx
                .insert_factor(
                    member,
                    FactorKind::Passkey,
                    "Key",
                    None,
                    r#"{"id":[1],"n":5}"#,
                )
                .await;
            assert!(
                matches!(&again, Err(StoreError::Core(Error::InvalidInput(m))) if m == PASSKEY_TAKEN),
                "{again:?}"
            );
        }
        let other = tx
            .insert_factor(bob, FactorKind::Passkey, "Key", None, r#"{"id":[2],"n":0}"#)
            .await
            .unwrap();
        assert!(tx.delete_factor(bob, other.id).await.unwrap());
        assert!(tx.get_member(ann.id).await.unwrap().has_two_step);

        // Only the owner deletes one; a reset deletes all.
        assert!(!tx.delete_factor(bob, email.id).await.unwrap());
        assert!(tx.delete_factor(ann.id, email.id).await.unwrap());
        assert_eq!(tx.delete_factors(ann.id).await.unwrap(), 2);
        assert!(!tx.get_member(ann.id).await.unwrap().has_two_step);

        // A sign-in challenge: its data, misses until it dies, and its end.
        let later = chrono::Utc::now() + chrono::Duration::minutes(5);
        tx.insert_two_step("c1", ann.id, later).await.unwrap();
        assert_eq!(
            tx.two_step("c1").await.unwrap(),
            Some((ann.id, String::new()))
        );
        assert_eq!(tx.login_session("c1").await.unwrap(), None);
        tx.set_two_step_data("c1", "code-hash").await.unwrap();
        assert_eq!(
            tx.two_step("c1").await.unwrap(),
            Some((ann.id, "code-hash".to_owned()))
        );
        for _ in 0..crate::MAX_OTP_ATTEMPTS {
            assert!(tx.two_step("c1").await.unwrap().is_some());
            tx.fail_two_step("c1").await.unwrap();
        }
        assert_eq!(tx.two_step("c1").await.unwrap(), None);
        tx.insert_two_step("c2", ann.id, later).await.unwrap();
        assert!(tx.end_two_step("c2").await.unwrap());
        assert!(!tx.end_two_step("c2").await.unwrap());
        assert_eq!(tx.two_step("c2").await.unwrap(), None);
    }
}
