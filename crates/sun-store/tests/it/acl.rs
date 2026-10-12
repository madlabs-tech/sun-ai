//! The access upgrades: fixed roles became groups, then groups became
//! workspace roles again. Through both, everyone keeps their access and
//! every `who: role:<name>` that admitted them.
#![allow(clippy::unwrap_used)]

use crate::permission_split;

use sqlx::migrate::Migrate;
use sqlx::{Connection, SqliteConnection};
use sun_core::model::{Role, roles_of};
use sun_core::{MemberId, WorkspaceId};
use sun_store::{MemberRepo, MembershipRepo, SettingRepo, Store, WorkspaceRepo};

#[tokio::test]
async fn acl_upgrade_turns_old_roles_into_workspace_roles() {
    let dir = tempfile::tempdir().unwrap();
    let url = format!("sqlite://{}?mode=rwc", dir.path().join("sun.db").display());
    let mut conn = SqliteConnection::connect(&url).await.unwrap();
    conn.ensure_migrations_table().await.unwrap();
    let migrator = sqlx::migrate!("./migrations");
    for migration in migrator.iter().filter(|m| m.version < 20261013090000) {
        conn.apply(migration).await.unwrap();
    }
    // A customized map: members only run, viewers approve, admins can't
    // touch secrets.
    sqlx::query("INSERT INTO kernel_setting (key, value_json) VALUES ('role_permissions', $1)")
        .bind(
            r#"{"admin":["run","approve","manage_plugins","manage_extensions","manage_runtimes"],
            "member":["run"],"viewer":["approve"]}"#,
        )
        .execute(&mut conn)
        .await
        .unwrap();
    let mut ids = Vec::new();
    for role in ["viewer", "member", "admin", "owner"] {
        let id = MemberId::generate();
        sqlx::query(
            "INSERT INTO member (id, issuer, subject, username, email, role)
             VALUES ($1, 'https://idp', $2, $2, $3, $4)",
        )
        .bind(id.to_string())
        .bind(role)
        .bind(format!("{role}@x.io"))
        .bind(role)
        .execute(&mut conn)
        .await
        .unwrap();
        ids.push(id);
    }
    conn.close().await.unwrap();

    let store = Store::open(dir.path()).await.unwrap();
    let mut tx = store.read().await.unwrap();
    assert_eq!(tx.setting("role_permissions").await.unwrap(), None);

    // Each member gets the highest role their old one covered, in Main (the
    // only workspace): `who: role:<their old role>` keeps admitting them.
    // A customized permission map is not kept: roles grant fixed ones.
    let mut roles = Vec::new();
    for id in &ids {
        let member = tx.get_member(*id).await.unwrap();
        let role = tx.role_of(WorkspaceId::MAIN, *id).await.unwrap().unwrap();
        roles.push((role, roles_of(member.is_owner, &[role])));
        assert_eq!(
            tx.workspaces_of_member(*id).await.unwrap(),
            [WorkspaceId::MAIN]
        );
    }
    assert_eq!(roles[0], (Role::Viewer, vec!["viewer".to_owned()]));
    assert_eq!(roles[1], (Role::Member, vec!["member".to_owned()]));
    assert_eq!(roles[2], (Role::Admin, vec!["admin".to_owned()]));
    assert_eq!(roles[3], (Role::Owner, vec!["owner".to_owned()]));
    assert!(tx.get_member(ids[3]).await.unwrap().is_owner);
    assert!(!tx.get_member(ids[2]).await.unwrap().is_owner);

    let mut conn = SqliteConnection::connect(&url).await.unwrap();
    let violations: Vec<(String,)> =
        sqlx::query_as("SELECT \"table\" FROM pragma_foreign_key_check")
            .fetch_all(&mut conn)
            .await
            .unwrap();
    assert!(violations.is_empty(), "{violations:?}");
}

#[tokio::test]
async fn acl_fresh_install_has_roles_and_no_groups() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let mut tx = store.read().await.unwrap();
    assert!(tx.members_of(WorkspaceId::MAIN).await.unwrap().is_empty());
    let url = format!("sqlite://{}", dir.path().join("sun.db").display());
    let mut conn = SqliteConnection::connect(&url).await.unwrap();
    let groups: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM sqlite_schema WHERE name IN ('user_group', 'member_group', \
         'workspace_group')",
    )
    .fetch_one(&mut conn)
    .await
    .unwrap();
    assert_eq!(groups, 0);
}

/// `manage_access` becomes every member permission plus
/// `manage_groups` and `manage_settings`; `edit_workspace` also gets
/// `delete_workspace`. Other permissions and groups stay as they were.
#[tokio::test]
async fn acl_upgrade_splits_manage_access_and_edit_workspace() {
    let dir = tempfile::tempdir().unwrap();
    let url = format!("sqlite://{}?mode=rwc", dir.path().join("sun.db").display());
    let mut conn = SqliteConnection::connect(&url).await.unwrap();
    conn.ensure_migrations_table().await.unwrap();
    let migrator = sqlx::migrate!("./migrations");
    for migration in migrator
        .iter()
        .filter(|m| m.version < permission_split::VERSION)
    {
        conn.apply(migration).await.unwrap();
    }
    // Stop before groups become roles, to read the groups as they were.
    let groups_left = migrator.iter().filter(|m| {
        m.version >= permission_split::VERSION && m.version < crate::workspace_member::M1
    });
    for (id, key, permissions) in permission_split::OLD_GROUPS {
        sqlx::query(
            "INSERT INTO user_group (id, key, name, permissions_json) VALUES ($1, $2, $2, $3)",
        )
        .bind(id)
        .bind(key)
        .bind(permissions)
        .execute(&mut conn)
        .await
        .unwrap();
    }
    for migration in groups_left {
        conn.apply(migration).await.unwrap();
    }
    let rows: Vec<(String, String)> =
        sqlx::query_as("SELECT key, permissions_json FROM user_group ORDER BY id")
            .fetch_all(&mut conn)
            .await
            .unwrap();
    conn.close().await.unwrap();
    let groups: Vec<(String, Vec<String>)> = rows
        .into_iter()
        .map(|(key, json)| (key, serde_json::from_str(&json).unwrap()))
        .collect();
    permission_split::assert_split(&groups);
}
