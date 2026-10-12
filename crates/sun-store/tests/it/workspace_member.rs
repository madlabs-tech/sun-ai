//! Groups become workspace roles (`20261101090000_workspace_member`),
//! checked on a copy of an old-schema database; memberships and
//! invitations round-trip; a workspace always keeps an owner.
#![allow(clippy::unwrap_used)]

use std::path::Path;

use sqlx::migrate::Migrate;
use sqlx::{Connection, SqliteConnection};
use sun_core::model::{Role, WorkspaceInvitation};
use sun_core::{Error, InvitationId, MemberId, WorkspaceId};
use sun_store::{InvitationRepo, MemberRepo, MembershipRepo, Store, StoreError, WorkspaceRepo};

/// The migration that turns groups into workspace roles.
pub const M1: i64 = 20261101090000;

const OPS: &str = "01K0000000000000000000000W";
const EMPTY: &str = "01K0000000000000000000000X";
const LEADS: &str = "01K0000000000000000000000L";
const RUNNERS: &str = "01K0000000000000000000000R";
const VIEWER_GROUP: &str = "00000000000000000000000001";
const ADMIN_GROUP: &str = "00000000000000000000000003";

/// An old member: id, issuer, password hash, is_owner, groups.
type OldMember = (
    &'static str,
    &'static str,
    Option<&'static str>,
    i64,
    &'static [&'static str],
);

const MEMBERS: [OldMember; 7] = [
    // A deployment owner with a password.
    ("01K000000000000000000000M1", "local", Some("h"), 1, &[]),
    // The token holder.
    ("01K000000000000000000000M2", "token", None, 1, &[]),
    // In Viewer, which is attached to Main and to Ops.
    (
        "01K000000000000000000000M3",
        "local",
        Some("h"),
        0,
        &[VIEWER_GROUP],
    ),
    // In Admin, which sees every workspace.
    (
        "01K000000000000000000000M4",
        "https://idp",
        None,
        0,
        &[ADMIN_GROUP],
    ),
    // In a custom group with an admin-only permission, attached to Ops.
    (
        "01K000000000000000000000M5",
        "local",
        Some("h"),
        0,
        &[LEADS],
    ),
    // In a custom group that runs (attached nowhere) and in Viewer.
    (
        "01K000000000000000000000M6",
        "local",
        Some("h"),
        0,
        &[RUNNERS, VIEWER_GROUP],
    ),
    // Invited, never signed in, in no group.
    ("01K000000000000000000000M7", "local", None, 0, &[]),
];

async fn exec(conn: &mut SqliteConnection, sql: &str, binds: &[&str]) {
    let mut query = sqlx::query(sql);
    for bind in binds {
        query = query.bind(*bind);
    }
    query
        .execute(&mut *conn)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
}

