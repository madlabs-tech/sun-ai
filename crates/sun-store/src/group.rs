//! `member_group` and `member_group_member`: deployment-wide sets of people
//! ([`sun_core::group`]) and their manual members. Who is in a group is
//! [`GroupRepo::groups_of`], read per request; a deleted group's editor
//! grants go with it (a trigger).

use async_trait::async_trait;
use sqlx::Row;
use sqlx::sqlite::SqliteRow;
use sun_core::group::{MemberGroup, is_member};
use sun_core::{GroupId, MemberId};

use crate::repo::{parse, parse_ts, ts};
use crate::{Result, StoreError, Tx};

/// **Repository** for groups. Write the audit row in the same `Tx`.
#[async_trait]
pub trait GroupRepo {
    /// Every group, by name.
    async fn groups(&mut self) -> Result<Vec<MemberGroup>>;
    /// Group `id`; `NotFound` when there is none.
    async fn get_group(&mut self, id: GroupId) -> Result<MemberGroup>;
    /// Stores `group`; `InvalidInput` when its name is taken (ignoring case).
    async fn insert_group(&mut self, group: &MemberGroup) -> Result<()>;
    /// Saves `group`'s name, description and domains; `NotFound` when there
    /// is none, `InvalidInput` when the name is taken.
    async fn update_group(&mut self, group: &MemberGroup) -> Result<()>;
    /// Deletes group `id` (its members and editor grants with it) and
    /// returns it; `NotFound` when there is none.
    async fn delete_group(&mut self, id: GroupId) -> Result<MemberGroup>;
    /// The members added to `group` by hand, oldest first.
    async fn group_member_ids(&mut self, group: GroupId) -> Result<Vec<MemberId>>;
    /// Adds `member` to `group` by hand; `false` when they already were.
    async fn add_group_member(
        &mut self,
        group: GroupId,
        member: MemberId,
        added_by: Option<MemberId>,
    ) -> Result<bool>;
    /// Removes `member` from `group`'s manual members; `false` when they
    /// weren't one.
    async fn remove_group_member(&mut self, group: GroupId, member: MemberId) -> Result<bool>;
    /// The one membership query, by name: the groups `member` is in now
    /// ([`is_member`]: added by hand, or a verified email on one of the
    /// group's domains). None for a deactivated or unknown member. Never
    /// cached, so a removal or an email change applies to the next request.
    async fn groups_of(&mut self, member: MemberId) -> Result<Vec<MemberGroup>>;
}

const COLUMNS: &str = "id, name, description, domains, created_by, created_at";

fn group_from(row: &SqliteRow) -> Result<MemberGroup> {
    let created_by: Option<String> = row.try_get("created_by")?;
    let domains: String = row.try_get("domains")?;
    Ok(MemberGroup {
        id: parse(row.try_get("id")?)?,
        name: row.try_get("name")?,
        description: row.try_get("description")?,
        domains: serde_json::from_str(&domains)
            .map_err(|e| sun_core::Error::InvalidInput(format!("group domains: {e}")))?,
        created_by: created_by.map(parse).transpose()?,
        created_at: parse_ts(row.try_get("created_at")?)?,
    })
}

/// A unique name violation as `InvalidInput`.
fn name_taken(done: std::result::Result<u64, sqlx::Error>) -> Result<u64> {
    match done {
        Ok(rows) => Ok(rows),
        Err(sqlx::Error::Database(db)) if db.is_unique_violation() => {
            Err(sun_core::Error::InvalidInput("A group with that name exists.".into()).into())
        }
        Err(e) => Err(e.into()),
    }
}

fn domains_json(group: &MemberGroup) -> String {
    serde_json::Value::from(group.domains.clone()).to_string()
}

#[async_trait]
impl GroupRepo for Tx {
    async fn groups(&mut self) -> Result<Vec<MemberGroup>> {
        sqlx::query(&format!(
            "SELECT {COLUMNS} FROM member_group ORDER BY name, id"
        ))
        .fetch_all(self.conn())
        .await?
        .iter()
        .map(group_from)
        .collect()
    }

