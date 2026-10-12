//! End to end: a value from the real secret store never reaches a
//! hand-off pack, a context pack, its session files or its stored blob.
#![allow(clippy::unwrap_used)]

use sun_context::*;
use sun_core::model::{MemoryItem, MemoryScope, MemoryStatus, Note, NoteKind, SecretScope};
use sun_core::*;
use sun_store::{SecretRepo, Store};

/// Shaped like no known token: only the known-value pass can catch it.
const VALUE: &str = "correct-horse-battery-staple-42";

#[tokio::test]
async fn stored_secret_never_appears_in_handoff_or_context_pack() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let mut tx = store.begin().await.unwrap();
    tx.set_secret(&SecretScope::Mod("jira".into()), "JIRA_TOKEN", VALUE)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let known = store.read().await.unwrap().known_secrets().await.unwrap();
    let known: Vec<&str> = known.iter().map(String::as_str).collect();

    // The secret leaks into every input a pack is built from.
    let leak = format!("use {VALUE} to log in");
    let step_key = StepKey::new("implement").unwrap();
    let brief = StepBrief {
        task_id: TaskId::generate(),
        title: "Sync Jira".into(),
        body: leak.clone(),
        step_key: step_key.clone(),
        goal: leak.clone(),
        constraints: vec![leak.clone()],
        gates: vec!["test".into()],
        acceptance: vec![leak.clone()],
        context_budget_tokens: None,
    };
    let events = vec![
        SunEvent::Plan {
            todos: vec![Todo {
                text: leak.clone(),
                status: TodoStatus::Pending,
            }],
        },
        SunEvent::Text {
            role: Role::Assistant,
            text: leak.clone(),
        },
        SunEvent::Error {
            message: leak.clone(),
        },
    ];
    let notes = vec![Note {
        id: NoteId::generate(),
        run_id: RunId::generate(),
        step_key: step_key.clone(),
        kind: NoteKind::Decision,
        body: leak.clone(),
        superseded_by: None,
    }];
    let handoff = build_handoff(HandoffInput {
        brief: &brief,
        attempt: 2,
        from: &HandoffFrom {
            runtime: "claude-code".into(),
            reason: HandoffReason::LimitHit,
            reset_at: None,
        },
        events: &events,
        notes: &notes,
        progress_md: &leak,
        git: None,
        gates: &[],
        transcript_ref: None,
        known_secrets: &known,
    });
    let memory = vec![MemoryItem {
        id: MemoryItemId::generate(),
        workspace_id: sun_core::WorkspaceId::MAIN,
        scope: MemoryScope::Ecosystem,
        scope_id: None,
        body: leak.clone(),
        target: None,
        reason: String::new(),
        source_run: None,
        status: MemoryStatus::Active,
        author: "sun".into(),
        reviewed_by: None,
        created_at: chrono::Utc::now(),
    }];
    let knowledge = vec![KnowledgeHit {
        citation: format!("Runbook {VALUE}"),
        text: leak.clone(),
        pointer: format!("{SOURCES_DIR}/01.md"),
        flags: Vec::new(),
    }];
    let ledger = LedgerView {
        handoff: Some(handoff.clone()),
        step_outputs: vec![StepOutput {
            step_key: StepKey::new("plan").unwrap(),
            summary: leak.clone(),
            pointer: "artifact:1".into(),
        }],
        workspace_context: leak.clone(),
        ..Default::default()
    };

    let mut pack = compile(&brief, &ledger, &memory, &knowledge, 1_000_000);
    assert!(pack.render_markdown().contains(VALUE), "inputs carry it");
    pack.redact(&known);

    let stored = store_pack(store.blobs(), StepId::generate(), &pack)
        .await
        .unwrap();
    let blob = String::from_utf8(store.blobs().get(&stored.pack_hash).await.unwrap()).unwrap();
    let files = pack.files(Some(&handoff));
    let mut outputs = vec![
        serde_json::to_string(&handoff).unwrap(),
        handoff.render_markdown(),
        pack.render_markdown(),
        pack.envelope(),
        serde_json::to_string(&pack.envelope_parts(&brief, &known)).unwrap(),
        pack.summary(),
        serde_json::to_string(&pack).unwrap(),
        serde_json::to_string(&stored).unwrap(),
        blob,
    ];
    outputs.extend(files.iter().map(|f| format!("{}\n{}", f.path, f.contents)));
    for output in &outputs {
        assert!(!output.contains(VALUE), "leaked in:\n{output}");
    }
    // Still the same pack, with the value masked.
    assert!(pack.render_markdown().contains("use [REDACTED] to log in"));
    assert!(
        files
            .iter()
            .any(|f| f.path == format!("{SOURCES_DIR}/01.md"))
    );
}
