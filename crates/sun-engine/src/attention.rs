//! Notifications for runs that need a person or have news.
//!
//! Every `to:` channel of the kernel's [`Attention`] gets a message when:
//! - a step starts waiting for an approval (or an answer) or the watchdog
//!   stops it (stalled, looping, over budget): with a deep link,
//!   `<public_url>/#/approve/<run>/<step>`, that opens the UI's two-tap
//!   sheet. A waiting approval also carries `approval_id` `<run>/<step>`, so
//!   mod-slack adds Approve / Reject buttons (the click comes back as an
//!   `approval` trigger, see `triggers`);
//! - a step fails over to another runtime (a `handoff` event, not a manual
//!   switch);
//! - a gate step used up its send-backs and escalates (`escalated`);
//! - a run ends (succeeded, failed, cancelled): with `#/tasks/<task>`.
//!
//! **Observer** on the event log; each message is a **transactional outbox**
//! row keyed by the event's `seq`, so an event is announced once.

use serde_json::{Value, json};
use sun_core::model::{EventRecord, OutboxItem, Task};
use sun_core::{IdemKey, Result, StepKey};
use sun_store::{OutboxRepo, RunRepo, TaskRepo};
use tokio::sync::broadcast::error::RecvError;

use crate::Engine;
use crate::executor::agent::ASKED;
use crate::executor::failover::HANDOFF;
use crate::executor::kind::{RUN_STATUS, STEP_STATUS};
use crate::gates::ESCALATED;
use crate::outbox::notify_item;

/// Where the kernel sends "needs you" messages (`sun.yaml`).
#[derive(Debug, Clone)]
pub struct Attention {
    /// The URL people open the UI at, without a trailing `/`.
    pub public_url: String,
    /// Notifier targets, e.g. `slack:#eng`.
    pub to: Vec<String>,
}

/// Step statuses that need a person, and how the message says it.
const NEEDS: &[(&str, &str)] = &[
    ("waiting_approval", "waits for you"),
    ("stalled", "is stalled"),
    ("looping", "is looping"),
    ("budget_exceeded", "is over budget"),
];

/// Run endings worth a message, and how the message says it.
const ENDS: &[(&str, &str)] = &[
    ("succeeded", "done"),
    ("failed", "failed"),
    ("cancelled", "cancelled"),
];

/// Idempotency-key step of run-level messages.
const RUN_KEY: &str = "run";

/// What one event announces.
struct Notice {
    /// The step the outbox key is scoped to.
    step: StepKey,
    /// The sentence after `<task title>: `.
    what: String,
    /// Link to the approve sheet of `step` (else to the task).
    sheet: bool,
    /// Add Approve / Reject buttons.
    buttons: bool,
}

/// The notice `event` calls for (none for most events).
fn notice(event: &EventRecord) -> Option<Notice> {
    let field = |k: &str| event.payload.get(k).and_then(Value::as_str);
    match (event.kind.as_str(), &event.step_key) {
        (STEP_STATUS, Some(step)) => {
            let (status, what) = NEEDS.iter().find(|(s, _)| Some(*s) == field("to"))?;
            Some(Notice {
                step: step.clone(),
                what: format!("step {step} {what}"),
                sheet: true,
                buttons: *status == "waiting_approval" && field("reason") != Some(ASKED),
            })
        }
        (HANDOFF, Some(step)) => {
            let (from, to, reason) = (field("from")?, field("to")?, field("reason")?);
            if from == to || reason == "manual_switch" {
                return None;
            }
            let why = reason.replace('_', " ");
            Some(Notice {
                step: step.clone(),
                what: format!("step {step} failed over from {from} to {to} ({why})"),
                sheet: false,
                buttons: false,
            })
        }
        (ESCALATED, Some(step)) => Some(Notice {
            step: step.clone(),
            what: format!("step {step} needs you: {}", field("reason")?),
            sheet: false,
            buttons: false,
        }),
        (RUN_STATUS, None) => {
            let (_, what) = ENDS.iter().find(|(s, _)| Some(*s) == field("to"))?;
            let what = match field("reason").filter(|r| !r.is_empty()) {
                Some(reason) => format!("{what} ({reason})"),
                None => (*what).to_owned(),
            };
            Some(Notice {
                step: StepKey::new(RUN_KEY).ok()?,
                what,
                sheet: false,
                buttons: false,
            })
        }
        _ => None,
    }
}

/// The messages `event` calls for, one per channel.
fn rows(event: &EventRecord, task: &Task, attention: &Attention) -> Vec<OutboxItem> {
    let Some(notice) = notice(event) else {
        return Vec::new();
    };
    let (run, step) = (event.run_id, &notice.step);
    let link = match notice.sheet {
        true => format!("{}/#/approve/{run}/{step}", attention.public_url),
        false => format!("{}/#/tasks/{}", attention.public_url, task.id),
    };
    let text = format!("{}: {}. {link}", task.title, notice.what);
    attention
        .to
        .iter()
        .map(|to| {
            let suffix = format!("attention:{}:{to}", event.seq);
            let mut item = notify_item(Some(run), IdemKey::new(run, step, &suffix), to, &text);
            if notice.buttons {
                item.payload["approval_id"] = json!(format!("{run}/{step}"));
            }
            item
        })
        .collect()
}

