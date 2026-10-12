//! Mirrored pull requests: the stale-delivery guard on upsert, the snapshot
//! head guard, task links and exclusions, and the PR-automation task moves.
#![allow(clippy::unwrap_used)]

use chrono::{Duration, Utc};
use serde_json::json;
use sun_core::model::{
    ActivityKind, PrSnapshot, PrState, PullRequest, Task, VcsProvider, Workflow,
};
use sun_core::{Error, PullRequestId, TaskStatus, WorkflowId, WorkspaceId};
use sun_store::{MemberRepo, PullRequestRepo, Store, StoreError, TaskRepo, WorkflowRepo};

fn pull_request(number: i64, title: &str, updated: chrono::DateTime<Utc>) -> PullRequest {
    PullRequest {
        id: PullRequestId::generate(),
        workspace_id: WorkspaceId::MAIN,
        provider: VcsProvider::Github,
        installation_id: Some(42),
        repo_owner: "hryer".into(),
        repo_name: "sun".into(),
        number,
        title: title.into(),
        state: PrState::Open,
        html_url: format!("https://github.com/hryer/sun/pull/{number}"),
        branch: Some(format!("sun/MAIN-{number}-x")),
        author_login: Some("hryer".into()),
        author_avatar_url: None,
        merged_at: None,
        closed_at: None,
        pr_created_at: updated - Duration::hours(1),
        pr_updated_at: updated,
        head_sha: "abc".into(),
        additions: 10,
        deletions: 2,
        changed_files: 3,
        mergeable: None,
        merge_state_status: None,
        checks_rollup_state: None,
        checks_total: 0,
        checks_passed: 0,
        checks_failed: 0,
        checks_pending: 0,
        snapshot_head_sha: String::new(),
        snapshot_fetched_at: None,
    }
}

