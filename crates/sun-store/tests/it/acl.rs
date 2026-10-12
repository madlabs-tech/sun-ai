//! Groups: the upgrade from fixed roles to groups keeps everyone's
//! access and every `who: role:<name>` that admitted them.
#![allow(clippy::unwrap_used)]

use crate::permission_split;

use sqlx::migrate::Migrate;
use sqlx::{Connection, SqliteConnection};
use sun_core::model::{Permission, permissions_of, roles_of};
use sun_core::{GroupId, MemberId, WorkspaceId};
use sun_store::{GroupRepo, MemberRepo, SettingRepo, Store, WorkspaceRepo};

#[tokio::test]
async fn acl_upgrade_turns_roles_into_groups() {
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
    let groups = tx.groups().await.unwrap();
    let ids_and_keys: Vec<_> = groups.iter().map(|g| (g.id, g.key.as_str())).collect();
    assert_eq!(
        ids_and_keys,
        [
            (GroupId::VIEWER, "viewer"),
            (GroupId::MEMBER, "member"),
            (GroupId::ADMIN, "admin")
        ]
    );
    use Permission::*;
    assert_eq!(groups[0].permissions, [Approve]);
    assert_eq!(groups[1].permissions, [Run, CreateWorkflow, EditWorkflow]);
    assert_eq!(
        groups[2].permissions,
        [
            Run,
            Approve,
            ManagePlugins,
            ManageExtensions,
            ManageRuntimes,
            // The permission matrix split `manage_access` and `edit_workspace`.
            ManageSettings,
            InviteMembers,
            CreateMembers,
            ViewMembers,
            EditMembers,
            DeactivateMembers,
            ManageGroups,
            CreateWorkspace,
            EditWorkspace,
            DeleteWorkspace,
            CreateWorkflow,
            EditWorkflow,
            AccessAllWorkspaces
        ]
    );
    assert_eq!(tx.setting("role_permissions").await.unwrap(), None);

    // Each member is in every group their role covered, so `who:` keeps
    // admitting them; only the owner is an owner.
    let mut access = Vec::new();
    for id in &ids {
        let member = tx.get_member(*id).await.unwrap();
        let groups = tx.groups_of_member(*id).await.unwrap();
        access.push((
            member.is_owner,
            roles_of(member.is_owner, &groups),
            permissions_of(member.is_owner, &groups),
        ));
    }
    assert_eq!(access[0].1, ["viewer"]);
    assert_eq!(access[1].1, ["viewer", "member"]);
    assert_eq!(access[2].1, ["viewer", "member", "admin"]);
    assert_eq!(access[3].1, ["viewer", "member", "admin", "owner"]);
    assert_eq!(access[0].2, [Approve]);
    assert_eq!(access[1].2, [Run, Approve, CreateWorkflow, EditWorkflow]);
    assert!(access[2].2.contains(&ManageGroups) && !access[2].2.contains(&ManageSecrets));
    assert!(access[3].0 && access[3].2 == Permission::ALL);
    assert!(!access[2].0);

    // Main is open to Viewer, Member and Admin, so every member still
    // sees it after the upgrade.
    let main = [
        (WorkspaceId::MAIN, GroupId::VIEWER),
        (WorkspaceId::MAIN, GroupId::MEMBER),
        (WorkspaceId::MAIN, GroupId::ADMIN),
    ];
    assert_eq!(tx.workspace_groups().await.unwrap(), main);
    for id in &ids {
        assert_eq!(
            tx.workspaces_of_member(*id).await.unwrap(),
            [WorkspaceId::MAIN]
        );
    }

    let mut conn = SqliteConnection::connect(&url).await.unwrap();
    let violations: Vec<(String,)> =
        sqlx::query_as("SELECT \"table\" FROM pragma_foreign_key_check")
            .fetch_all(&mut conn)
            .await
            .unwrap();
    assert!(violations.is_empty(), "{violations:?}");
}

#[tokio::test]
async fn acl_fresh_install_seeds_default_groups() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let mut tx = store.read().await.unwrap();
    let groups = tx.groups().await.unwrap();
    let member = &groups[1];
    assert_eq!(
        member.permissions,
        [
            Permission::Run,
            Permission::Approve,
            Permission::CreateWorkflow,
            Permission::EditWorkflow
        ],
        "Member can't touch workspaces after install"
    );
    assert_eq!(groups[2].permissions, Permission::ALL);
    assert!(groups[0].permissions.is_empty());
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
    conn.close().await.unwrap();

    let store = Store::open(dir.path()).await.unwrap();
    let mut tx = store.read().await.unwrap();
    permission_split::assert_split(&tx.groups().await.unwrap());
}
