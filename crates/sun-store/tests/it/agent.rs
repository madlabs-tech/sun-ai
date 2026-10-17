//! Agents: names unique among active ones ignoring case, the trigger-kept
//! foreign key of `task.assignee_agent_id`, and system workflows left out
//! of the workflow lists.
#![allow(clippy::unwrap_used)]

use sun_core::model::{Agent, Task, Workflow, WorkflowOwner};
use sun_core::{AgentId, Error, WorkflowId, WorkspaceId};
use sun_store::{AgentRepo, Store, StoreError, TaskRepo, WorkflowRepo};

fn agent(name: &str) -> Agent {
    Agent {
        id: AgentId::generate(),
        workspace_id: WorkspaceId::MAIN,
        name: name.into(),
        description: String::new(),
        instructions: "Be brief.".into(),
        runtime_chain: vec!["claude-code".into(), "codex".into()],
        skill_ids: vec!["tdd".into()],
        permission_mode: None,
        created_at: chrono::Utc::now(),
        archived_at: None,
    }
}

#[tokio::test]
async fn agent_names_are_unique_while_active_and_tasks_name_real_agents() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let mut tx = store.begin().await.unwrap();
    let mut ada = agent("Ada");
    tx.insert_agent(&ada).await.unwrap();
    assert_eq!(tx.get_agent(ada.id).await.unwrap(), ada);
    let taken = tx.insert_agent(&agent("ADA")).await;
    assert!(matches!(
        taken,
        Err(StoreError::Core(Error::InvalidInput(_)))
    ));
    // Archived: off the list, and its name is free again.
    ada.archived_at = Some(chrono::Utc::now());
    tx.update_agent(&ada).await.unwrap();
    assert!(tx.agents(Some(WorkspaceId::MAIN)).await.unwrap().is_empty());
    let ada2 = agent("ada");
    tx.insert_agent(&ada2).await.unwrap();
    assert_eq!(
        tx.agents(Some(WorkspaceId::MAIN)).await.unwrap(),
        std::slice::from_ref(&ada2)
    );

    // A task names an agent that exists.
    let mut task = Task::new(WorkspaceId::MAIN, "x");
    task.assignee_agent_id = Some(AgentId::generate());
    assert!(tx.insert_task(&task).await.is_err());
    task.assignee_agent_id = Some(ada2.id);
    tx.insert_task(&task).await.unwrap();
    assert_eq!(
        tx.agent_task_counts().await.unwrap(),
        [(ada2.id, WorkspaceId::MAIN, 1)]
    );
    task.assignee_agent_id = Some(AgentId::generate());
    assert!(tx.update_task(&task).await.is_err());

    // A system workflow: found per (agent, workspace), never listed.
    let system = Workflow {
        id: WorkflowId::generate(),
        workspace_id: WorkspaceId::MAIN,
        name: "ada".into(),
    };
    tx.insert_system_workflow(&system, ada2.id).await.unwrap();
    let mine = Workflow {
        id: WorkflowId::generate(),
        ..system.clone()
    };
    tx.insert_workflow(&mine).await.unwrap();
    assert_eq!(
        tx.system_workflow(ada2.id, WorkspaceId::MAIN)
            .await
            .unwrap(),
        Some(system.id)
    );
    // A direct chat's workflow: hidden too, owned by no agent.
    let chat = Workflow {
        id: WorkflowId::generate(),
        ..system.clone()
    };
    tx.insert_direct_chat_workflow(&chat).await.unwrap();
    let agent = WorkflowOwner::Agent(ada2.id);
    assert_eq!(tx.workflow_owner(system.id).await.unwrap(), agent);
    assert_eq!(
        tx.workflow_owner(mine.id).await.unwrap(),
        WorkflowOwner::Person
    );
    assert_eq!(
        tx.workflow_owner(chat.id).await.unwrap(),
        WorkflowOwner::DirectChat
    );
    let missing = WorkflowId::generate();
    assert_eq!(
        tx.workflow_owner(missing).await.unwrap(),
        WorkflowOwner::Person
    );
    assert_eq!(tx.workflows().await.unwrap(), std::slice::from_ref(&mine));
    let listed = tx.workflows_of_workspace(WorkspaceId::MAIN).await.unwrap();
    assert_eq!(listed, [mine]);
    let mut hidden = vec![system, chat];
    hidden.sort_by_key(|w| w.id.to_string());
    assert_eq!(tx.hidden_workflows().await.unwrap(), hidden);
}

/// Item 9: `agent.permission_mode` is `None` (inherit) unless set, and
/// round-trips through insert, update and the list.
#[tokio::test]
async fn an_agents_permission_mode_is_none_until_set() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let mut tx = store.begin().await.unwrap();
    let mut ada = agent("Ada");
    tx.insert_agent(&ada).await.unwrap();
    assert_eq!(tx.get_agent(ada.id).await.unwrap().permission_mode, None);
    ada.permission_mode = Some(sun_core::PermissionMode::Plan);
    tx.update_agent(&ada).await.unwrap();
    assert_eq!(tx.get_agent(ada.id).await.unwrap(), ada);
    assert_eq!(tx.agents(None).await.unwrap(), [ada.clone()]);
    ada.permission_mode = None;
    tx.update_agent(&ada).await.unwrap();
    assert_eq!(tx.get_agent(ada.id).await.unwrap().permission_mode, None);
}
