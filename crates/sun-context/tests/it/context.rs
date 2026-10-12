//! sun-context contract tests: compiler, hand-off, files, storage, MCP server.
// clippy's allow-unwrap-in-tests covers `#[test]` fns, not the helpers here.
#![allow(clippy::unwrap_used)]

use std::sync::{Arc, Mutex};
use std::time::Instant;

use rmcp::model::CallToolRequestParams;
use rmcp::{ClientHandler, ServiceExt};
use sun_context::*;
use sun_core::model::{MemoryItem, MemoryScope, MemoryStatus, Note, NoteKind};
use sun_core::*;
use sun_store::{ContextRepo, Store};

fn brief(budget: Option<u32>) -> StepBrief {
    StepBrief {
        task_id: TaskId::generate(),
        title: "Add retry to webhook".into(),
        body: "Webhooks fail on timeouts.".into(),
        step_key: StepKey::new("implement").unwrap(),
        goal: "Retry failed webhook deliveries".into(),
        constraints: vec!["keep the queue in SQLite".into()],
        gates: vec!["test".into()],
        acceptance: vec!["3 retries with backoff".into()],
        context_budget_tokens: budget,
    }
}

fn note(kind: NoteKind, body: &str, superseded: bool) -> Note {
    Note {
        id: NoteId::generate(),
        run_id: RunId::generate(),
        step_key: StepKey::new("implement").unwrap(),
        kind,
        body: body.into(),
        superseded_by: superseded.then(NoteId::generate),
    }
}

fn handoff() -> HandoffPack {
    let brief = brief(None);
    let events = vec![
        SunEvent::Plan {
            todos: vec![Todo {
                text: "old plan".into(),
                status: TodoStatus::Pending,
            }],
        },
        SunEvent::Text {
            role: Role::Assistant,
            text: "x".repeat(20_000),
        },
        SunEvent::Error {
            message: "key sk-abcdefghijklmnopqrstuvwxyz leaked".into(),
        },
        SunEvent::ToolResult {
            ok: false,
            summary: "test webhook_retry failed: timeout".into(),
        },
        SunEvent::Plan {
            todos: vec![
                Todo {
                    text: "add retry to webhook".into(),
                    status: TodoStatus::Done,
                },
                Todo {
                    text: "tests".into(),
                    status: TodoStatus::Pending,
                },
            ],
        },
        SunEvent::Text {
            role: Role::Assistant,
            text: "now writing tests".into(),
        },
    ];
    let notes = [
        note(
            NoteKind::Decision,
            "keep the queue in SQLite, no Redis",
            false,
        ),
        note(NoteKind::Decision, "use Redis", true),
        note(NoteKind::Todo, "tests", false),
        note(NoteKind::Todo, "update docs", false),
    ];
    let from = HandoffFrom {
        runtime: "claude-code".into(),
        reason: HandoffReason::LimitHit,
        reset_at: None,
    };
    let git = GitState {
        branch: "sun/01J".into(),
        commits: vec!["a1b2 add retry".into()],
        diff_stat: "3 files +120 -8".into(),
        uncommitted: true,
    };
    let gates = [GateResult {
        name: "test".into(),
        pass: false,
    }];
    build_handoff(HandoffInput {
        brief: &brief,
        attempt: 2,
        from: &from,
        events: &events,
        notes: &notes,
        progress_md: "retry done; tests next",
        git: Some(&git),
        gates: &gates,
        transcript_ref: Some("blob:9f3c"),
        known_secrets: &[],
    })
}

