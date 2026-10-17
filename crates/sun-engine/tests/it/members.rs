//! Multi-tenant members: sign-up (on, off, domain allowlist, the pending
//! invitation override), email verification and the first verified person
//! becoming the deployment owner, invitations into a workspace (accept,
//! reuse, expiry, decline, revoke), the last-owner guard, and creating a
//! workspace with its owner in one transaction.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use sun_core::model::{Role, SignupPolicy};
use sun_core::{Error, HostId, MemberId, WorkspaceId};
use sun_engine::{Engine, EngineConfig, WatchdogConfig};
use sun_store::{MembershipRepo, Store};

use crate::queue::{NoHost, NoMods};

async fn engine(dir: &std::path::Path) -> (Store, Engine) {
    let store = Store::open(dir).await.unwrap();
    let config = EngineConfig {
        allow_bypass: false,
        host: HostId::generate(),
        adapter: "fake".into(),
        delivery_timeout: Duration::from_secs(5),
        watchdog: WatchdogConfig::default(),
    };
    let (engine, _task) = Engine::start(store.clone(), Arc::new(NoHost), Arc::new(NoMods), config)
        .await
        .unwrap();
    (store, engine)
}

fn later() -> sun_core::Timestamp {
    chrono::Utc::now() + chrono::Duration::minutes(15)
}

/// Signs `email` up with code hash `code` and signs in with it: verified.
async fn sign_up_and_verify(engine: &Engine, email: &str, code: &str) -> MemberId {
    let open = SignupPolicy::default();
    let member = engine
        .signup(email, &open, code, later())
        .await
        .unwrap()
        .expect("a new account");
    assert!(member.email_verified_at.is_none());
    let signed_in = engine
        .sign_in_with_code(member.id, code, &format!("session-{code}"), later())
        .await
        .unwrap()
        .expect("the code works");
    assert!(signed_in.email_verified_at.is_some() && signed_in.must_set_password);
    member.id
}

fn forbidden<T: std::fmt::Debug>(result: sun_core::Result<T>) -> bool {
    matches!(result, Err(Error::Forbidden(_)))
}

fn invalid<T: std::fmt::Debug>(result: sun_core::Result<T>) -> bool {
    matches!(result, Err(Error::InvalidInput(_)))
}

#[tokio::test]
async fn signup_verifies_by_code_and_the_first_verified_person_owns_the_deployment() {
    let dir = tempfile::tempdir().unwrap();
    let (_store, engine) = engine(dir.path()).await;
    // The token holder exists first, still on the placeholder email.
    engine.seed_owner("root", "token-hash").await.unwrap();

    let ann = sign_up_and_verify(&engine, " Ann@Acme.io ", "c1").await;
    let access = engine.member_access(ann).await.unwrap();
    assert!(access.member.is_owner, "first verified person");
    assert_eq!(access.role_in(WorkspaceId::MAIN), Some(Role::Owner));
    assert_eq!(access.member.email.as_deref(), Some("ann@acme.io"));

    let bob = sign_up_and_verify(&engine, "bob@acme.io", "c2").await;
    let access = engine.member_access(bob).await.unwrap();
    assert!(!access.member.is_owner && access.memberships.is_empty());
    assert_eq!(access.workspaces, Some(Vec::new()), "sees no workspace yet");

    // Signing up again: a verified account writes nothing; an unverified
    // one gets a new code (the old one stops working).
    let open = SignupPolicy::default();
    assert!(
        engine
            .signup("bob@acme.io", &open, "c3", later())
            .await
            .unwrap()
            .is_none()
    );
    let cat = engine
        .signup("cat@acme.io", &open, "c4", later())
        .await
        .unwrap()
        .unwrap();
    let again = engine
        .signup("cat@acme.io", &open, "c5", later())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(again.id, cat.id);
    assert!(
        engine
            .sign_in_with_code(cat.id, "c4", "s4", later())
            .await
            .unwrap()
            .is_none()
    );
    assert!(invalid(
        engine.signup("not-an-email", &open, "c6", later()).await
    ));
}

#[tokio::test]
async fn an_owner_with_a_real_email_blocks_the_first_person_promotion() {
    let dir = tempfile::tempdir().unwrap();
    let (_store, engine) = engine(dir.path()).await;
    engine.seed_owner("root", "token-hash").await.unwrap();
    engine
        .set_owner_email("root", "root@acme.io")
        .await
        .unwrap();
    let ann = sign_up_and_verify(&engine, "ann@acme.io", "c1").await;
    assert!(!engine.member(ann).await.unwrap().is_owner);
    // `verify_email` is the same rule, and idempotent.
    let verified = engine.verify_email(ann).await.unwrap();
    assert!(!verified.is_owner && verified.email_verified_at.is_some());
}