#[tokio::test]
async fn pull_requests_upsert_snapshot_link_and_exclude() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let mut tx = store.begin().await.unwrap();
    let workflow = Workflow {
        id: WorkflowId::generate(),
        workspace_id: WorkspaceId::MAIN,
        name: "w".into(),
    };
    tx.insert_workflow(&workflow).await.unwrap();
    let task = |title: &str| Task {
        workflow_id: Some(workflow.id),
        ..Task::new(WorkspaceId::MAIN, title)
    };
    let (a, b) = (task("a"), task("b"));
    tx.insert_task(&a).await.unwrap();
    tx.insert_task(&b).await.unwrap();
    let member = tx
        .upsert_member("local", "ana@x.io", "ana", Some("ana@x.io"), false)
        .await
        .unwrap();

    // Upsert: a newer delivery updates the row (keeping its id), an older
    // one is stale and changes nothing.
    let now = Utc::now();
    let first = pull_request(12, "MAIN-1 first", now);
    let stored = tx.upsert_pull_request(&first).await.unwrap().unwrap();
    assert_eq!(stored, first);
    let stale = pull_request(12, "older", now - Duration::minutes(5));
    assert_eq!(tx.upsert_pull_request(&stale).await.unwrap(), None);
    assert_eq!(
        tx.get_pull_request(first.id).await.unwrap().title,
        first.title
    );
    let newer = PullRequest {
        state: PrState::Merged,
        merged_at: Some(now + Duration::minutes(1)),
        head_sha: "def".into(),
        ..pull_request(12, "MAIN-1 merged", now + Duration::minutes(1))
    };
    let stored = tx.upsert_pull_request(&newer).await.unwrap().unwrap();
    assert_eq!(stored.id, first.id, "the existing row keeps its id");
    assert_eq!(
        (stored.state, stored.head_sha.as_str()),
        (PrState::Merged, "def")
    );
    assert!(matches!(
        tx.get_pull_request(newer.id).await,
        Err(StoreError::Core(Error::NotFound(_)))
    ));
    assert_eq!(
        tx.pull_request_by_url(WorkspaceId::MAIN, &first.html_url)
            .await
            .unwrap()
            .map(|p| p.id),
        Some(first.id)
    );
    // A pasted URL with the owner in another case still finds it.
    assert_eq!(
        tx.pull_request_by_url(WorkspaceId::MAIN, "https://github.com/Hryer/Sun/pull/12")
            .await
            .unwrap()
            .map(|p| p.id),
        Some(first.id)
    );
    assert_eq!(
        tx.pull_request_by_url(WorkspaceId::MAIN, "https://github.com/hryer/sun/pull/999")
            .await
            .unwrap(),
        None
    );
    let by_head = tx
        .pull_requests_by_head(WorkspaceId::MAIN, "hryer", "sun", "def")
        .await
        .unwrap();
    assert_eq!(by_head.len(), 1);
    assert!(
        tx.pull_requests_by_head(WorkspaceId::MAIN, "hryer", "sun", "abc")
            .await
            .unwrap()
            .is_empty()
    );

    // Snapshot: stored for the current head only, and kept by later upserts.
    let snapshot = PrSnapshot {
        head_sha: "def".into(),
        mergeable: Some("MERGEABLE".into()),
        merge_state_status: Some("CLEAN".into()),
        checks_rollup_state: Some("SUCCESS".into()),
        checks_total: 3,
        checks_passed: 3,
        checks_failed: 0,
        checks_pending: 0,
    };
    assert!(tx.set_snapshot(first.id, &snapshot).await.unwrap());
    let moved = PrSnapshot {
        head_sha: "stale-head".into(),
        ..snapshot.clone()
    };
    assert!(!tx.set_snapshot(first.id, &moved).await.unwrap());
    let pr = tx.get_pull_request(first.id).await.unwrap();
    assert!(pr.snapshot_available());
    assert_eq!((pr.checks_total, pr.checks_passed), (3, 3));
    assert!(pr.snapshot_fetched_at.is_some());
    let touched = pull_request(12, "touched", now + Duration::minutes(2));
    let after = tx.upsert_pull_request(&touched).await.unwrap().unwrap();
    assert_eq!(
        after.snapshot_head_sha, "def",
        "upserts leave the snapshot alone"
    );
    assert_eq!(after.checks_rollup_state.as_deref(), Some("SUCCESS"));

    // Links: automatic (no member) and manual.
    assert!(tx.link(a.id, first.id, None).await.unwrap());
    assert!(!tx.link(a.id, first.id, None).await.unwrap());
    assert!(tx.link(b.id, first.id, Some(member.id)).await.unwrap());
    assert_eq!(tx.auto_linked_tasks(first.id).await.unwrap(), vec![a.id]);
    let links = tx.tasks_of_pull_request(first.id).await.unwrap();
    assert_eq!(
        links
            .iter()
            .map(|l| (l.task_id, l.linked_by))
            .collect::<Vec<_>>(),
        [(a.id, None), (b.id, Some(member.id))]
    );
    let of_a = tx.pull_requests_of_task(a.id).await.unwrap();
    assert_eq!(of_a.len(), 1);
    assert_eq!((of_a[0].0.task_id, of_a[0].1.id), (a.id, first.id));
    assert_eq!(of_a[0].1.title, "touched");
    assert!(tx.unlink(a.id, first.id).await.unwrap());
    assert!(!tx.unlink(a.id, first.id).await.unwrap());
    assert!(tx.pull_requests_of_task(a.id).await.unwrap().is_empty());

    // Exclusions.
    assert!(!tx.is_excluded(a.id, first.id).await.unwrap());
    tx.exclude(a.id, first.id, Some(member.id)).await.unwrap();
    tx.exclude(a.id, first.id, None).await.unwrap();
    assert!(tx.is_excluded(a.id, first.id).await.unwrap());
    assert!(tx.unexclude(a.id, first.id).await.unwrap());
    assert!(!tx.unexclude(a.id, first.id).await.unwrap());
    assert!(!tx.is_excluded(a.id, first.id).await.unwrap());

    // The activity log takes the new kind.
    let activity = tx
        .append_activity(
            a.id,
            ActivityKind::PullRequest,
            None,
            json!({"action": "linked", "number": 12, "source": "title"}),
        )
        .await
        .unwrap();
    assert_eq!(activity.kind, ActivityKind::PullRequest);

    // PR automation moves an open task straight to done or needs_you, once.
    assert_eq!(
        tx.set_task_status_by_merge(a.id, TaskStatus::Done)
            .await
            .unwrap(),
        Some(TaskStatus::Todo)
    );
    assert_eq!(tx.get_task(a.id).await.unwrap().status, TaskStatus::Done);
    assert_eq!(
        tx.set_task_status_by_merge(a.id, TaskStatus::NeedsYou)
            .await
            .unwrap(),
        None,
        "a done task stays done"
    );
    assert_eq!(
        tx.set_task_status_by_merge(b.id, TaskStatus::NeedsYou)
            .await
            .unwrap(),
        Some(TaskStatus::Todo)
    );
    assert_eq!(
        tx.set_task_status_by_merge(b.id, TaskStatus::Backlog)
            .await
            .unwrap(),
        None,
        "only done and needs_you are merge targets"
    );
    assert!(matches!(
        tx.set_task_status_by_merge(sun_core::TaskId::generate(), TaskStatus::Done)
            .await,
        Err(StoreError::Core(Error::NotFound(_)))
    ));
    tx.set_pr_auto_complete(b.id, true).await.unwrap();
    assert!(tx.get_task(b.id).await.unwrap().pr_auto_complete_disabled);
    tx.set_pr_auto_complete(b.id, false).await.unwrap();
    assert!(!tx.get_task(b.id).await.unwrap().pr_auto_complete_disabled);
    assert!(matches!(
        tx.set_pr_auto_complete(sun_core::TaskId::generate(), true)
            .await,
        Err(StoreError::Core(Error::NotFound(_)))
    ));
    tx.commit().await.unwrap();
}