    async fn get_group(&mut self, id: GroupId) -> Result<MemberGroup> {
        let row = sqlx::query(&format!("SELECT {COLUMNS} FROM member_group WHERE id = $1"))
            .bind(id.to_string())
            .fetch_optional(self.conn())
            .await?;
        match row {
            Some(row) => group_from(&row),
            None => Err(StoreError::not_found("group", id)),
        }
    }

    async fn insert_group(&mut self, group: &MemberGroup) -> Result<()> {
        let done = sqlx::query(&format!(
            "INSERT INTO member_group ({COLUMNS}) VALUES ($1, $2, $3, $4, $5, $6)"
        ))
        .bind(group.id.to_string())
        .bind(&group.name)
        .bind(&group.description)
        .bind(domains_json(group))
        .bind(group.created_by.map(|m| m.to_string()))
        .bind(ts(&group.created_at))
        .execute(self.conn())
        .await
        .map(|d| d.rows_affected());
        name_taken(done)?;
        Ok(())
    }

    async fn update_group(&mut self, group: &MemberGroup) -> Result<()> {
        let done = sqlx::query(
            "UPDATE member_group SET name = $2, description = $3, domains = $4 WHERE id = $1",
        )
        .bind(group.id.to_string())
        .bind(&group.name)
        .bind(&group.description)
        .bind(domains_json(group))
        .execute(self.conn())
        .await
        .map(|d| d.rows_affected());
        match name_taken(done)? {
            0 => Err(StoreError::not_found("group", group.id)),
            _ => Ok(()),
        }
    }

    async fn delete_group(&mut self, id: GroupId) -> Result<MemberGroup> {
        let row = sqlx::query(&format!(
            "DELETE FROM member_group WHERE id = $1 RETURNING {COLUMNS}"
        ))
        .bind(id.to_string())
        .fetch_optional(self.conn())
        .await?;
        match row {
            Some(row) => group_from(&row),
            None => Err(StoreError::not_found("group", id)),
        }
    }

    async fn group_member_ids(&mut self, group: GroupId) -> Result<Vec<MemberId>> {
        let ids: Vec<String> = sqlx::query_scalar(
            "SELECT member_id FROM member_group_member WHERE group_id = $1
             ORDER BY created_at, member_id",
        )
        .bind(group.to_string())
        .fetch_all(self.conn())
        .await?;
        ids.into_iter().map(parse).collect()
    }

    async fn add_group_member(
        &mut self,
        group: GroupId,
        member: MemberId,
        added_by: Option<MemberId>,
    ) -> Result<bool> {
        let done = sqlx::query(
            "INSERT INTO member_group_member (group_id, member_id, added_by, created_at)
             VALUES ($1, $2, $3, $4) ON CONFLICT DO NOTHING",
        )
        .bind(group.to_string())
        .bind(member.to_string())
        .bind(added_by.map(|m| m.to_string()))
        .bind(ts(&chrono::Utc::now()))
        .execute(self.conn())
        .await?;
        Ok(done.rows_affected() == 1)
    }

    async fn remove_group_member(&mut self, group: GroupId, member: MemberId) -> Result<bool> {
        let done =
            sqlx::query("DELETE FROM member_group_member WHERE group_id = $1 AND member_id = $2")
                .bind(group.to_string())
                .bind(member.to_string())
                .execute(self.conn())
                .await?;
        Ok(done.rows_affected() == 1)
    }

