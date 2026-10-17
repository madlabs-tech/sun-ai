//! Wire shapes of `/api/v1` — the mirror of `web/src/lib/api/types.ts` — and
//! the mapping from engine models to them. A task's status is its own (the
//! engine moves it after its run); a run's (`verifying`, `waiting_approval`)
//! is derived here from its steps.

use std::collections::{HashMap, HashSet};

use serde::Serialize;
use serde_json::{Value, json};
use sun_core::model::{
    Artifact, Assignee, EventRecord, Priority, Run as RunRow, Step as StepRow, Task, Workflow,
};
use sun_core::{Result, RunStatus, StateMachine, TaskStatus, Timestamp, WorkflowId, WorkspaceId};
use sun_engine::{Engine, Plan};

/// `TaskSummary` (+ `gates`, `artifacts` for `TaskDetail`).
#[derive(Debug, Serialize)]
pub(crate) struct TaskView {
    pub id: String,
    /// `{issue_prefix}-{number}`; `null` for a chat.
    pub key: Option<String>,
    pub workspace_id: String,
    /// `null` for a chat.
    pub number: Option<u32>,
    pub title: String,
    pub body: String,
    pub priority: Priority,
    pub status: TaskStatus,
    /// `null`: a plain task that never runs.
    pub workflow: Option<WorkflowRef>,
    pub project_id: Option<String>,
    /// Who works on it: `{kind: "agent" | "workflow", id}`; `null` for a plain task.
    pub assignee: Option<Assignee>,
    pub position: f64,
    /// The member who created it; `null`: the engine.
    pub created_by: Option<String>,
    pub run: Option<RunView>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gates: Option<Vec<GateView>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artifacts: Option<Vec<Artifact>>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct WorkflowRef {
    pub id: String,
    pub name: String,
    pub workspace_id: String,
    pub version: u32,
}

#[derive(Debug, Serialize)]
pub(crate) struct RunView {
    pub id: String,
    pub task_id: String,
    pub workflow_version: u32,
    pub status: &'static str,
    pub end_reason: Option<&'static str>,
    /// The engine's own end reason text, e.g. `done (unverified): …`.
    pub end_detail: Option<String>,
    pub budget: Value,
    pub cost_usd: f64,
    pub runtime_id: Option<String>,
    pub runtime_chain: Vec<String>,
    pub runtime_switches: u32,
    pub steps: Vec<StepView>,
    /// The workspace repos the run works in (resolved from the task
    /// text); none: a plain folder.
    pub repo_ids: Vec<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct StepView {
    pub step_key: String,
    pub name: String,
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub attempt: u32,
    pub status: &'static str,
    pub runtime_id: Option<String>,
    pub started_at: Option<Timestamp>,
    pub ended_at: Option<Timestamp>,
}

#[derive(Debug, Serialize)]
pub(crate) struct GateView {
    pub step_key: String,
    pub name: String,
    pub status: &'static str,
}

/// `RunEvent`: the stored event plus a one-line `payload.summary`.
#[derive(Debug, Serialize)]
pub(crate) struct EventView {
    pub seq: i64,
    pub run_id: String,
    pub step_key: Option<String>,
    pub kind: String,
    pub payload: Value,
    pub at: Timestamp,
}

/// Workflow names and latest versions (hidden workflows too, for task
/// views; never listed), and workspace issue prefixes (for task keys),
/// loaded once per request.
pub(crate) struct Workflows {
    workflows: HashMap<WorkflowId, WorkflowRef>,
    system: HashSet<WorkflowId>,
    prefixes: HashMap<WorkspaceId, String>,
}

impl Workflows {
    pub(crate) async fn load(engine: &Engine) -> Result<Self> {
        let system = engine.hidden_workflows().await?;
        let mut loaded = Self::of(engine.workflows().await?);
        loaded.system = system.iter().map(|(w, _)| w.id).collect();
        loaded.workflows.extend(Self::of(system).workflows);
        loaded.prefixes = engine
            .workspaces()
            .await?
            .into_iter()
            .map(|w| (w.id, w.issue_prefix))
            .collect();
        Ok(loaded)
    }

    /// Workflows only: no task keys.
    pub(crate) fn of(workflows: Vec<(Workflow, u32)>) -> Self {
        let workflows = workflows
            .into_iter()
            .map(|(w, version)| {
                let view = WorkflowRef {
                    id: w.id.to_string(),
                    name: w.name,
                    workspace_id: w.workspace_id.to_string(),
                    version,
                };
                (w.id, view)
            })
            .collect();
        Self {
            workflows,
            system: HashSet::new(),
            prefixes: HashMap::new(),
        }
    }