#[test]
fn handoff_pack_is_built_from_the_ledger() {
    let pack = handoff();
    let plan: Vec<(&str, TodoStatus)> = pack
        .plan
        .iter()
        .map(|p| (p.todo.as_str(), p.status))
        .collect();
    assert_eq!(
        plan,
        [
            ("add retry to webhook", TodoStatus::Done),
            ("tests", TodoStatus::Pending),
            ("update docs", TodoStatus::Pending),
        ]
    );
    assert_eq!(pack.decisions, ["keep the queue in SQLite, no Redis"]);
    assert_eq!(pack.last_errors.len(), 2);
    assert!(pack.last_errors[0].contains("[REDACTED]"));
    assert!(!pack.last_errors[0].contains("abcdefghij"));
    assert!(pack.transcript_tail.len() <= 12_000);
    assert!(
        pack.transcript_tail
            .ends_with("assistant: now writing tests\n\n")
    );

    // The fixed JSON shape of the pack.
    let json = serde_json::to_value(&pack).unwrap();
    let mut keys: Vec<&str> = json
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "attempt",
            "decisions",
            "from",
            "gates",
            "git",
            "last_errors",
            "plan",
            "progress_md",
            "step",
            "task",
            "transcript_ref",
            "transcript_tail"
        ]
    );
    assert_eq!(json["from"]["reason"], "limit_hit");
    assert_eq!(
        json["plan"][0],
        serde_json::json!({"todo": "add retry to webhook", "status": "done"})
    );
    assert_eq!(json["git"]["diff_stat"], "3 files +120 -8");
    assert_eq!(
        json["gates"][0],
        serde_json::json!({"name": "test", "pass": false})
    );
    assert_eq!(serde_json::from_value::<HandoffPack>(json).unwrap(), pack);
}

fn memory(scope: MemoryScope, status: MemoryStatus, body: &str) -> MemoryItem {
    MemoryItem {
        id: MemoryItemId::generate(),
        workspace_id: sun_core::WorkspaceId::MAIN,
        scope,
        scope_id: None,
        body: body.into(),
        target: None,
        reason: String::new(),
        source_run: None,
        status,
        author: "sun".into(),
        reviewed_by: None,
        created_at: chrono::Utc::now(),
    }
}

fn hits(n: usize, size: usize) -> Vec<KnowledgeHit> {
    (0..n)
        .map(|i| KnowledgeHit {
            citation: format!("Webhook guide v{i}"),
            text: "k".repeat(size),
            pointer: format!("doc:{i}"),
            flags: Vec::new(),
        })
        .collect()
}

#[test]
fn compile_fills_by_priority_and_trims_to_pointers() {
    let ledger = LedgerView {
        handoff: Some(handoff()),
        step_outputs: vec![StepOutput {
            step_key: StepKey::new("plan").unwrap(),
            summary: "plan: retry 3x".into(),
            pointer: "artifact:01J".into(),
        }],
        ..Default::default()
    };
    let memories = [
        memory(
            MemoryScope::Ecosystem,
            MemoryStatus::Active,
            "ecosystem rule",
        ),
        memory(MemoryScope::Workflow, MemoryStatus::Active, "workflow rule"),
        memory(MemoryScope::Repo, MemoryStatus::Proposed, "not reviewed"),
    ];
    // Window 100k → 10k share; the step asks for 5k, so B = 5k.
    let pack = compile(
        &brief(Some(5_000)),
        &ledger,
        &memories,
        &hits(10, 4_000),
        100_000,
    );
    assert_eq!(pack.budget_tokens, 5_000);

    let labels: Vec<(&str, bool)> = pack
        .items
        .iter()
        .map(|i| (i.label.as_str(), i.included))
        .collect();
    assert_eq!(labels[0], ("Task: Add retry to webhook", true));
    assert_eq!(labels[1], ("Hand-off", true));
    // The 3k-token transcript tail is P1, so it is filled before any knowledge.
    assert_eq!(labels[2], ("Transcript tail", true));
    assert_eq!(labels[3], ("Output of step `plan`", true));
    assert_eq!(
        labels[4],
        ("Memory (workflow)", true),
        "most specific scope first"
    );
    assert_eq!(labels[5], ("Memory (ecosystem)", true));
    assert!(
        !labels.iter().any(|(l, _)| l.contains("repo")),
        "only active memory"
    );
    let sources: Vec<bool> = pack
        .items
        .iter()
        .filter(|i| i.priority == 4)
        .map(|i| i.included)
        .collect();
    assert_eq!(sources.len(), 10);
    assert!(
        sources.iter().filter(|s| !**s).count() >= 8,
        "most sources become pointers"
    );
    assert!(
        pack.token_estimate <= pack.budget_tokens + 200,
        "{}",
        pack.token_estimate
    );

    let md = pack.render_markdown();
    assert!(md.contains("## Pointers"));
    assert!(md.contains("`.sun/context/sources/10.md`"));
    assert!(md.contains("Gates the step must pass:\n- test"));
    assert!(
        pack.summary().ends_with("trimmed (pointers)"),
        "{}",
        pack.summary()
    );

    let envelope = pack.envelope();
    assert!(envelope.starts_with("**Add retry to webhook**"));
    assert!(envelope.contains("Continuing `implement` (attempt 2)"));
    assert!(envelope.contains("Read `.sun/context/pack.md`"));
    assert!(!envelope.contains("kkkk"));

    // The same facts, split for the per-CLI templates.
    let parts = pack.envelope_parts(&brief(Some(5_000)), &[]);
    assert!(parts.task.starts_with("**Add retry to webhook**"));
    assert!(!parts.task.contains("Gates the step must pass"));
    assert_eq!(parts.gates, ["test"]);
    assert!(parts.context[0].starts_with("`.sun/context/pack.md`"));
    assert!(
        parts
            .handoff
            .unwrap()
            .contains("Continuing `implement` (attempt 2)")
    );

    // Pure: same input, same pack.
    let again = compile(
        &brief(Some(5_000)),
        &ledger,
        &memories,
        &hits(10, 4_000),
        100_000,
    );
    assert_eq!(again, pack);
}

