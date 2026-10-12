//! Projects: names unique per workspace ignoring case; they go with their workspace.
#![allow(clippy::unwrap_used)]

use sun_core::model::{Project, Workspace};
use sun_core::{Error, ProjectId, WorkspaceId};
use sun_store::{ProjectRepo, Store, StoreError, WorkspaceRepo};

fn project(workspace_id: WorkspaceId, name: &str) -> Project {
    Project {
        id: ProjectId::generate(),
        workspace_id,
        name: name.into(),
        description: String::new(),
        created_at: chrono::Utc::now(),
    }
}

fn invalid(result: Result<(), StoreError>) -> bool {
    matches!(result, Err(StoreError::Core(Error::InvalidInput(_))))
}

#[tokio::test]
async fn project_crud_with_names_unique_per_workspace() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let mut tx = store.begin().await.unwrap();
    let ops = Workspace {
        id: WorkspaceId::generate(),
        name: "Ops".into(),
        slug: "ops".into(),
        description: String::new(),
        context: String::new(),
        logo_hash: None,
        issue_prefix: "OPS".into(),
        issue_counter: 0,
        default_workflow_id: None,
        git_settings: Default::default(),
        created_at: chrono::Utc::now(),
    };
    tx.insert_workspace(&ops).await.unwrap();

    let web = project(WorkspaceId::MAIN, "Web");
    tx.insert_project(&web).await.unwrap();
    let api = project(WorkspaceId::MAIN, "API");
    tx.insert_project(&api).await.unwrap();
    assert!(invalid(
        tx.insert_project(&project(WorkspaceId::MAIN, "web")).await
    ));
    // Another workspace may use the name.
    let ops_web = project(ops.id, "WEB");
    tx.insert_project(&ops_web).await.unwrap();

    assert_eq!(
        tx.projects_of_workspace(WorkspaceId::MAIN).await.unwrap(),
        vec![api.clone(), web.clone()]
    );
    let renamed = Project {
        name: "Api".into(),
        description: "The public API".into(),
        ..api.clone()
    };
    tx.update_project(&renamed).await.unwrap(); // its own name, other case
    assert_eq!(tx.get_project(api.id).await.unwrap(), renamed);
    let clash = Project {
        name: "WEB".into(),
        ..api.clone()
    };
    assert!(invalid(tx.update_project(&clash).await));

    tx.delete_project(web.id).await.unwrap();
    let missing =
        |r: Result<(), StoreError>| matches!(r, Err(StoreError::Core(Error::NotFound(_))));
    assert!(missing(tx.delete_project(web.id).await));
    // A deleted workspace takes its projects.
    tx.delete_workspace(ops.id).await.unwrap();
    assert!(missing(tx.get_project(ops_web.id).await.map(|_| ())));
}