impl Engine {
    /// Runs forever: announces every step that starts needing a person,
    /// every failover and every run that ends.
    /// `sun serve` spawns it when `sun.yaml` names `notify_to`.
    // ponytail: live events only; a step that started waiting while the
    // kernel was down (or during a broadcast lag) is not announced.
    pub async fn follow_attention(self, attention: Attention) {
        let mut events = self.store.subscribe();
        loop {
            let event = match events.recv().await {
                Ok(event) => event,
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => return,
            };
            if let Err(e) = self.announce(&event, &attention).await {
                eprintln!("sun: attention: {e}");
            }
        }
    }

    async fn announce(&self, event: &EventRecord, attention: &Attention) -> Result<()> {
        if notice(event).is_none() {
            return Ok(());
        }
        let mut tx = self.store.begin().await?;
        let run = tx.get_run(event.run_id).await?;
        let task = tx.get_task(run.task_id).await?;
        for row in rows(event, &task, attention) {
            tx.enqueue(&row).await?;
        }
        tx.commit().await?;
        self.outbox_wake.notify_one();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use serde_json::json;
    use sun_core::{RunId, StepKey, TaskStatus, WorkflowId};

    use super::*;

    fn attention() -> Attention {
        Attention {
            public_url: "https://sun.example".into(),
            to: vec!["slack:#eng".into(), "slack:#ops".into()],
        }
    }

    fn task(title: &str) -> Task {
        Task {
            title: title.into(),
            workflow_id: Some(WorkflowId::generate()),
            status: TaskStatus::InProgress,
            ..sun_core::model::Task::new(sun_core::WorkspaceId::MAIN, "")
        }
    }

    fn record(run: RunId, step: Option<&str>, kind: &str, payload: Value) -> EventRecord {
        EventRecord {
            seq: 42,
            run_id: run,
            step_key: step.map(|s| StepKey::new(s).unwrap()),
            kind: kind.into(),
            payload,
            blob: None,
            at: chrono::Utc::now(),
        }
    }

    #[test]
    fn waiting_and_stuck_steps_get_a_deep_link_per_channel() {
        let attention = attention();
        let run = RunId::generate();
        let event = |to: &str, reason: Option<&str>| {
            let payload = json!({ "from": "running", "to": to, "reason": reason });
            record(run, Some("review"), STEP_STATUS, payload)
        };
        let fix = task("Fix login");
        let got = rows(&event("waiting_approval", None), &fix, &attention);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].target, "slack.notify");
        assert_eq!(got[0].payload["channel"], "#eng");
        assert_eq!(
            got[0].payload["text"],
            format!(
                "Fix login: step review waits for you. https://sun.example/#/approve/{run}/review"
            )
        );
        assert_eq!(got[0].payload["approval_id"], format!("{run}/review"));
        assert_eq!(
            got[1].idem_key.as_str(),
            format!("{run}:review:attention:42:slack:#ops")
        );
        let asked = rows(&event("waiting_approval", Some(ASKED)), &fix, &attention);
        assert!(asked[0].payload.get("approval_id").is_none(), "no buttons");
        let stalled = rows(&event("stalled", None), &fix, &attention);
        assert!(
            stalled[0].payload["text"]
                .as_str()
                .unwrap()
                .contains("is stalled")
        );
        assert!(stalled[0].payload.get("approval_id").is_none());
        assert!(rows(&event("running", None), &fix, &attention).is_empty());
    }

    #[test]
    fn failover_and_completion_notify() {
        let attention = attention();
        let run = RunId::generate();
        let fix = task("Fix login");
        let handoff = |from: &str, to: &str, reason: &str| {
            let payload = json!({ "from": from, "to": to, "reason": reason });
            rows(
                &record(run, Some("build"), HANDOFF, payload),
                &fix,
                &attention,
            )
        };
        let failover = handoff("claude", "codex", "limit_hit");
        assert_eq!(failover.len(), 2);
        assert_eq!(
            failover[0].payload["text"],
            format!(
                "Fix login: step build failed over from claude to codex (limit hit). https://sun.example/#/tasks/{}",
                fix.id
            )
        );
        assert!(failover[0].payload.get("approval_id").is_none());
        assert!(handoff("claude", "codex", "manual_switch").is_empty());
        assert!(handoff("claude", "claude", "host_lost").is_empty());

        let ended = |to: &str, reason: Option<&str>| {
            let payload = json!({ "from": "running", "to": to, "reason": reason });
            rows(&record(run, None, RUN_STATUS, payload), &fix, &attention)
        };
        let done = ended("succeeded", None);
        assert_eq!(
            done[0].payload["text"],
            format!("Fix login: done. https://sun.example/#/tasks/{}", fix.id)
        );
        assert_eq!(
            done[0].idem_key.as_str(),
            format!("{run}:run:attention:42:slack:#eng")
        );
        let failed = ended("failed", Some("step `build` failed"));
        assert!(
            failed[0].payload["text"]
                .as_str()
                .unwrap()
                .starts_with("Fix login: failed (step `build` failed).")
        );
        assert!(!ended("cancelled", None).is_empty());
        assert!(ended("running", None).is_empty());
        assert!(ended("paused", None).is_empty());
    }

    #[test]
    fn gate_escalation_notifies() {
        let run = RunId::generate();
        let fix = task("Fix login");
        let payload =
            json!({ "send_backs": 2, "reason": "gate failed: test, after 2 send-back(s)" });
        let got = rows(
            &record(run, Some("verify"), ESCALATED, payload),
            &fix,
            &attention(),
        );
        assert_eq!(got.len(), 2);
        assert_eq!(
            got[0].payload["text"],
            format!(
                "Fix login: step verify needs you: gate failed: test, after 2 send-back(s). https://sun.example/#/tasks/{}",
                fix.id
            )
        );
    }
}