#[test]
fn budget_is_capped_at_ten_percent_of_the_window_and_p0_always_fits() {
    let mut step = brief(Some(50_000));
    step.body = "b".repeat(40_000);
    let pack = compile(&step, &LedgerView::default(), &[], &hits(1, 100), 20_000);
    assert_eq!(pack.budget_tokens, 2_000);
    assert!(pack.items[0].included, "P0 is never trimmed");
    assert!(!pack.items[1].included);
    assert_eq!(
        pack.summary(),
        format!(
            "{:.1}k tokens · 0 docs · 1 trimmed (pointers)",
            f64::from(pack.token_estimate) / 1000.0
        )
    );
}

#[test]
fn compile_is_fast() {
    let ledger = LedgerView {
        handoff: Some(handoff()),
        step_outputs: vec![],
        ..Default::default()
    };
    let knowledge = hits(500, 2_000);
    let start = Instant::now();
    let pack = compile(&brief(None), &ledger, &[], &knowledge, 200_000);
    let elapsed = start.elapsed();
    assert_eq!(pack.budget_tokens, 20_000);
    assert!(elapsed.as_millis() <= 20, "compile took {elapsed:?}");
}

#[test]
fn workspace_context_is_its_own_p0_section() {
    let ledger = LedgerView {
        workspace_context: "  Deploy only from main.\n".into(),
        ..Default::default()
    };
    // Even with no room, like the task.
    let pack = compile(&brief(Some(10)), &ledger, &[], &[], 200_000);
    let item = &pack.items[1];
    assert_eq!(
        (item.priority, item.label.as_str(), item.included),
        (0, WORKSPACE_CONTEXT, true)
    );
    assert!(
        pack.render_markdown()
            .contains("## Workspace context\n\nDeploy only from main.\n")
    );
    assert!(
        pack.envelope()
            .contains("\n\n**Workspace context**\n\nDeploy only from main.")
    );
    let parts = pack.envelope_parts(&brief(Some(10)), &[]);
    assert!(
        parts
            .task
            .contains("**Workspace context**\n\nDeploy only from main.")
    );
    // None set: no section.
    let none = compile(&brief(None), &LedgerView::default(), &[], &[], 200_000);
    assert!(none.items.iter().all(|i| i.label != WORKSPACE_CONTEXT));
}