    /// True when workflow `id` is known and `sees` its workspace.
    pub(crate) fn on_workspace(&self, id: WorkflowId, sees: impl Fn(&str) -> bool) -> bool {
        self.workflows
            .get(&id)
            .is_some_and(|w| sees(&w.workspace_id))
    }

    /// Every workflow but hidden ones, by id.
    pub(crate) fn all(&self) -> Vec<WorkflowRef> {
        let mut all: Vec<_> = self
            .workflows
            .iter()
            .filter(|(id, _)| !self.system.contains(id))
            .map(|(_, w)| w.clone())
            .collect();
        all.sort_by(|a, b| a.id.cmp(&b.id));
        all
    }

    /// The task's key, from its workspace's prefix (the one place keys are
    /// written); `None` for a chat.
    pub(crate) fn task_key(&self, task: &Task) -> Option<String> {
        let prefix = self.prefixes.get(&task.workspace_id).cloned();
        task.key(prefix.unwrap_or_default())
            .ok()
            .map(|k| k.to_string())
    }
}

/// Builds the workspace card of a task from its latest run.
// ponytail: a few reads per task; batch them when workspaces get big.
pub(crate) async fn task_view(
    engine: &Engine,
    workflows: &Workflows,
    task: Task,
    detail: bool,
) -> Result<TaskView> {
    let latest = engine.runs_of_task(task.id).await?.pop();
    let run = match latest {
        Some(run) => Some(run_view(engine, run).await?),
        None => None,
    };
    let gates = detail.then(|| run.as_ref().map(gates_of).unwrap_or_default());
    let artifacts = match detail {
        true => Some(engine.artifacts(task.id).await?),
        false => None,
    };
    let workflow = task.workflow_id.map(|id| {
        let known = workflows.workflows.get(&id).cloned();
        let workflow = known.unwrap_or_else(|| WorkflowRef {
            id: id.to_string(),
            name: String::new(),
            workspace_id: task.workspace_id.to_string(),
            version: 0,
        });
        WorkflowRef {
            version: run
                .as_ref()
                .map_or(workflow.version, |r| r.workflow_version),
            ..workflow
        }
    });
    Ok(TaskView {
        key: workflows.task_key(&task),
        assignee: task.assignee(),
        id: task.id.to_string(),
        workspace_id: task.workspace_id.to_string(),
        number: task.number,
        title: task.title,
        body: task.body,
        priority: task.priority,
        status: task.status,
        workflow,
        project_id: task.project_id.map(|p| p.to_string()),
        position: task.position,
        created_by: task.created_by.map(|m| m.to_string()),
        run,
        created_at: task.created_at,
        updated_at: task.updated_at,
        gates,
        artifacts,
    })
}

/// The run's gate steps, as the task detail shows them.
fn gates_of(run: &RunView) -> Vec<GateView> {
    run.steps
        .iter()
        .filter(|s| s.kind == "gate")
        .map(|s| GateView {
            step_key: s.step_key.clone(),
            name: s.name.clone(),
            status: gate_status(s.status),
        })
        .collect()
}

/// `GET /runs/{id}`: any run of a task, with its steps and gates in the
/// task detail's shapes.
#[derive(Debug, Serialize)]
pub(crate) struct RunDetailView {
    pub run: RunView,
    pub gates: Vec<GateView>,
}

pub(crate) async fn run_detail(engine: &Engine, run_id: sun_core::RunId) -> Result<RunDetailView> {
    let run = run_view(engine, engine.run(run_id).await?).await?;
    Ok(RunDetailView {
        gates: gates_of(&run),
        run,
    })
}

/// One run of a task for its execution log (`GET /tasks/{id}/runs`).
#[derive(Debug, Serialize)]
pub(crate) struct TaskRunView {
    pub id: String,
    /// 1 for the task's first run.
    pub attempt: usize,
    /// Why it ran: `created`, `trigger`, `moved`, `retry`, or `manual` for
    /// runs from before triggers were recorded.
    pub trigger: String,
    pub status: &'static str,
    pub cost_usd: f64,
    pub started_at: Option<Timestamp>,
    pub ended_at: Option<Timestamp>,
}

/// The task's runs, oldest first.
pub(crate) async fn task_runs(engine: &Engine, task: sun_core::TaskId) -> Result<Vec<TaskRunView>> {
    let mut out = Vec::new();
    for (i, run) in engine.runs_of_task(task).await?.into_iter().enumerate() {
        let first = engine.events(run.id, 0, 1).await?;
        let trigger = first
            .first()
            .and_then(|e| e.payload["reason"].as_str())
            .unwrap_or("manual")
            .to_owned();
        let ended = run.status.is_terminal();
        let view = run_view(engine, run).await?;
        out.push(TaskRunView {
            attempt: i + 1,
            trigger,
            started_at: view.steps.iter().filter_map(|s| s.started_at).min(),
            ended_at: ended
                .then(|| view.steps.iter().filter_map(|s| s.ended_at).max())
                .flatten(),
            status: view.status,
            cost_usd: view.cost_usd,
            id: view.id,
        });
    }
    Ok(out)
}

async fn run_view(engine: &Engine, run: RunRow) -> Result<RunView> {
    let plan = engine.plan(&run).await?;
    let attempts = engine.steps(run.id).await?;
    let usage = engine.usage(Some(run.id)).await?;
    let steps = steps_view(&plan, &attempts);
    let status = run_status(run.status, &steps);
    Ok(RunView {
        id: run.id.to_string(),
        task_id: run.task_id.to_string(),
        workflow_version: run.workflow_version,
        status,
        end_reason: end_reason(&run, &steps),
        end_detail: run.end_reason,
        budget: json!({
            "wall": run.budget.wall_secs.map(|s| format!("{s}s")),
            "turns": run.budget.turns,
            "cost_usd": run.budget.cost_usd,
        }),
        cost_usd: usage.cost_usd,
        runtime_id: None,
        runtime_chain: plan.runtime_chain,
        runtime_switches: 0,
        steps,
        repo_ids: run.repo_ids.iter().map(ToString::to_string).collect(),
    })
}

/// One row per workflow step: its latest attempt, or `queued` if not reached.
fn steps_view(plan: &Plan, attempts: &[StepRow]) -> Vec<StepView> {
    plan.steps
        .iter()
        .map(|planned| {
            let latest = attempts
                .iter()
                .filter(|s| s.step_key == planned.key)
                .max_by_key(|s| s.attempt);
            StepView {
                step_key: planned.key.to_string(),
                name: planned.key.to_string(),
                kind: planned.kind,
                attempt: latest.map_or(1, |s| s.attempt),
                status: latest.map_or("queued", |s| s.status.as_str()),
                runtime_id: latest.and_then(|s| s.runtime_id).map(|id| id.to_string()),
                started_at: latest.and_then(|s| s.started_at),
                ended_at: latest.and_then(|s| s.ended_at),
            }
        })
        .collect()
}

/// The workspace status of a run: `running` splits into `verifying` and
/// `waiting_approval` by what its steps do now.
fn run_status(status: RunStatus, steps: &[StepView]) -> &'static str {
    let doing = |what: &str| steps.iter().any(|s| s.status == what);
    match status {
        RunStatus::Queued => "queued",
        RunStatus::Running if doing("waiting_approval") => "waiting_approval",
        RunStatus::Running
            if doing("verifying")
                || steps
                    .iter()
                    .any(|s| s.kind == "gate" && s.status == "running") =>
        {
            "verifying"
        }
        RunStatus::Running => "running",
        RunStatus::Paused => "waiting_approval",
        RunStatus::Succeeded => "done",
        RunStatus::Cancelled => "cancelled",
        RunStatus::Failed | RunStatus::Stalled | RunStatus::Looping | RunStatus::BudgetExceeded => {
            "failed"
        }
    }
}

/// `EndReason` of the UI from the run and its steps' latest attempts.
fn end_reason(run: &RunRow, steps: &[StepView]) -> Option<&'static str> {
    let has = |kind: &str, status: &str| {
        steps
            .iter()
            .any(|s| (kind.is_empty() || s.kind == kind) && s.status == status)
    };
    let unverified = run
        .end_reason
        .as_deref()
        .is_some_and(|r| r.starts_with("done (unverified)"));
    match run.status {
        RunStatus::Succeeded if unverified => Some("unverified"),
        RunStatus::Succeeded if has("approval", "succeeded") => Some("approved"),
        RunStatus::Succeeded => Some("gates_passed"),
        RunStatus::Cancelled => Some("cancelled"),
        status if status.is_terminal() && has("", "rejected") => Some("rejected"),
        status if status.is_terminal() => Some("failed"),
        _ => None,
    }
}

