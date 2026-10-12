//! `user_group` and `member_group`: groups are roles an admin can
//! rename and customize, and a member may be in several. A group's `key`
//! (what approval `who: role:<key>` matches) is set once, from its first
//! name, and never changes.

use async_trait::async_trait;
use sqlx::Row;
use sqlx::sqlite::SqliteRow;
use sun_core::model::{Group, Permission};
use sun_core::{GroupId, MemberId};

use crate::repo::parse;
use crate::{Result, StoreError, Tx};

/// **Repository** for groups and who is in them. Names are checked with
/// [`Group::clean_name`] by the caller; here they must be unique ignoring case.
#[async_trait]
pub trait GroupRepo {
    /// Every group, oldest first (Viewer, Member, Admin, then the rest).
    async fn groups(&mut self) -> Result<Vec<Group>>;
    /// One group, or `NotFound`.
    async fn get_group(&mut self, id: GroupId) -> Result<Group>;
    /// A new group named `name`, keyed by [`Group::key_from_name`] made
    /// unique (`_2`, `_3`, …; `owner` is reserved). A taken name is
    /// `InvalidInput`.
    async fn insert_group(&mut self, name: &str, permissions: &[Permission]) -> Result<Group>;
    /// Renames a group (its key stays); `NotFound` or `InvalidInput`.
    async fn rename_group(&mut self, id: GroupId, name: &str) -> Result<()>;
    /// Replaces a group's permissions; `NotFound` when missing.
    async fn set_group_permissions(
        &mut self,
        id: GroupId,
        permissions: &[Permission],
    ) -> Result<()>;
    /// Deletes a group and its memberships; `NotFound` when missing.
    async fn delete_group(&mut self, id: GroupId) -> Result<()>;
    /// The groups `member` is in, oldest first.
    async fn groups_of_member(&mut self, member: MemberId) -> Result<Vec<Group>>;
    /// Puts `member` in exactly `groups`; an unknown group is `NotFound`.
    async fn set_member_groups(&mut self, member: MemberId, groups: &[GroupId]) -> Result<()>;
    /// Makes `members` exactly the group's members; an unknown member or
    /// group is `NotFound`.
    async fn set_group_members(&mut self, group: GroupId, members: &[MemberId]) -> Result<()>;
}

const GROUP_COLUMNS: &str = "id, key, name, permissions_json";

#[async_trait]
impl GroupRepo for Tx {
    async fn groups(&mut self) -> Result<Vec<Group>> {
        sqlx::query(&format!(
            "SELECT {GROUP_COLUMNS} FROM user_group ORDER BY id"
        ))
        .fetch_all(self.conn())
        .await?
        .iter()
        .map(group_from)
        .collect()
    }

    async fn get_group(&mut self, id: GroupId) -> Result<Group> {
        let row = sqlx::query(&format!(
            "SELECT {GROUP_COLUMNS} FROM user_group WHERE id = $1"
        ))
        .bind(id.to_string())
        .fetch_optional(self.conn())
        .await?
        .ok_or_else(|| StoreError::not_found("group", id))?;
        group_from(&row)
    }

    async fn insert_group(&mut self, name: &str, permissions: &[Permission]) -> Result<Group> {
        name_free(self, None, name).await?;
        let base = Group::key_from_name(name);
        let mut key = base.clone();
        let mut n = 1;
        loop {
            let taken: Option<String> =
                sqlx::query_scalar("SELECT id FROM user_group WHERE key = $1")
                    .bind(&key)
                    .fetch_optional(self.conn())
                    .await?;
            if taken.is_none() && key != "owner" {
                break;
            }
            n += 1;
            key = format!("{base}_{n}");
        }
        let group = Group {
            id: GroupId::generate(),
            key,
            name: name.to_owned(),
            permissions: permissions.to_vec(),
        };
        sqlx::query(
            "INSERT INTO user_group (id, key, name, permissions_json) VALUES ($1, $2, $3, $4)",
        )
        .bind(group.id.to_string())
        .bind(&group.key)
        .bind(&group.name)
        .bind(serde_json::to_string(&group.permissions)?)
        .execute(self.conn())
        .await?;
        Ok(group)
    }

    async fn rename_group(&mut self, id: GroupId, name: &str) -> Result<()> {
        name_free(self, Some(id), name).await?;
        let done = sqlx::query("UPDATE user_group SET name = $1 WHERE id = $2")
            .bind(name)
            .bind(id.to_string())
            .execute(self.conn())
            .await?;
        found(done.rows_affected(), id)
    }