#[test]
fn ui_task_pack_includes_design_md() {
    let ledger = LedgerView {
        design: vec![
            DesignFile {
                path: "DESIGN.md".into(),
                text: "# Design\n\nButtons use --brand.".into(),
            },
            DesignFile {
                path: "src/styles/tokens.css".into(),
                text: ":root { --brand: #ff5a1f; }".into(),
            },
        ],
        ..Default::default()
    };
    let pack = compile(&brief(None), &ledger, &[], &[], 200_000);
    let md = pack.render_markdown();
    assert!(md.contains("## Design system: DESIGN.md\n\n# Design\n\nButtons use --brand."));
    assert!(md.contains("## Design system: src/styles/tokens.css"));
    let design = pack
        .items
        .iter()
        .find(|i| i.pointer == "DESIGN.md")
        .unwrap();
    assert_eq!((design.priority, design.included), (3, true));
    // The pack's files never overwrite the repo's own design files.
    assert!(pack.files(None).iter().all(|f| f.path.starts_with(".sun/")));

    // No room: the file itself is the pointer.
    let tight = compile(
        &brief(Some(80)),
        &LedgerView {
            design: vec![DesignFile {
                path: "DESIGN.md".into(),
                text: "d".repeat(4_000),
            }],
            ..Default::default()
        },
        &[],
        &[],
        200_000,
    );
    assert!(
        tight
            .render_markdown()
            .contains("- Design system: DESIGN.md (~1000 tokens): `DESIGN.md`")
    );
}

#[test]
fn files_are_written_and_progress_is_kept() {
    let dir = tempfile::tempdir().unwrap();
    let handoff = handoff();
    let ledger = LedgerView {
        handoff: Some(handoff.clone()),
        step_outputs: vec![],
        ..Default::default()
    };
    let pack = compile(&brief(None), &ledger, &[], &hits(3, 50), 200_000);
    write_files(dir.path(), &pack.files(Some(&handoff))).unwrap();

    let read = |p: &str| std::fs::read_to_string(dir.path().join(p)).unwrap();
    assert_eq!(read(PACK_PATH), pack.render_markdown());
    assert!(read(HANDOFF_PATH).contains("Full transcript: `blob:9f3c`"));
    assert!(read(".sun/context/sources/03.md").starts_with("# Source: Webhook guide v2"));
    assert_eq!(read(".sun/context/.gitignore"), "*\n");
    assert_eq!(read(PROGRESS_PATH), "retry done; tests next");

    // Re-render with fewer sources: stale ones go, the agent's progress stays.
    std::fs::write(dir.path().join(PROGRESS_PATH), "agent edits").unwrap();
    let pack = compile(
        &brief(None),
        &LedgerView::default(),
        &[],
        &hits(1, 50),
        200_000,
    );
    write_files(dir.path(), &pack.files(None)).unwrap();
    assert!(!dir.path().join(".sun/context/sources/03.md").exists());
    assert!(!dir.path().join(HANDOFF_PATH).exists());
    assert_eq!(read(PROGRESS_PATH), "agent edits");
}

#[tokio::test]
async fn packs_are_stored_as_blob_plus_row() {
    use sun_core::model::{Run, Step, Task, Workflow};
    use sun_store::{RunRepo, TaskRepo, WorkflowRepo};

    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let workflow = Workflow {
        id: WorkflowId::generate(),
        workspace_id: sun_core::WorkspaceId::MAIN,
        name: "fix".into(),
    };
    let task = Task {
        title: "t".into(),
        workflow_id: Some(workflow.id),
        status: TaskStatus::Todo,
        ..sun_core::model::Task::new(sun_core::WorkspaceId::MAIN, "")
    };
    let run = Run {
        id: RunId::generate(),
        task_id: task.id,
        workflow_id: workflow.id,
        workflow_version: 1,
        status: RunStatus::Queued,
        end_reason: None,
        budget: Default::default(),
        repo_ids: vec![],
    };
    let step = Step {
        id: StepId::generate(),
        run_id: run.id,
        step_key: StepKey::new("implement").unwrap(),
        attempt: 1,
        status: StepStatus::Queued,
        runtime_id: None,
        session_id: None,
        started_at: None,
        ended_at: None,
    };
    let mut tx = store.begin().await.unwrap();
    tx.insert_workflow(&workflow).await.unwrap();
    tx.save_workflow_version(workflow.id, "name: fix", "h")
        .await
        .unwrap();
    tx.insert_task(&task).await.unwrap();
    tx.insert_run(&run).await.unwrap();
    tx.insert_step(&step).await.unwrap();
    tx.commit().await.unwrap();

    let pack = compile(
        &brief(None),
        &LedgerView::default(),
        &[],
        &hits(2, 50),
        200_000,
    );
    let row = store_pack(store.blobs(), step.id, &pack).await.unwrap();
    let mut tx = store.begin().await.unwrap();
    tx.insert_context_pack(&row).await.unwrap();
    tx.commit().await.unwrap();

    let mut tx = store.read().await.unwrap();
    let stored = tx.get_context_pack(row.id).await.unwrap();
    assert_eq!(stored, row);
    assert_eq!(stored.items[0]["label"], "Task: Add retry to webhook");
    assert!(stored.items[0].get("body").is_none());
    let blob = store.blobs().get(&stored.pack_hash).await.unwrap();
    assert_eq!(String::from_utf8(blob).unwrap(), pack.render_markdown());
}