#[tokio::test]
async fn signup_off_and_domain_allowlist_yield_to_a_pending_invitation() {
    let dir = tempfile::tempdir().unwrap();
    let (_store, engine) = engine(dir.path()).await;
    let closed = SignupPolicy {
        allow_signup: false,
        ..SignupPolicy::default()
    };
    assert!(forbidden(
        engine.signup("eve@else.io", &closed, "c1", later()).await
    ));
    let acme_only = SignupPolicy {
        allowed_email_domains: vec!["acme.io".into()],
        ..SignupPolicy::default()
    };
    assert!(forbidden(
        engine
            .signup("eve@else.io", &acme_only, "c1", later())
            .await
    ));
    assert!(
        engine
            .signup("ann@acme.io", &acme_only, "c2", later())
            .await
            .unwrap()
            .is_some()
    );
    assert!(forbidden(
        engine.signup("ann2@acme.io", &closed, "c3", later()).await
    ));

    // A pending invitation admits whatever the policy says.
    engine
        .invite(
            WorkspaceId::MAIN,
            "eve@else.io",
            Role::Viewer,
            None,
            "t1",
            later(),
            "user:root",
        )
        .await
        .unwrap();
    for policy in [&closed, &acme_only] {
        assert!(
            engine
                .signup("eve@else.io", policy, "c4", later())
                .await
                .unwrap()
                .is_some()
        );
    }
}

#[tokio::test]
async fn invitations_accept_once_for_the_invited_email_and_expire() {
    let dir = tempfile::tempdir().unwrap();
    let (store, engine) = engine(dir.path()).await;
    let open = SignupPolicy::default();
    let ann = sign_up_and_verify(&engine, "ann@acme.io", "c1").await;
    let ops = engine
        .create_workspace("Ops", None, Some(ann), &open, "user:ann")
        .await
        .unwrap()
        .id;
    let bob = sign_up_and_verify(&engine, "bob@acme.io", "c2").await;
    let eve = sign_up_and_verify(&engine, "eve@acme.io", "c3").await;

    let invite = |email: &'static str, role: Role, token: &'static str| {
        let engine = engine.clone();
        async move {
            engine
                .invite(ops, email, role, Some(ann), token, later(), "user:ann")
                .await
        }
    };
    let invitation = invite("BOB@acme.io", Role::Admin, "t1").await.unwrap();
    assert_eq!(invitation.email, "bob@acme.io");
    assert_eq!(engine.invitations(ops).await.unwrap().len(), 1);
    assert_eq!(engine.my_invitations(bob).await.unwrap().len(), 1);
    assert_eq!(
        engine.invitation_by_token("t1").await.unwrap().unwrap().id,
        invitation.id
    );
    // Only the invited email takes it.
    assert!(forbidden(
        engine
            .accept_invitation(invitation.id, eve, &crate::support::unlimited())
            .await
    ));
    assert!(forbidden(
        engine.decline_invitation(invitation.id, eve).await
    ));
    let joined = engine
        .accept_invitation(invitation.id, bob, &crate::support::unlimited())
        .await
        .unwrap();
    assert_eq!((joined.workspace_id, joined.role), (ops, Role::Admin));
    assert!(engine.member_access(bob).await.unwrap().sees(ops));
    // Used once: no reuse, no decline afterwards, no re-invite of a member.
    assert!(invalid(
        engine
            .accept_invitation(invitation.id, bob, &crate::support::unlimited())
            .await
    ));
    assert!(invalid(engine.decline_invitation(invitation.id, bob).await));
    assert!(invalid(invite("bob@acme.io", Role::Viewer, "t2").await));
    assert!(engine.invitations(ops).await.unwrap().is_empty());

    // Expired: not pending, can't be accepted.
    let old = engine
        .invite(
            ops,
            "eve@acme.io",
            Role::Member,
            Some(ann),
            "t3",
            chrono::Utc::now() - chrono::Duration::seconds(1),
            "user:ann",
        )
        .await
        .unwrap();
    assert!(engine.my_invitations(eve).await.unwrap().is_empty());
    assert!(invalid(
        engine
            .accept_invitation(old.id, eve, &crate::support::unlimited())
            .await
    ));
    // Declined and revoked ones are done too.
    let declined = invite("eve@acme.io", Role::Member, "t4").await.unwrap();
    engine.decline_invitation(declined.id, eve).await.unwrap();
    assert!(invalid(
        engine
            .accept_invitation(declined.id, eve, &crate::support::unlimited())
            .await
    ));
    let revoked = invite("eve@acme.io", Role::Member, "t5").await.unwrap();
    engine
        .revoke_invitation(revoked.id, "user:ann")
        .await
        .unwrap();
    assert!(invalid(
        engine.revoke_invitation(revoked.id, "user:ann").await
    ));
    assert!(invalid(
        engine
            .accept_invitation(revoked.id, eve, &crate::support::unlimited())
            .await
    ));
    assert_eq!(engine.member_access(eve).await.unwrap().role_in(ops), None);

    // An unverified account can't accept, even with the right email.
    let fay = engine
        .signup("fay@acme.io", &open, "c4", later())
        .await
        .unwrap()
        .unwrap();
    let for_fay = invite("fay@acme.io", Role::Viewer, "t6").await.unwrap();
    assert!(forbidden(
        engine
            .accept_invitation(for_fay.id, fay.id, &crate::support::unlimited())
            .await
    ));

    // Joined some other way while the invitation was pending: accepting
    // keeps a higher role, and raises a lower one.
    let gus = sign_up_and_verify(&engine, "gus@acme.io", "c5").await;
    let low = invite("gus@acme.io", Role::Viewer, "t7").await.unwrap();
    let high = engine
        .invite(
            WorkspaceId::MAIN,
            "gus@acme.io",
            Role::Admin,
            None,
            "t8",
            later(),
            "user:ann",
        )
        .await
        .unwrap();
    let mut tx = store.begin().await.unwrap();
    tx.add_member(ops, gus, Role::Member).await.unwrap();
    tx.add_member(WorkspaceId::MAIN, gus, Role::Member)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        engine
            .accept_invitation(low.id, gus, &crate::support::unlimited())
            .await
            .unwrap()
            .role,
        Role::Member
    );
    assert_eq!(
        engine
            .accept_invitation(high.id, gus, &crate::support::unlimited())
            .await
            .unwrap()
            .role,
        Role::Admin
    );
}