/// A database migrated to just before [`M1`], with groups and members.
async fn seed_old_schema(db: &Path) {
    let url = format!("sqlite://{}?mode=rwc", db.display());
    let mut conn = SqliteConnection::connect(&url).await.unwrap();
    conn.ensure_migrations_table().await.unwrap();
    for migration in sqlx::migrate!("./migrations")
        .iter()
        .filter(|m| m.version < M1)
    {
        conn.apply(migration).await.unwrap();
    }
    let at = "2026-10-30T00:00:00+00:00";
    for (id, name, prefix) in [(OPS, "Ops", "OPS"), (EMPTY, "Empty", "EMPT")] {
        exec(
            &mut conn,
            "INSERT INTO workspace (id, name, slug, issue_prefix, created_at)
             VALUES ($1, $2, lower($2), $3, $4)",
            &[id, name, prefix, at],
        )
        .await;
    }
    for (id, key, permissions) in [
        (LEADS, "leads", r#"["run","manage_secrets"]"#),
        (RUNNERS, "runners", r#"["run"]"#),
    ] {
        exec(
            &mut conn,
            "INSERT INTO user_group (id, key, name, permissions_json) VALUES ($1, $2, $2, $3)",
            &[id, key, permissions],
        )
        .await;
    }
    for (workspace, group) in [(OPS, VIEWER_GROUP), (OPS, LEADS)] {
        exec(
            &mut conn,
            "INSERT INTO workspace_group (workspace_id, group_id) VALUES ($1, $2)",
            &[workspace, group],
        )
        .await;
    }
    for (n, (id, issuer, hash, is_owner, groups)) in MEMBERS.iter().enumerate() {
        let email = format!("m{n}@x.io");
        sqlx::query(
            "INSERT INTO member (id, issuer, subject, username, email, password_hash, is_owner)
             VALUES ($1, $2, $3, $3, $3, $4, $5)",
        )
        .bind(id)
        .bind(issuer)
        .bind(&email)
        .bind(hash)
        .bind(is_owner)
        .execute(&mut conn)
        .await
        .unwrap();
        for group in *groups {
            exec(
                &mut conn,
                "INSERT INTO member_group (member_id, group_id) VALUES ($1, $2)",
                &[id, group],
            )
            .await;
        }
    }
    conn.close().await.unwrap();
}

async fn has_table(db: &Path, table: &str) -> bool {
    let url = format!("sqlite://{}", db.display());
    let mut conn = SqliteConnection::connect(&url).await.unwrap();
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM sqlite_schema WHERE name = $1")
        .bind(table)
        .fetch_one(&mut conn)
        .await
        .unwrap();
    conn.close().await.unwrap();
    n > 0
}

#[tokio::test]
async fn groups_become_workspace_roles_on_a_copy_of_an_old_database() {
    let dir = tempfile::tempdir().unwrap();
    let old = dir.path().join("old");
    std::fs::create_dir_all(&old).unwrap();
    seed_old_schema(&old.join("sun.db")).await;
    // Upgrade a copy; the original stays as it was.
    let data = dir.path().join("copy");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::copy(old.join("sun.db"), data.join("sun.db")).unwrap();

    let store = Store::open(&data).await.unwrap();
    let mut tx = store.read().await.unwrap();
    let main = WorkspaceId::MAIN;
    let ops: WorkspaceId = OPS.parse().unwrap();
    let empty: WorkspaceId = EMPTY.parse().unwrap();
    let id = |n: usize| -> MemberId { MEMBERS[n].0.parse().unwrap() };
    let mut got = Vec::new();
    for n in 0..MEMBERS.len() {
        let mut roles = Vec::new();
        for workspace in [main, ops, empty] {
            roles.push(tx.role_of(workspace, id(n)).await.unwrap());
        }
        got.push(roles);
    }
    use Role::*;
    let want = [
        [Some(Owner), Some(Owner), Some(Owner)],
        [Some(Owner), Some(Owner), Some(Owner)],
        [Some(Viewer), Some(Viewer), None],
        [Some(Admin), Some(Admin), Some(Admin)],
        [Some(Admin), Some(Admin), None],
        [Some(Member), Some(Viewer), None],
        [Some(Viewer), None, None],
    ];
    assert_eq!(got, want);
    assert_eq!(
        tx.workspaces_of_member(id(2)).await.unwrap(),
        [main, ops],
        "workspaces_of_member reads the memberships"
    );
    // Verified: a password, an identity provider or the token; not the
    // pending invite.
    for n in 0..MEMBERS.len() {
        let verified = tx.get_member(id(n)).await.unwrap().email_verified_at;
        assert_eq!(verified.is_some(), n != 6, "member {n}");
    }
    for workspace in [main, ops, empty] {
        assert_eq!(tx.get_workspace(workspace).await.unwrap().created_by, None);
    }
    drop(tx);
    drop(store);

    let upgraded = data.join("sun.db");
    for table in ["user_group", "member_group", "workspace_group"] {
        assert!(!has_table(&upgraded, table).await, "{table} dropped");
        assert!(has_table(&old.join("sun.db"), table).await, "original kept");
    }
    assert!(!has_table(&old.join("sun.db"), "workspace_member").await);
    // The backup taken before migrating is the old schema: restoring it is
    // the rollback.
    let backups: Vec<_> = std::fs::read_dir(data.join("backups"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(backups.len(), 1);
    assert!(has_table(&backups[0], "member_group").await);
    assert!(!has_table(&backups[0], "workspace_member").await);
    // A second open changes nothing and takes no new backup.
    drop(Store::open(&data).await.unwrap());
    assert_eq!(std::fs::read_dir(data.join("backups")).unwrap().count(), 1);
}

fn invalid<T: std::fmt::Debug>(result: Result<T, StoreError>) -> bool {
    matches!(result, Err(StoreError::Core(Error::InvalidInput(_))))
}

fn not_found<T: std::fmt::Debug>(result: Result<T, StoreError>) -> bool {
    matches!(result, Err(StoreError::Core(Error::NotFound(_))))
}

#[tokio::test]
async fn memberships_round_trip_and_a_workspace_keeps_an_owner() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let mut tx = store.begin().await.unwrap();
    let ann = tx.create_unverified("ann@x.io").await.unwrap();
    let bob = tx.create_unverified("bob@x.io").await.unwrap();
    let main = WorkspaceId::MAIN;

    let joined = tx.add_member(main, ann.id, Role::Owner).await.unwrap();
    assert_eq!((joined.member_id, joined.role), (ann.id, Role::Owner));
    tx.add_member(main, bob.id, Role::Member).await.unwrap();
    assert!(invalid(tx.add_member(main, bob.id, Role::Viewer).await));
    assert!(not_found(
        tx.add_member(WorkspaceId::generate(), bob.id, Role::Viewer)
            .await
    ));
    assert!(not_found(
        tx.add_member(main, MemberId::generate(), Role::Viewer)
            .await
    ));
    let members: Vec<(MemberId, Role)> = tx
        .members_of(main)
        .await
        .unwrap()
        .into_iter()
        .map(|m| (m.member_id, m.role))
        .collect();
    assert_eq!(members, [(ann.id, Role::Owner), (bob.id, Role::Member)]);
    let mine = tx.memberships_of(bob.id).await.unwrap();
    assert_eq!(mine.len(), 1);
    assert_eq!((mine[0].workspace_id, mine[0].role), (main, Role::Member));
    assert_eq!(tx.owner_count(main).await.unwrap(), 1);

    // The last owner can't step down or leave.
    assert!(invalid(tx.set_role(main, ann.id, Role::Admin).await));
    assert!(invalid(tx.remove_member(main, ann.id).await));
    tx.set_role(main, ann.id, Role::Owner).await.unwrap();
    tx.set_role(main, bob.id, Role::Owner).await.unwrap();
    assert_eq!(tx.owner_count(main).await.unwrap(), 2);
    tx.set_role(main, ann.id, Role::Viewer).await.unwrap();
    assert!(tx.remove_member(main, ann.id).await.unwrap());
    assert!(!tx.remove_member(main, ann.id).await.unwrap());
    assert!(not_found(tx.set_role(main, ann.id, Role::Viewer).await));
    assert_eq!(tx.role_of(main, ann.id).await.unwrap(), None);
    assert_eq!(tx.role_of(main, bob.id).await.unwrap(), Some(Role::Owner));
    tx.commit().await.unwrap();
}

fn invitation(email: &str, role: Role, expires_in: chrono::Duration) -> WorkspaceInvitation {
    let now = chrono::Utc::now();
    WorkspaceInvitation {
        id: InvitationId::generate(),
        workspace_id: WorkspaceId::MAIN,
        email: email.into(),
        role,
        inviter_id: None,
        created_at: now,
        expires_at: now + expires_in,
        accepted_at: None,
        declined_at: None,
    }
}

#[tokio::test]
async fn invitations_replace_accept_decline_and_expire_once() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let mut tx = store.begin().await.unwrap();
    let hour = chrono::Duration::hours(1);
    let first = invitation("Ann@X.io", Role::Member, hour);
    tx.insert_invitation(&first, "t1").await.unwrap();
    let stored = tx.invitation_by_token_hash("t1").await.unwrap().unwrap();
    assert_eq!((stored.id, stored.email.as_str()), (first.id, "ann@x.io"));
    assert_eq!(
        tx.get_invitation(first.id).await.unwrap().role,
        Role::Member
    );
    assert!(not_found(tx.get_invitation(InvitationId::generate()).await));

    // Inviting the same email again replaces the pending one.
    let again = invitation("ann@x.io", Role::Admin, hour);
    tx.insert_invitation(&again, "t2").await.unwrap();
    assert_eq!(tx.invitation_by_token_hash("t1").await.unwrap(), None);
    let pending = tx.invitations(WorkspaceId::MAIN).await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!((pending[0].id, pending[0].role), (again.id, Role::Admin));
    assert_eq!(tx.invitations_by_email("ANN@x.io").await.unwrap(), pending);

    // Accepted once; then nothing else applies to it.
    assert!(tx.accept_invitation(again.id).await.unwrap());
    assert!(!tx.accept_invitation(again.id).await.unwrap());
    assert!(!tx.decline_invitation(again.id).await.unwrap());
    assert!(!tx.expire_invitation(again.id).await.unwrap());
    let accepted = tx.get_invitation(again.id).await.unwrap();
    assert!(accepted.accepted_at.is_some() && accepted.declined_at.is_none());
    assert!(tx.invitations(WorkspaceId::MAIN).await.unwrap().is_empty());
    // An accepted invitation doesn't block a new one.
    let third = invitation("ann@x.io", Role::Viewer, hour);
    tx.insert_invitation(&third, "t3").await.unwrap();
    assert!(tx.decline_invitation(third.id).await.unwrap());
    assert!(!tx.accept_invitation(third.id).await.unwrap());

    // Expired ones are not pending and can't be accepted; revoking ends one.
    let old = invitation("old@x.io", Role::Viewer, -hour);
    tx.insert_invitation(&old, "t4").await.unwrap();
    assert!(!tx.accept_invitation(old.id).await.unwrap());
    let live = invitation("live@x.io", Role::Viewer, hour);
    tx.insert_invitation(&live, "t5").await.unwrap();
    assert_eq!(tx.invitations(WorkspaceId::MAIN).await.unwrap().len(), 1);
    assert!(tx.expire_invitation(live.id).await.unwrap());
    assert!(tx.invitations(WorkspaceId::MAIN).await.unwrap().is_empty());
    assert!(!tx.accept_invitation(live.id).await.unwrap());
    // A token is unique.
    let clash = invitation("x@x.io", Role::Viewer, hour);
    assert!(tx.insert_invitation(&clash, "t5").await.is_err());
    tx.commit().await.unwrap();
}