/// Records what the MCP server asked of the engine.
#[derive(Default)]
struct FakeEngine {
    calls: Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl EngineHandle for FakeEngine {
    async fn add_note(
        &self,
        run_id: RunId,
        step_key: StepKey,
        kind: NoteKind,
        body: String,
    ) -> Result<NoteId> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("note {run_id} {step_key} {kind} {body}"));
        Ok(NoteId::generate())
    }
    async fn run_status(&self, _: RunId) -> Result<RunStatus> {
        Ok(RunStatus::Running)
    }
    async fn task(&self, _: RunId, _: StepKey) -> Result<StepBrief> {
        Ok(brief(None))
    }
    async fn get(&self, _: RunId, pointer: String) -> Result<String> {
        if pointer == "doc:1" {
            Ok("the doc".into())
        } else {
            Err(Error::NotFound(pointer))
        }
    }
    async fn notes(&self, _: RunId) -> Result<Vec<Note>> {
        Ok(vec![note(NoteKind::Decision, "no Redis", false)])
    }
    async fn ask(
        &self,
        _: RunId,
        _: StepKey,
        question: String,
        options: Vec<String>,
    ) -> Result<String> {
        Ok(format!("{question} -> {}", options.join("|")))
    }
    async fn done(&self, _: RunId, _: StepKey, summary: String) -> Result<()> {
        self.calls.lock().unwrap().push(format!("done {summary}"));
        Ok(())
    }
    // The device side stands in for the host the engine forwards to.
    async fn device_screen(&self, _: RunId) -> Result<host_protocol::DeviceScreen> {
        Ok(host_protocol::DeviceScreen {
            device: "Pixel_7_API_34".into(),
            platform: host_protocol::DevicePlatform::Android,
            input: true,
            mime: "image/png".into(),
            image: b"PNG".to_vec(),
        })
    }
    async fn device_input(
        &self,
        run_id: RunId,
        step_key: StepKey,
        input: host_protocol::DeviceInput,
    ) -> Result<()> {
        let input = serde_json::to_string(&input).unwrap();
        let call = format!("input {run_id} {step_key} {input}");
        self.calls.lock().unwrap().push(call);
        Ok(())
    }
    async fn device_flow(
        &self,
        _: RunId,
        _: StepKey,
        request: host_protocol::DeviceFlowRequest,
    ) -> Result<host_protocol::DeviceResult> {
        let call = format!("flow {}", serde_json::to_string(&request).unwrap());
        self.calls.lock().unwrap().push(call);
        Ok(host_protocol::DeviceResult {
            device: "Pixel_7_API_34".into(),
            passed: false,
            output: "Assertion is false: Welcome is visible".into(),
            screenshots: Vec::new(),
        })
    }
}

#[derive(Clone, Default)]
struct Client;
impl ClientHandler for Client {}

