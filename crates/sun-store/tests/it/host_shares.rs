//! Shared hosts (`host_share`): a host attached through a share gives its
//! workspace the host only while the share holds, for the member who
//! attached it (`groups_of`, read per call); and a changed email is not
//! verified until proven, so it can't carry a domain group over.
#![allow(clippy::unwrap_used)]

use sun_core::group::MemberGroup;
use sun_core::model::Host;
use sun_core::{GroupId, HostId, MemberId, WorkspaceId};
use sun_store::{GroupRepo, HostRepo, MemberRepo, Store, Tx, WorkspaceItem, WorkspaceRepo};

async fn group(tx: &mut Tx, name: &str, domains: &[&str]) -> GroupId {
    let group = MemberGroup {
        id: GroupId::generate(),
        name: name.into(),
        description: String::new(),
        domains: domains.iter().map(|d| d.to_string()).collect(),
        created_by: None,
        created_at: chrono::Utc::now(),
    };
    tx.insert_group(&group).await.unwrap();
    group.id
}

/// A member with `email`, verified or not.
async fn member(tx: &mut Tx, email: &str, verified: bool) -> MemberId {
    let member = tx.invite_member(email, email, false).await.unwrap();
    if verified {
        tx.verify_email(member.id).await.unwrap();
    }
    member.id
}

async fn host(tx: &mut Tx, name: &str) -> HostId {
    let host = Host {
        id: HostId::generate(),
        name: name.into(),
        capabilities: serde_json::json!({}),
        enrolled_by: None,
    };
    tx.insert_host(&host).await.unwrap();
    host.id
}

#[tokio::test]
async fn a_changed_email_is_not_verified_until_proven() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let mut tx = store.begin().await.unwrap();
    let id = member(&mut tx, "old@corp.io", true).await;
    // The same address in another case is no change.
    tx.set_member_email(id, "OLD@corp.io").await.unwrap();
    assert!(tx.get_member(id).await.unwrap().email_verified_at.is_some());
    tx.set_member_email(id, "x@madlabs.tech").await.unwrap();
    let changed = tx.get_member(id).await.unwrap();
    assert_eq!(changed.email.as_deref(), Some("x@madlabs.tech"));
    assert!(changed.email_verified_at.is_none());
    let team = group(&mut tx, "Madlabs team", &["madlabs.tech"]).await;
    assert!(tx.groups_of(id).await.unwrap().is_empty());
    tx.verify_email(id).await.unwrap();
    let groups = tx.groups_of(id).await.unwrap();
    assert_eq!(groups.iter().map(|g| g.id).collect::<Vec<_>>(), [team]);
    tx.commit().await.unwrap();
}

#[tokio::test]
async fn a_link_through_a_share_lasts_while_the_share_holds() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let main = WorkspaceId::MAIN;
    let mut tx = store.begin().await.unwrap();
    let team = group(&mut tx, "Madlabs team", &["madlabs.tech"]).await;
    let ana = member(&mut tx, "ana@madlabs.tech", true).await;
    let box_ = host(&mut tx, "team-box").await;
    let item = WorkspaceItem::Host(box_);
    assert!(tx.hosts_shared_with(ana).await.unwrap().is_empty());
    assert!(tx.share_host(box_, team, None).await.unwrap());
    assert!(!tx.share_host(box_, team, None).await.unwrap(), "once");
    assert_eq!(tx.hosts_shared_with(ana).await.unwrap(), [box_]);
    assert_eq!(tx.host_shares(box_).await.unwrap()[0].group_id, team);

    let live = |hosts: Vec<Host>| hosts.iter().any(|h| h.id == box_);
    tx.attach_host(main, box_, Some(ana)).await.unwrap();
    assert!(live(tx.hosts_in(main).await.unwrap()));
    assert_eq!(tx.workspaces_of(item).await.unwrap(), [main]);

    // Unshared: no longer the workspace's, for placement, lists and the editor.
    assert!(tx.unshare_host(box_, team).await.unwrap());
    assert!(!tx.unshare_host(box_, team).await.unwrap());
    assert!(!live(tx.hosts_in(main).await.unwrap()));
    assert!(tx.workspaces_of(item).await.unwrap().is_empty());
    assert!(tx.host_runtimes(Some(main)).await.unwrap().is_empty());
    assert_eq!(
        tx.host_links(box_).await.unwrap(),
        [main],
        "dormant, still there"
    );
    // Shared again: the link holds again.
    tx.share_host(box_, team, None).await.unwrap();
    assert!(live(tx.hosts_in(main).await.unwrap()));

    // Ana leaving the group (her email changes) ends it too.
    tx.set_member_email(ana, "ana@gmail.com").await.unwrap();
    assert!(!live(tx.hosts_in(main).await.unwrap()));

    // Attached directly (an owner, its enroller), it stays whatever the
    // shares, and a later attach through a share doesn't make it lapse.
    tx.attach_host(main, box_, None).await.unwrap();
    assert!(live(tx.hosts_in(main).await.unwrap()));
    let bob = member(&mut tx, "bob@madlabs.tech", true).await;
    tx.attach_host(main, box_, Some(bob)).await.unwrap();
    tx.unshare_host(box_, team).await.unwrap();
    assert!(live(tx.hosts_in(main).await.unwrap()));
    assert!(tx.unlink(main, item).await.unwrap());
    tx.commit().await.unwrap();
}