#[tokio::test]
async fn a_workspace_keeps_its_last_owner() {
    let dir = tempfile::tempdir().unwrap();
    let (_store, engine) = engine(dir.path()).await;
    let open = SignupPolicy::default();
    let ann = sign_up_and_verify(&engine, "ann@acme.io", "c1").await;
    let bob = sign_up_and_verify(&engine, "bob@acme.io", "c2").await;
    let ops = engine
        .create_workspace("Ops", None, Some(bob), &open, "user:bob")
        .await
        .unwrap()
        .id;
    assert!(invalid(
        engine.set_role(ops, bob, Role::Admin, "user:bob").await
    ));
    assert!(invalid(engine.remove_member(ops, bob, "user:ann").await));
    assert!(invalid(engine.leave(ops, bob).await));
    assert!(matches!(
        engine.set_role(ops, ann, Role::Admin, "user:bob").await,
        Err(Error::NotFound(_))
    ));
    let invitation = engine
        .invite(
            ops,
            "ann@acme.io",
            Role::Owner,
            Some(bob),
            "t1",
            later(),
            "user:bob",
        )
        .await
        .unwrap();
    engine
        .accept_invitation(invitation.id, ann, &crate::support::unlimited())
        .await
        .unwrap();
    engine.leave(ops, bob).await.unwrap();
    let members = engine.workspace_members(ops).await.unwrap();
    assert_eq!(members.len(), 1);
    assert_eq!((members[0].member_id, members[0].role), (ann, Role::Owner));
    assert!(matches!(
        engine.remove_member(ops, bob, "user:ann").await,
        Err(Error::NotFound(_))
    ));
}

#[tokio::test]
async fn creating_a_workspace_makes_its_creator_owner_in_one_transaction() {
    let dir = tempfile::tempdir().unwrap();
    let (_store, engine) = engine(dir.path()).await;
    let open = SignupPolicy::default();
    let ann = sign_up_and_verify(&engine, "ann@acme.io", "c1").await; // deployment owner
    let bob = sign_up_and_verify(&engine, "bob@acme.io", "c2").await;

    let ops = engine
        .create_workspace("Ops", None, Some(bob), &open, "user:bob")
        .await
        .unwrap();
    assert_eq!(ops.created_by, Some(bob));
    let access = engine.member_access(bob).await.unwrap();
    assert_eq!(access.role_in(ops.id), Some(Role::Owner));
    assert_eq!(
        access.principal_in(ops.id).roles,
        ["viewer", "member", "admin", "owner"]
    );
    assert!(access.principal_in(WorkspaceId::MAIN).roles.is_empty());

    // A failed create writes nothing: no workspace, no membership.
    assert!(invalid(
        engine
            .create_workspace("ops", None, Some(bob), &open, "user:bob")
            .await
    ));
    assert_eq!(
        engine.member_access(bob).await.unwrap().memberships.len(),
        1
    );

    // Creation turned off: only deployment owners (and the token holder).
    let closed = SignupPolicy {
        disable_workspace_creation: true,
        ..SignupPolicy::default()
    };
    assert!(forbidden(
        engine
            .create_workspace("Bob's", None, Some(bob), &closed, "user:bob")
            .await
    ));
    let mine = engine
        .create_workspace("Ann's", None, Some(ann), &closed, "user:ann")
        .await
        .unwrap();
    assert_eq!(
        engine.workspace_members(mine.id).await.unwrap()[0].member_id,
        ann
    );
    let token = engine
        .create_workspace("Token's", None, None, &closed, "user:root")
        .await
        .unwrap();
    assert!(engine.workspace_members(token.id).await.unwrap().is_empty());
    assert_eq!(token.created_by, None);
}