#[tokio::test]
async fn mcp_server_tools_reach_the_engine_scoped_to_the_token() {
    let engine = Arc::new(FakeEngine::default());
    let sessions = McpSessions::new();
    let run_id = RunId::generate();
    let token = sessions.issue(run_id, StepKey::new("implement").unwrap());
    assert!(sessions.open("wrong", engine.clone()).is_err());
    let server = sessions.open(&token, engine.clone()).unwrap();

    let (server_io, client_io) = tokio::io::duplex(64 * 1024);
    let server_task =
        tokio::spawn(async move { server.serve(server_io).await.unwrap().waiting().await });
    let client = Client.serve(client_io).await.unwrap();

    let tools = client.peer().list_all_tools().await.unwrap();
    let mut names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        [
            "ask",
            "device_flow",
            "device_screen",
            "device_tap",
            "done",
            "get",
            "note",
            "notes",
            "search",
            "task"
        ]
    );

    let call = |name: &'static str, args: serde_json::Value| {
        let mut params = CallToolRequestParams::new(name);
        if let serde_json::Value::Object(map) = args {
            params = params.with_arguments(map);
        }
        client.call_tool(params)
    };
    let text = |result: rmcp::model::CallToolResult| {
        (
            result.is_error.unwrap_or(false),
            result.content[0].as_text().unwrap().text.clone(),
        )
    };

    // `search` without a knowledge store is an agent-visible error.
    let (err, body) = text(
        call("search", serde_json::json!({"query": "pricing"}))
            .await
            .unwrap(),
    );
    assert!(err && body.contains("not enabled"), "{body}");

    let (err, body) = text(call("task", serde_json::json!({})).await.unwrap());
    assert!(!err);
    assert!(body.contains("\"goal\": \"Retry failed webhook deliveries\""));

    assert_eq!(
        text(
            call("get", serde_json::json!({"id": "doc:1"}))
                .await
                .unwrap()
        ),
        (false, "the doc".into())
    );
    assert!(
        text(
            call("get", serde_json::json!({"id": "doc:2"}))
                .await
                .unwrap()
        )
        .0
    );

    let (err, _) = text(
        call(
            "note",
            serde_json::json!({"kind": "decision", "body": "token=sk-abcdefghijklmnopqrstuv"}),
        )
        .await
        .unwrap(),
    );
    assert!(!err);
    let (err, body) = text(
        call("note", serde_json::json!({"kind": "rumour", "body": "x"}))
            .await
            .unwrap(),
    );
    assert!(err && body.contains("rumour"), "{body}");
    assert!(
        text(
            call("note", serde_json::json!({"kind": "todo", "body": "  "}))
                .await
                .unwrap()
        )
        .0
    );

    let (_, body) = text(call("notes", serde_json::json!({})).await.unwrap());
    assert!(body.contains("no Redis"));
    let (_, body) = text(
        call(
            "ask",
            serde_json::json!({"question": "Redis?", "options": ["yes", "no"]}),
        )
        .await
        .unwrap(),
    );
    assert_eq!(body, "Redis? -> yes|no");
    assert_eq!(
        text(
            call("done", serde_json::json!({"summary": "retry added"}))
                .await
                .unwrap()
        ),
        (false, "verification requested".into())
    );

    let calls = engine.calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 2);
    assert!(calls[0].starts_with(&format!("note {run_id} implement decision ")));
    assert!(calls[0].contains("[REDACTED]") && !calls[0].contains("abcdefghij"));
    assert_eq!(calls[1], "done retry added");

    sessions.revoke(&token);
    assert!(sessions.open(&token, engine.clone()).is_err());
    client.cancel().await.unwrap();
    server_task.abort();
}