fn gate_status(step_status: &str) -> &'static str {
    match step_status {
        "queued" => "pending",
        "running" | "verifying" => "running",
        "succeeded" => "passed",
        _ => "failed",
    }
}

/// The ULID's timestamp.
pub(crate) fn created_at(id: &str) -> Timestamp {
    ulid::Ulid::from_string(id)
        .ok()
        .and_then(|u| Timestamp::from_timestamp_millis(u.timestamp_ms() as i64))
        .unwrap_or_default()
}

pub(crate) fn event_view(event: EventRecord) -> EventView {
    let mut payload = match event.payload {
        Value::Object(map) => map,
        other => {
            let mut map = serde_json::Map::new();
            map.insert("value".into(), other);
            map
        }
    };
    if !payload.contains_key("summary") {
        let summary = summary(&event.kind, &payload);
        payload.insert("summary".into(), Value::String(summary));
    }
    EventView {
        seq: event.seq,
        run_id: event.run_id.to_string(),
        step_key: event.step_key.map(|k| k.to_string()),
        kind: event.kind,
        payload: Value::Object(payload),
        at: event.at,
    }
}

/// One plain-language line per event.
fn summary(kind: &str, p: &serde_json::Map<String, Value>) -> String {
    let s = |key: &str| p.get(key).and_then(Value::as_str).unwrap_or("");
    match kind {
        "step_status" | "run_status" | "task_status" => {
            let what = kind.trim_end_matches("_status");
            let reason = s("reason");
            let mut line = format!("{what} {} → {}", s("from"), s("to"));
            if !reason.is_empty() {
                line.push_str(&format!(" · {reason}"));
            }
            line.trim_start_matches(' ').replace("  ", " ")
        }
        "intervention" => format!("You: {}", s("text")),
        // The step view shows where the agent ran.
        "session_assigned" => {
            let env = p.get("exec_env");
            let field = |key: &str| env.and_then(|e| e.get(key)).and_then(Value::as_str);
            let place = match field("kind") {
                Some("container") => format!("container {}", field("image").unwrap_or("?")),
                Some("vm") => format!("VM {}", field("template").unwrap_or("?")),
                _ => "on the host".into(),
            };
            // Who paid for the session.
            let paid = match s("source") {
                "byo_runtime" => " · your subscription",
                "byo_key" => " · your API key",
                "credits" => " · Sun credits",
                _ => "",
            };
            format!("Session on {}{paid} · {place}", s("adapter"))
        }
        // The timeline says which runtime took over, and why.
        "handoff" => {
            let why = match s("reason") {
                "limit_hit" => "hit its limit",
                "manual_switch" => "switched by hand",
                "switch_back" => "is back after its cooldown",
                "context_rotation" => "context full",
                "watchdog_restart" => "restarted by the watchdog",
                "host_lost" => "host lost",
                _ => "hand-off",
            };
            let (from, to) = (s("from"), s("to"));
            match from == to {
                true => format!("Fresh {to} session with a hand-off pack ({why})"),
                false if s("reason") == "switch_back" => {
                    format!("Back on {to} ({to} {why}) with a hand-off pack")
                }
                false => format!("Moved from {from} to {to} with a hand-off pack ({from} {why})"),
            }
        }
        "text" => s("text").to_string(),
        "tool_call" => format!("Ran {}", s("name")),
        "approval" => {
            let verdict = if p.get("approved") == Some(&Value::Bool(true)) {
                "Approved"
            } else {
                "Rejected"
            };
            format!("{verdict} by {}", s("by"))
        }
        "gate_result" => {
            let result = p.get("result").and_then(Value::as_object);
            let name = result
                .and_then(|r| r.get("name"))
                .and_then(Value::as_str)
                .unwrap_or("gate");
            let pass = result.and_then(|r| r.get("pass")) == Some(&Value::Bool(true));
            format!("{name} {}", if pass { "passed" } else { "failed" })
        }
        "data_touched" => match p.get("files").and_then(Value::as_array) {
            Some(files) if !files.is_empty() => {
                format!("Data touched (detected): {} file(s)", files.len())
            }
            _ => "No data touched (detected)".into(),
        },
        "decision" => {
            let answer = p.get("answer").and_then(Value::as_object);
            let field = |k: &str| answer.and_then(|a| a.get(k));
            let answer = match (field("choice"), field("score"), field("noul")) {
                (Some(Value::String(label)), _, _) => label.clone(),
                (_, Some(level), _) => format!("level {level}"),
                (_, _, Some(p)) => format!("{:.0}% likely", p.as_f64().unwrap_or(0.0) * 100.0),
                _ => "no answer".into(),
            };
            let by = match s("by") {
                "" => String::new(),
                by => format!(" · by {by}"),
            };
            // Engine hooks (watchdog, failover, ...) fall back to a rule.
            match (p.get("acted") == Some(&Value::Bool(true)), p.get("rule")) {
                (true, _) => format!("Decided {}: {answer}{by}", s("decides")),
                (false, Some(_)) => {
                    format!("Decision {} kept the rule: {}", s("decides"), s("reason"))
                }
                (false, None) => {
                    format!("Decision {} needs a person: {}", s("decides"), s("reason"))
                }
            }
        }
        "triaged" => format!(
            "Priority set to {}",
            p.get("priority").unwrap_or(&Value::Null)
        ),
        "pr_merged" => format!("PR merged into {}", s("base")),
        "pr_state" if s("state") == "closed" => "PR closed without merging".into(),
        "pr_state" => "PR reopened".into(),
        // An extension's metric, asked at run end.
        "metric" => format!(
            "Metric {}: {} · {}",
            s("name"),
            p.get("value").unwrap_or(&Value::Null),
            s("mod")
        ),
        "gate_suggested" => format!("Suggested gate: {} with `{}`", s("gate"), s("command")),
        "escalated" => format!(
            "Gave up after {} send-back(s): needs you",
            p.get("send_backs").and_then(Value::as_u64).unwrap_or(0)
        ),
        "quality_checked" => match p.get("held_up").and_then(Value::as_bool) {
            Some(true) => "Post-merge check: held up".into(),
            Some(false) => "Post-merge check: needed follow-up".into(),
            None => "Post-merge check: CI pending".into(),
        },
        other => other.replace('_', " "),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summaries_keys_and_statuses() {
        let p = json!({"from": "running", "to": "verifying", "reason": null});
        let p = p.as_object().unwrap();
        assert_eq!(summary("step_status", p), "step running → verifying");
        let p = json!({"result": {"name": "test", "pass": false}});
        assert_eq!(
            summary("gate_result", p.as_object().unwrap()),
            "test failed"
        );
        assert_eq!(
            summary("session_started", &Default::default()),
            "session started"
        );
        let handoff = json!({"from": "claude-code", "to": "codex", "reason": "limit_hit"});
        assert_eq!(
            summary("handoff", handoff.as_object().unwrap()),
            "Moved from claude-code to codex with a hand-off pack (claude-code hit its limit)"
        );
        let session =
            json!({"adapter": "codex", "source": "byo_key", "exec_env": {"kind": "bare"}});
        assert_eq!(
            summary("session_assigned", session.as_object().unwrap()),
            "Session on codex · your API key · on the host"
        );
        let metric = json!({"mod": "lint", "name": "lines", "value": 42});
        assert_eq!(
            summary("metric", metric.as_object().unwrap()),
            "Metric lines: 42 · lint"
        );
        assert_eq!(gate_status("verifying"), "running");
        let decided = json!({"decides": "triage", "acted": true, "answer": {"choice": "billing"}, "by": "ana"});
        assert_eq!(
            summary("decision", decided.as_object().unwrap()),
            "Decided triage: billing · by ana"
        );
        let asked =
            json!({"decides": "flag", "acted": false, "reason": "low", "answer": {"noul": 0.4}});
        assert_eq!(
            summary("decision", asked.as_object().unwrap()),
            "Decision flag needs a person: low"
        );
    }
}

/// An outbox row for a human (`GET /outbox`): what it does and for which
/// step, never its payload (it may carry file bytes).
#[derive(Debug, Serialize)]
pub(crate) struct OutboxView {
    pub id: String,
    pub run_id: Option<String>,
    pub step_key: Option<String>,
    pub target: String,
    pub idem_key: String,
    pub status: &'static str,
    /// The PR title or message text, if the payload has one.
    pub summary: Option<String>,
}

pub(crate) fn outbox_view(item: &sun_core::model::OutboxItem) -> OutboxView {
    let text = |key: &str| item.payload.get(key).and_then(Value::as_str);
    OutboxView {
        id: item.id.to_string(),
        run_id: item.run_id.map(|r| r.to_string()),
        // Keys of run rows are `<run>:<step>:<action>`.
        step_key: item
            .run_id
            .and(item.idem_key.as_str().split(':').nth(1))
            .map(str::to_owned),
        target: item.target.clone(),
        idem_key: item.idem_key.as_str().to_owned(),
        status: item.status.as_str(),
        summary: text("title")
            .or_else(|| text("text"))
            .map(|t| t.chars().take(200).collect()),
    }
}