    async fn set_group_permissions(
        &mut self,
        id: GroupId,
        permissions: &[Permission],
    ) -> Result<()> {
        let done = sqlx::query("UPDATE user_group SET permissions_json = $1 WHERE id = $2")
            .bind(serde_json::to_string(permissions)?)
            .bind(id.to_string())
            .execute(self.conn())
            .await?;
        found(done.rows_affected(), id)
    }

    async fn delete_group(&mut self, id: GroupId) -> Result<()> {
        sqlx::query("DELETE FROM member_group WHERE group_id = $1")
            .bind(id.to_string())
            .execute(self.conn())
            .await?;
        let done = sqlx::query("DELETE FROM user_group WHERE id = $1")
            .bind(id.to_string())
            .execute(self.conn())
            .await?;
        found(done.rows_affected(), id)
    }

    async fn groups_of_member(&mut self, member: MemberId) -> Result<Vec<Group>> {
        sqlx::query(
            "SELECT g.id, g.key, g.name, g.permissions_json FROM user_group g
             JOIN member_group mg ON mg.group_id = g.id
             WHERE mg.member_id = $1 ORDER BY g.id",
        )
        .bind(member.to_string())
        .fetch_all(self.conn())
        .await?
        .iter()
        .map(group_from)
        .collect()
    }

    async fn set_member_groups(&mut self, member: MemberId, groups: &[GroupId]) -> Result<()> {
        member_exists(self, member).await?;
        for group in groups {
            self.get_group(*group).await?;
        }
        sqlx::query("DELETE FROM member_group WHERE member_id = $1")
            .bind(member.to_string())
            .execute(self.conn())
            .await?;
        for group in groups {
            add(self, member, *group).await?;
        }
        Ok(())
    }

    async fn set_group_members(&mut self, group: GroupId, members: &[MemberId]) -> Result<()> {
        self.get_group(group).await?;
        for member in members {
            member_exists(self, *member).await?;
        }
        sqlx::query("DELETE FROM member_group WHERE group_id = $1")
            .bind(group.to_string())
            .execute(self.conn())
            .await?;
        for member in members {
            add(self, *member, group).await?;
        }
        Ok(())
    }
}

async fn add(tx: &mut Tx, member: MemberId, group: GroupId) -> Result<()> {
    sqlx::query(
        "INSERT INTO member_group (member_id, group_id) VALUES ($1, $2) ON CONFLICT DO NOTHING",
    )
    .bind(member.to_string())
    .bind(group.to_string())
    .execute(tx.conn())
    .await?;
    Ok(())
}

async fn member_exists(tx: &mut Tx, member: MemberId) -> Result<()> {
    let id: Option<String> = sqlx::query_scalar("SELECT id FROM member WHERE id = $1")
        .bind(member.to_string())
        .fetch_optional(tx.conn())
        .await?;
    match id {
        Some(_) => Ok(()),
        None => Err(StoreError::not_found("member", member)),
    }
}

fn found(rows: u64, id: GroupId) -> Result<()> {
    match rows {
        0 => Err(StoreError::not_found("group", id)),
        _ => Ok(()),
    }
}

/// `InvalidInput` when a group other than `except` is named `name` (any case).
async fn name_free(tx: &mut Tx, except: Option<GroupId>, name: &str) -> Result<()> {
    let taken: Option<String> =
        sqlx::query_scalar("SELECT id FROM user_group WHERE lower(name) = lower($1)")
            .bind(name)
            .fetch_optional(tx.conn())
            .await?;
    match taken {
        Some(id) if Some(id.as_str()) != except.map(|e| e.to_string()).as_deref() => Err(
            sun_core::Error::InvalidInput(format!("A group named `{name}` already exists.")).into(),
        ),
        _ => Ok(()),
    }
}

fn group_from(row: &SqliteRow) -> Result<Group> {
    let stored: String = row.try_get("permissions_json")?;
    let stored: Vec<Permission> = serde_json::from_str(&stored)?;
    Ok(Group {
        id: parse(row.try_get("id")?)?,
        key: row.try_get("key")?,
        name: row.try_get("name")?,
        // In `Permission::ALL` order, once each.
        permissions: Permission::ALL
            .iter()
            .copied()
            .filter(|p| stored.contains(p))
            .collect(),
    })
}