#[tokio::test]
async fn agent_device_tap_reaches_fake_host() {
    let engine = Arc::new(FakeEngine::default());
    let run_id = RunId::generate();
    let server = SunMcp::new(engine.clone(), run_id, StepKey::new("implement").unwrap());
    let (server_io, client_io) = tokio::io::duplex(64 * 1024);
    let server_task =
        tokio::spawn(async move { server.serve(server_io).await.unwrap().waiting().await });
    let client = Client.serve(client_io).await.unwrap();
    let call = |name: &'static str, args: serde_json::Value| {
        let serde_json::Value::Object(map) = args else {
            unreachable!()
        };
        client.call_tool(CallToolRequestParams::new(name).with_arguments(map))
    };

    let tapped = call("device_tap", serde_json::json!({"x": 10, "y": 20}))
        .await
        .unwrap();
    assert!(!tapped.is_error.unwrap_or(false));

    let screen = call("device_screen", serde_json::json!({})).await.unwrap();
    assert!(
        screen.content[0]
            .as_text()
            .unwrap()
            .text
            .contains("Pixel_7")
    );
    let image = screen.content[1].as_image().unwrap();
    assert_eq!(
        (image.data.as_str(), image.mime_type.as_str()),
        ("UE5H", "image/png")
    );

    let flow = call(
        "device_flow",
        serde_json::json!({"flow": "appId: dev.sun\n---\n- launchApp", "platform": "android", "app": "app.apk"}),
    )
    .await
    .unwrap();
    let body = &flow.content[0].as_text().unwrap().text;
    assert!(
        body.starts_with("flow failed on Pixel_7_API_34") && body.contains("Welcome"),
        "{body}"
    );

    // Bad input stops at the MCP boundary.
    for bad in [
        serde_json::json!({"flow": "- launchApp", "platform": "windows"}),
        serde_json::json!({"flow": "  "}),
    ] {
        let refused = call("device_flow", bad).await.unwrap();
        assert!(refused.is_error.unwrap_or(false));
    }

    let calls = engine.calls.lock().unwrap().clone();
    assert_eq!(
        calls[0],
        format!(r#"input {run_id} implement {{"kind":"tap","x":10,"y":20}}"#)
    );
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert!(
        calls[1].contains(r#""platform":"android""#) && calls[1].contains(r#""app":"app.apk""#),
        "{}",
        calls[1]
    );
    client.cancel().await.unwrap();
    server_task.abort();
}

#[tokio::test]
async fn knowledge_search_tool_returns_cited_snippets() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let source = knowledge::add_source(&store, "upload", "prds", serde_json::json!({}))
        .await
        .unwrap();
    let doc = knowledge::SourceDoc {
        external_id: "prd.md".into(),
        title: "Billing PRD".into(),
        version: "v2".into(),
        updated_at: None,
        url: None,
        acl: Vec::new(),
        content: knowledge::DocContent::Markdown("# Pricing\n\nRp 50.000 per seat.".into()),
    };
    knowledge::ingest(&store, &source, doc).await.unwrap();

    let sessions = McpSessions::new().with_knowledge(store);
    let token = sessions.issue(RunId::generate(), StepKey::new("implement").unwrap());
    let server = sessions
        .open(&token, Arc::new(FakeEngine::default()))
        .unwrap();
    let (server_io, client_io) = tokio::io::duplex(64 * 1024);
    let server_task =
        tokio::spawn(async move { server.serve(server_io).await.unwrap().waiting().await });
    let client = Client.serve(client_io).await.unwrap();

    let search = |args: serde_json::Value| {
        let serde_json::Value::Object(map) = args else {
            unreachable!()
        };
        client.call_tool(CallToolRequestParams::new("search").with_arguments(map))
    };
    let result = search(serde_json::json!({"query": "seat price", "k": 3}))
        .await
        .unwrap();
    assert!(!result.is_error.unwrap_or(false));
    let body = result.content[0].as_text().unwrap().text.clone();
    assert!(
        body.starts_with("## Billing PRD > Pricing · v2\ndoc:"),
        "{body}"
    );
    assert!(body.contains("Rp 50.000 per seat."));
    let result = search(serde_json::json!({"query": "kubernetes"}))
        .await
        .unwrap();
    assert!(
        result.content[0]
            .as_text()
            .unwrap()
            .text
            .starts_with("no matches")
    );

    client.cancel().await.unwrap();
    server_task.abort();
}

#[test]
fn pack_lists_the_workspace_repos_agents_can_use() {
    let ledger = LedgerView {
        repos: vec![
            (
                "https://github.com/acme/web.git".into(),
                "Customer site\nNext.js".into(),
            ),
            ("https://gitlab.com/acme/api.git".into(), String::new()),
        ],
        ..Default::default()
    };
    let pack = compile(&brief(Some(1)), &ledger, &[], &[], 1_000);
    let item = pack
        .items
        .iter()
        .find(|i| i.label == "Repositories agents can use")
        .unwrap();
    assert!(item.included, "P0: in even a tiny budget");
    assert_eq!(
        item.body,
        "- https://github.com/acme/web.git: Customer site Next.js\n- https://gitlab.com/acme/api.git\n"
    );
    let empty = compile(&brief(None), &LedgerView::default(), &[], &[], 200_000);
    assert!(
        empty
            .items
            .iter()
            .all(|i| !i.label.starts_with("Repositories"))
    );
}