    async fn groups_of(&mut self, member: MemberId) -> Result<Vec<MemberGroup>> {
        let id = member.to_string();
        let who: Option<(Option<String>, bool)> = sqlx::query_as(
            "SELECT email, email_verified_at IS NOT NULL FROM member
             WHERE id = $1 AND deactivated_at IS NULL",
        )
        .bind(&id)
        .fetch_optional(self.conn())
        .await?;
        let Some((email, verified)) = who else {
            return Ok(Vec::new());
        };
        // ponytail: every group is read and matched here; a domain index
        // when groups number in the thousands.
        let rows = sqlx::query(&format!(
            "SELECT {COLUMNS}, EXISTS (
                 SELECT 1 FROM member_group_member m
                 WHERE m.group_id = member_group.id AND m.member_id = $1
             ) AS manual
             FROM member_group ORDER BY name, id"
        ))
        .bind(&id)
        .fetch_all(self.conn())
        .await?;
        let mut groups = Vec::new();
        for row in &rows {
            let group = group_from(row)?;
            let manual: bool = row.try_get("manual")?;
            if is_member(&group, manual, email.as_deref(), verified) {
                groups.push(group);
            }
        }
        Ok(groups)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MemberRepo, Store};

    fn group(name: &str, domains: &[&str]) -> MemberGroup {
        MemberGroup {
            id: GroupId::generate(),
            name: name.into(),
            description: String::new(),
            domains: domains.iter().map(|d| (*d).to_owned()).collect(),
            created_by: None,
            created_at: chrono::Utc::now(),
        }
    }

    fn names(groups: &[MemberGroup]) -> Vec<&str> {
        groups.iter().map(|g| g.name.as_str()).collect()
    }

    #[tokio::test]
    async fn groups_of_is_manual_members_and_verified_emails_on_a_domain() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).await.unwrap();
        let mut tx = store.begin().await.unwrap();
        let ana = tx
            .upsert_member("https://idp", "ana", "ana", Some("ana@MadLabs.tech"), false)
            .await
            .unwrap();
        let bob = tx.create_unverified("bob@madlabs.tech").await.unwrap();
        let eve = tx
            .upsert_member(
                "https://idp",
                "eve",
                "eve",
                Some("eve@evilmadlabs.tech"),
                false,
            )
            .await
            .unwrap();
        tx.verify_email(ana.id).await.unwrap();
        tx.verify_email(eve.id).await.unwrap();
        let team = group("Team", &["madlabs.tech"]);
        let ops = group("ops", &[]);
        tx.insert_group(&team).await.unwrap();
        tx.insert_group(&ops).await.unwrap();
        assert!(tx.insert_group(&group("TEAM", &[])).await.is_err(), "name");

        assert_eq!(names(&tx.groups_of(ana.id).await.unwrap()), ["Team"]);
        assert!(tx.groups_of(bob.id).await.unwrap().is_empty(), "unverified");
        assert!(tx.groups_of(eve.id).await.unwrap().is_empty(), "look-alike");

        assert!(tx.add_group_member(ops.id, eve.id, None).await.unwrap());
        assert!(!tx.add_group_member(ops.id, eve.id, None).await.unwrap());
        assert!(tx.add_group_member(ops.id, ana.id, None).await.unwrap());
        assert_eq!(names(&tx.groups_of(eve.id).await.unwrap()), ["ops"]);
        assert_eq!(
            names(&tx.groups_of(ana.id).await.unwrap()),
            ["ops", "Team"],
            "by name, any case"
        );
        assert_eq!(tx.group_member_ids(ops.id).await.unwrap(), [eve.id, ana.id]);

        // Revoked on the next read: a manual removal, a domain dropped, an
        // email moved off the domain.
        assert!(tx.remove_group_member(ops.id, eve.id).await.unwrap());
        assert!(!tx.remove_group_member(ops.id, eve.id).await.unwrap());
        assert!(tx.groups_of(eve.id).await.unwrap().is_empty());
        let moved = MemberGroup {
            domains: vec!["sun.test".into()],
            ..team.clone()
        };
        tx.update_group(&moved).await.unwrap();
        assert_eq!(names(&tx.groups_of(ana.id).await.unwrap()), ["ops"]);
        tx.update_group(&team).await.unwrap();
        tx.set_member_email(ana.id, "ana@gmail.com").await.unwrap();
        assert_eq!(names(&tx.groups_of(ana.id).await.unwrap()), ["ops"]);
        assert!(tx.update_group(&group("ghost", &[])).await.is_err());

        assert_eq!(tx.delete_group(ops.id).await.unwrap(), ops);
        assert!(tx.get_group(ops.id).await.is_err());
        assert!(tx.groups_of(ana.id).await.unwrap().is_empty());
        assert_eq!(names(&tx.groups().await.unwrap()), ["Team"]);
    }
}
