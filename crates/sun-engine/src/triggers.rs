//! Mod triggers and run hooks, the kernel's side of the
//! **Plugin** protocol beyond calls:
//!
//! - **Trigger dispatcher** ([`Engine::follow_triggers`]): a mod event
//!   `<mod>.<topic>` that a workflow's `on:` names (`jira.issue_labeled:agent`)
//!   creates a task on that workflow and queues it. The filter after `:`
//!   must equal a top-level string of the payload, or an entry of a
//!   top-level list (`labels`), ignoring ASCII case. One task per workflow
//!   and event: the task and its dedupe key commit in one transaction.
//!   The event's payload is kept with the task, so the run's action steps
//!   can report back to its source (`with: {issue: "${trigger.issue}"}`,
//!   see `outbox::fill`).
//! - **Alerts**: an `alert` trigger (mod-otel) asks the kernel to run
//!   `<mod>.notify(params)`; it goes through the outbox, keyed by
//!   `params.key`, and journals an `alert` event on the run.
//! - **Review corrections**: `pr_review` on a task's `sun/<KEY>-<slug>`
//!   branch records a `reject` (changes requested) or `review_comment`
//!   (a plain comment) correction.
//! - **PR state**: `pr_state` on a task's branch (a PR closed
//!   without merging, or reopened) is journalled on the task's latest run,
//!   where `multi_repo::task_repos` reads it.
//! - **Chat control**: an `approval` (mod-slack's Approve / Reject
//!   buttons on an attention notify) decides the step its `approval_id`
//!   names; a `steer` (a reply in the thread of a notify) steers the run its
//!   message `key` names.
//! - **Cron** ([`Engine::follow_cron`]): each minute, every `cron:<expr>`
//!   trigger due then ([`Engine::fire_cron`], UTC) creates and queues a task,
//!   once per workflow, expression and minute. On boot, a trigger due while
//!   the kernel was down fires once for its latest missed minute
//!   ([`Engine::catch_up_cron`], sun.yaml `cron_catch_up`, default on).
//! - **Webhook** ([`Engine::trigger_webhook`], `POST /workflows/{id}/trigger`):
//!   a workflow with `webhook` in `on:` gets a task per call, once per `key`.
//! - **Failures** ([`Engine::trigger_failed`]): a trigger that fails
//!   is journalled in the audit log as `trigger_failed` and announced to the
//!   `notify_to` channels through the outbox (run-less rows committed with
//!   the audit entry), not just logged.
//! - **Run hooks** ([`Engine::feed_observer`]): a plugin that
//!   provides the action `observe` gets the event log in `seq` order as
//!   `observe {events: [EventRecord]}` batches; an extension gets only the
//!   hook events its `provides.hooks` names ([`hook_of`]), and with
//!   `provides.metrics` a `metrics {run}` call at each run end whose numbers
//!   are journalled as `metric` events. Batches go from a cursor persisted
//!   per mod (`kernel_setting` `mod_cursor:<mod>`), so a restart resumes
//!   where it stopped and a failed call is retried with the same batch.

use std::time::Duration;

use chrono::{DateTime, Datelike, Timelike, Utc};
use serde_json::{Value, json};
use sun_core::model::{
    ActivityKind, AuditAction, CorrectionKind, EventRecord, OutboxItem, OutboxStatus, Permission,
    Priority, Task,
};
use sun_core::redact::redact;
use sun_core::{
    Error, IdemKey, ModCall, ModEvent, OutboxId, Result, RunId, RunStatus, StateMachine, StepKey,
    StepStatus, TaskId, TaskStatus, WorkflowId,
};
use sun_store::{
    AuditRepo, EventLog, NewEvent, OutboxRepo, RunRepo, SettingRepo, TaskRepo, Tx, WorkflowRepo,
};
use sun_workflow::Cron;
use tokio::sync::broadcast::error::RecvError;

use crate::executor::{Triage, next};

/// Setting: the last minute the cron scheduler checked (RFC 3339), so a
/// restart knows which minutes it missed.
const CRON_CHECKED: &str = "cron_checked_at";

/// How far back [`Engine::catch_up_cron`] looks for missed minutes.
pub const CATCH_UP_DAYS: i64 = 7;

/// Whether `cron` is due at `t`'s minute (UTC).
fn cron_matches(cron: &Cron, t: DateTime<Utc>) -> bool {
    cron.matches(
        t.minute(),
        t.hour(),
        t.day(),
        t.month(),
        t.weekday().num_days_from_sunday(),
    )
}

/// The latest minute after `since`'s, up to `now`'s, at which `cron` was due
/// (`now`'s own minute is not checked by the minute loop), looking back at
/// most [`CATCH_UP_DAYS`].
fn last_missed(cron: &Cron, since: DateTime<Utc>, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let floor = since.max(now - chrono::Duration::days(CATCH_UP_DAYS));
    let mut t = now.with_second(0)?.with_nanosecond(0)?;
    let floor = floor.with_second(0)?.with_nanosecond(0)?;
    while t > floor {
        if cron_matches(cron, t) {
            return Some(t);
        }
        t -= chrono::Duration::minutes(1);
    }
    None
}
use crate::executor::tasks;
use crate::outbox::notify_item;
use crate::queue::queue_run;
use crate::{Attention, Engine, Principal};

/// The trigger mod-otel emits to have the kernel notify someone.
pub const ALERT_TRIGGER: &str = "alert";
/// The git host trigger for a submitted PR review.
pub const REVIEW_TRIGGER: &str = "pr_review";
/// A chat button decided an approval:
/// `{approval_id: "<run>/<step>", decision: approved|rejected, user, channel, key}`.
pub const APPROVAL_TRIGGER: &str = "approval";
/// A reply in the thread of a notify message: `{key, text, user, channel, ts}`.
pub const STEER_TRIGGER: &str = "steer";
/// The PR trigger for a PR closed without merging (`state: closed`) or
/// reopened (`state: open`): `{repo, number, branch, state, url}`.
pub const PR_STATE_TRIGGER: &str = "pr_state";
/// Triggers kernel features want from every mod that provides them.
pub const KERNEL_TRIGGERS: [&str; 6] = [
    crate::MERGED_TRIGGER,
    REVIEW_TRIGGER,
    PR_STATE_TRIGGER,
    ALERT_TRIGGER,
    APPROVAL_TRIGGER,
    STEER_TRIGGER,
];
/// Kernel setting prefix: `<prefix><task id>` holds the payload of the event
/// that created the task, read by `${trigger.*}` in action `with:`.
pub(crate) const TRIGGER_OF: &str = "task_trigger:";
/// Event kind of an extension's metric value: `{mod, name, value}`.
pub const METRIC: &str = "metric";
/// Events per `observe` call.
const BATCH: u32 = 200;
/// Wait before retrying a failed `observe`.
const RETRY: Duration = Duration::from_secs(5);
/// Longest task title made from a trigger.
const MAX_TITLE: usize = 200;
/// Longest payload copied into a task body.
const MAX_PAYLOAD: usize = 16 * 1024;

/// What a mod's run-hook feed ([`Engine::feed_observer`]) sends it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Hooks {
    /// `None`: the whole log (a plugin's `observe` action). `Some`: only
    /// these [`hook::EVENTS`] (an extension's `provides.hooks`).
    pub events: Option<Vec<String>>,
    /// Metric names asked with `metrics {run}` at each run end.
    pub metrics: Vec<String>,
}

/// Run-hook names of mod API 1.2. They mirror `sun_mod_sdk::hook`, which
/// sun-engine may not depend on; a test keeps the two equal.
pub mod hook {
    /// The `call` action that gets hook events.
    pub const OBSERVE: &str = "observe";
    /// The `call` action asked for metrics at run end.
    pub const METRICS: &str = "metrics";
    /// A task's first run was queued.
    pub const TASK_CREATED: &str = "task_created";
    /// A step began work.
    pub const STEP_STARTED: &str = "step_started";
    /// A step reached a terminal status.
    pub const STEP_FINISHED: &str = "step_finished";
    /// A gate passed or failed.
    pub const GATE_RESULT: &str = "gate_result";
    /// A run reached a terminal status.
    pub const RUN_FINISHED: &str = "run_finished";
    /// Every hook event.
    pub const EVENTS: [&str; 5] = [
        TASK_CREATED,
        STEP_STARTED,
        STEP_FINISHED,
        GATE_RESULT,
        RUN_FINISHED,
    ];
}

/// The run hook ([`hook::EVENTS`]) a journalled event stands for, if any.
pub fn hook_of(event: &EventRecord) -> Option<&'static str> {
    let field = |k: &str| event.payload[k].as_str().unwrap_or_default();
    match event.kind.as_str() {
        "task_status" if field("from") == TaskStatus::Backlog.as_str() => Some(hook::TASK_CREATED),
        "step_status" if field("to") == StepStatus::Running.as_str() => {
            let from = field("from").parse::<StepStatus>().ok();
            matches!(from, Some(StepStatus::Queued | StepStatus::Dispatched))
                .then_some(hook::STEP_STARTED)
        }
        "step_status" => field("to")
            .parse::<StepStatus>()
            .is_ok_and(StateMachine::is_terminal)
            .then_some(hook::STEP_FINISHED),
        "gate_result" => Some(hook::GATE_RESULT),
        "run_status" => field("to")
            .parse::<RunStatus>()
            .is_ok_and(StateMachine::is_terminal)
            .then_some(hook::RUN_FINISHED),
        _ => None,
    }
}

/// A workflow trigger that names a mod event: `<mod>.<topic>[:<filter>]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ModTrigger {
    pub mod_name: String,
    pub topic: String,
    pub filter: Option<String>,
}

impl ModTrigger {
    /// `None` for `manual`, `webhook` and `cron:` triggers.
    pub(crate) fn parse(text: &str) -> Option<Self> {
        let (event, filter) = match text.split_once(':') {
            Some(("cron", _)) => return None,
            Some((event, filter)) => (event, Some(filter.to_owned())),
            None => (text, None),
        };
        let (mod_name, topic) = event.split_once('.')?;
        Some(Self {
            mod_name: mod_name.to_owned(),
            topic: topic.to_owned(),
            filter,
        })
    }

    fn matches(&self, event: &ModEvent) -> bool {
        if self.mod_name != event.mod_name || self.topic != event.topic {
            return false;
        }
        let Some(filter) = &self.filter else {
            return true;
        };
        let is = |v: &Value| v.as_str().is_some_and(|s| s.eq_ignore_ascii_case(filter));
        event.payload.as_object().is_some_and(|fields| {
            fields.values().any(|v| match v {
                Value::Array(items) => items.iter().any(is),
                v => is(v),
            })
        })
    }
}

/// What identifies one occurrence of an event, for dedupe.
fn occurrence(payload: &Value) -> String {
    let field = |k: &str| payload.get(k).and_then(Value::as_str);
    match (field("url"), field("ts")) {
        (Some(url), _) => url.to_owned(),
        (None, Some(ts)) => format!("{}:{ts}", field("channel").unwrap_or_default()),
        (None, None) => payload.to_string().chars().take(1024).collect(),
    }
}

/// The workflows whose `on:` matches `event`.
async fn matching(tx: &mut Tx, event: &ModEvent) -> Result<Vec<WorkflowId>> {
    let mut workflows = Vec::new();
    for workflow in tx.workflows().await? {
        if let Some(v) = tx.latest_workflow_version(workflow.id).await?
            && workflow_triggers(&v.source_yaml).any(|t| t.matches(event))
        {
            workflows.push(workflow.id);
        }
    }
    Ok(workflows)
}

/// The task a trigger creates: title from `title` or `text`, body with the
/// link and the raw payload.
fn task_text(event: &ModEvent) -> (String, String) {
    let field = |k: &str| event.payload.get(k).and_then(Value::as_str);
    let title = field("title")
        .or_else(|| field("text").and_then(|t| t.lines().next()))
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map_or_else(
            || format!("{} {}", event.mod_name, event.topic),
            str::to_owned,
        );
    let title: String = title.chars().take(MAX_TITLE).collect();
    let mut payload = serde_json::to_string_pretty(&event.payload).unwrap_or_default();
    if payload.len() > MAX_PAYLOAD {
        payload = payload.chars().take(MAX_PAYLOAD).collect();
        payload.push_str("\n…");
    }
    let mut body = format!("From {}.{}", event.mod_name, event.topic);
    if let Some(url) = field("url") {
        body.push_str(&format!(": {url}"));
    }
    if let Some(text) = field("text").filter(|t| t.lines().count() > 1) {
        body.push_str(&format!("\n\n{text}"));
    }
    body.push_str(&format!("\n\n```json\n{payload}\n```"));
    (title, body)
}

impl Engine {
    /// Subscribes each mod to what the kernel wants from it: every workflow
    /// trigger naming it plus [`KERNEL_TRIGGERS`]. Triggers a mod doesn't
    /// provide are skipped (each is subscribed on its own; the mod host
    /// keeps the union). Saving a workflow subscribes its new triggers.
    pub async fn subscribe_triggers(&self, mods: &[String]) -> Result<()> {
        let mut wanted: Vec<(String, String)> = Vec::new();
        for (_, source) in self.workflow_sources().await? {
            wanted.extend(workflow_triggers(&source).map(|t| (t.mod_name, t.topic)));
        }
        for name in mods {
            wanted.extend(KERNEL_TRIGGERS.map(|t| (name.clone(), t.to_owned())));
        }
        wanted.sort();
        wanted.dedup();
        for (mod_name, topic) in wanted {
            if let Err(e) = self.mods.subscribe(&mod_name, vec![topic.clone()]).await
                && !KERNEL_TRIGGERS.contains(&topic.as_str())
            {
                eprintln!("sun: triggers: {mod_name}.{topic}: {e}");
            }
        }
        Ok(())
    }

    /// The latest saved YAML of every workflow.
    async fn workflow_sources(&self) -> Result<Vec<(WorkflowId, String)>> {
        let mut tx = self.store.read().await?;
        let mut out = Vec::new();
        for workflow in tx.workflows().await? {
            if let Some(v) = tx.latest_workflow_version(workflow.id).await? {
                out.push((workflow.id, v.source_yaml));
            }
        }
        Ok(out)
    }

    /// Runs the trigger dispatcher until the mod event stream ends: workflow
    /// triggers, alerts and review corrections. A failure is reported
    /// through [`Self::trigger_failed`] to `attention`'s channels.
    pub async fn follow_triggers(self, attention: Option<Attention>) {
        let mut events = self.mods.events();
        while let Some(event) = next(&mut events).await {
            self.dispatch(&event, attention.as_ref()).await;
        }
    }

    /// Handles one mod event; failures go to [`Self::trigger_failed`].
    pub async fn dispatch(&self, event: &ModEvent, attention: Option<&Attention>) {
        let result = match event.topic.as_str() {
            ALERT_TRIGGER => self.route_alert(event).await.map(drop),
            REVIEW_TRIGGER => self.record_review(event).await.map(drop),
            PR_STATE_TRIGGER => self.record_pr_state(event).await.map(drop),
            APPROVAL_TRIGGER => self.decide_from_chat(event).await.map(drop),
            STEER_TRIGGER => self.steer_from_chat(event).await.map(drop),
            _ => Ok(()),
        };
        let source = format!("{}.{}", event.mod_name, event.topic);
        let actor = format!("mod:{}", event.mod_name);
        if let Err(e) = result {
            self.trigger_failed(&actor, &source, &e, attention).await;
        }
        if let Err(e) = self.start_triggered(event).await {
            self.trigger_failed(&actor, &source, &e, attention).await;
        }
    }

    /// A trigger from `source` (`jira.issue_labeled`, `cron`) failed:
    /// logged, journalled as a `trigger_failed` audit entry by `actor`, and
    /// announced to every `attention` channel.
    pub async fn trigger_failed(
        &self,
        actor: &str,
        source: &str,
        error: &Error,
        attention: Option<&Attention>,
    ) {
        eprintln!("sun: triggers: {source}: {error}");
        let detail = error.to_string();
        // The audit entry and its notices commit together; the outbox
        // delivers the notices (run-less rows, keyed by the entry).
        let journalled = async {
            let mut tx = self.store.begin().await?;
            let entry = tx
                .append_audit(actor, AuditAction::TriggerFailed, source, &detail)
                .await?;
            if let Some(attention) = attention {
                let text = format!(
                    "Sun couldn’t start work from {source}: {detail}. Audit log: {}/#/members",
                    attention.public_url
                );
                for to in &attention.to {
                    let key = IdemKey::outside_run("trigger_failed", &format!("{}:{to}", entry.id));
                    tx.enqueue(&notify_item(None, key, to, &text)).await?;
                }
            }
            tx.commit().await?;
            Ok::<_, Error>(())
        }
        .await;
        match journalled {
            Ok(()) => self.outbox_wake.notify_one(),
            Err(e) => eprintln!("sun: triggers: journalling the failure: {e}"),
        }
    }

    /// Runs `cron:` triggers forever: at the start of every minute,
    /// [`Self::fire_cron`]. With `catch_up`, first [`Self::catch_up_cron`]
    /// for the minutes missed while the kernel was down. Failures go to
    /// [`Self::trigger_failed`].
    pub async fn follow_cron(self, attention: Option<Attention>, catch_up: bool) {
        if catch_up && let Err(e) = self.catch_up_cron(Utc::now()).await {
            self.trigger_failed("cron", "cron", &e, attention.as_ref())
                .await;
        }
        loop {
            let now = Utc::now();
            let into_minute = Duration::from_secs(u64::from(now.second()))
                + Duration::from_nanos(u64::from(now.nanosecond() % 1_000_000_000));
            tokio::time::sleep(Duration::from_secs(60).saturating_sub(into_minute)).await;
            if let Err(e) = self.fire_cron(Utc::now()).await {
                self.trigger_failed("cron", "cron", &e, attention.as_ref())
                    .await;
            }
        }
    }

    /// Creates and queues a task for every `cron:` trigger due at `now`'s
    /// minute (UTC), once per workflow, expression and minute. The clock is
    /// the caller's, so tests pick the time.
    pub async fn fire_cron(&self, now: DateTime<Utc>) -> Result<Vec<TaskId>> {
        self.fire(now, None).await
    }

    /// On boot: every `cron:` trigger that was due while the kernel was down
    /// (since the last minute checked, at most [`CATCH_UP_DAYS`] back) fires
    /// once, for its latest missed minute. Nothing on a first start.
    pub async fn catch_up_cron(&self, now: DateTime<Utc>) -> Result<Vec<TaskId>> {
        let checked = self.store.read().await?.setting(CRON_CHECKED).await?;
        let checked = checked.and_then(|v| serde_json::from_value::<DateTime<Utc>>(v).ok());
        self.fire(now, Some(checked.unwrap_or(now))).await
    }

    /// Fires the triggers due at `now`'s minute or, with `missed_since`, at
    /// a minute after it up to `now`'s; records `now` as checked.
    async fn fire(
        &self,
        now: DateTime<Utc>,
        missed_since: Option<DateTime<Utc>>,
    ) -> Result<Vec<TaskId>> {
        let minute = |t: DateTime<Utc>| t.format("%Y-%m-%dT%H:%MZ").to_string();
        let mut tx = self.store.begin().await?;
        let mut created = Vec::new();
        for workflow in tx.workflows().await? {
            let Some(version) = tx.latest_workflow_version(workflow.id).await? else {
                continue;
            };
            let Ok(parsed) = sun_workflow::parse(&version.source_yaml) else {
                continue;
            };
            for expr in parsed.on.iter().filter_map(|t| t.strip_prefix("cron:")) {
                let Ok(cron) = Cron::parse(expr) else {
                    continue;
                };
                let due = match missed_since {
                    None => cron_matches(&cron, now).then_some(now),
                    Some(since) => last_missed(&cron, since, now),
                };
                let Some(due) = due else { continue };
                let at = minute(due);
                let title = format!("{}: scheduled run", parsed.name);
                let body = match missed_since {
                    None => format!("Started by `cron:{expr}` at {at}."),
                    Some(_) => {
                        format!("Started by `cron:{expr}` for {at}, missed while Sun was down.")
                    }
                };
                let key = format!("trigger:{}:cron:{expr}:{at}", workflow.id);
                let (task, new) =
                    create_once(&mut tx, workflow.id, title, body, &key, None).await?;
                if new {
                    created.push(task);
                }
            }
        }
        tx.put_setting(CRON_CHECKED, &json!(now)).await?;
        tx.commit().await?;
        Ok(created)
    }

    /// Creates and queues a task on `workflow_id` for a webhook call
    /// when the workflow's `on:` has `webhook`. `payload` is
    /// `{title?, text?, url?, repo?, key?, ..}`; a repeated `key` returns the
    /// task it made the first time (`false`).
    pub async fn trigger_webhook(
        &self,
        workflow_id: WorkflowId,
        payload: &Value,
    ) -> Result<(TaskId, bool)> {
        let mut tx = self.store.begin().await?;
        let version = tx
            .latest_workflow_version(workflow_id)
            .await?
            .ok_or_else(|| Error::NotFound(format!("workflow {workflow_id}")))?;
        let parsed = sun_workflow::parse(&version.source_yaml)
            .map_err(|_| Error::InvalidInput("the workflow doesn't parse".into()))?;
        if !parsed.on.iter().any(|t| t == "webhook") {
            return Err(Error::InvalidInput(format!(
                "workflow {} has no webhook trigger: add `webhook` to its `on:`",
                parsed.name
            )));
        }
        let key = match payload.get("key") {
            None => TaskId::generate().to_string(),
            Some(Value::String(k)) if !k.is_empty() && k.len() <= 200 => k.clone(),
            Some(_) => {
                return Err(Error::InvalidInput(
                    "`key` must be a string of 1 to 200 characters".into(),
                ));
            }
        };
        let event = ModEvent {
            mod_name: "webhook".into(),
            topic: parsed.name,
            payload: payload.clone(),
        };
        let (title, body) = task_text(&event);
        let key = format!("trigger:{workflow_id}:webhook:{key}");
        let made = create_once(&mut tx, workflow_id, title, body, &key, Some(payload)).await?;
        tx.commit().await?;
        Ok(made)
    }

    /// Creates and queues a task on every workflow whose `on:` matches the
    /// event, once per event (redelivery creates nothing). The run's repos
    /// come from the task text, like any run (`repo_resolve`).
    /// Tasks, runs and dedupe keys commit in one transaction: a crash leaves
    /// either nothing (the redelivery creates it) or queued tasks.
    ///
    /// With a decision backend, the task is triaged first: its
    /// type (a `Type:` line atop the body) and priority. Without one, the
    /// rules: no type, priority 0. Each answer is journalled on the new run.
    pub async fn start_triggered(&self, event: &ModEvent) -> Result<Vec<TaskId>> {
        if matching(&mut self.store.read().await?, event)
            .await?
            .is_empty()
        {
            return Ok(Vec::new());
        }
        let (title, body) = task_text(event);
        let state = json!({ "title": redact(&title, &[]), "body": redact(&body, &[]) });
        let triage = crate::executor::triage(self.decisions.get(), state).await;
        let mut tx = self.store.begin().await?;
        let created = self.create_triggered(&mut tx, event, &triage).await?;
        tx.commit().await?;
        Ok(created)
    }

    /// [`Engine::start_triggered`] without the triage and the commit.
    async fn create_triggered(
        &self,
        tx: &mut Tx,
        event: &ModEvent,
        triage: &Triage,
    ) -> Result<Vec<TaskId>> {
        let workflows = matching(tx, event).await?;
        if workflows.is_empty() {
            return Ok(Vec::new());
        }
        let (title, mut body) = task_text(event);
        if let Some(kind) = &triage.kind {
            body = format!("Type: {kind}\n\n{body}");
        }
        let mut created = Vec::new();
        for workflow_id in workflows {
            let key = format!(
                "trigger:{workflow_id}:{}.{}:{}",
                event.mod_name,
                event.topic,
                occurrence(&event.payload)
            );
            let (task, new) = create_once(
                tx,
                workflow_id,
                title.clone(),
                body.clone(),
                &key,
                Some(&event.payload),
            )
            .await?;
            if new {
                let priority = Priority::from_level(triage.priority);
                crate::executor::tasks::set_priority(tx, task, priority, None).await?;
                if let Some(run) = tx.runs_of_task(task).await?.pop() {
                    for settled in &triage.settled {
                        settled.journal(tx, run.id, None).await?;
                    }
                }
                created.push(task);
            }
        }
        Ok(created)
    }

    /// Routes an `alert {run_id, mod, action: notify, params: {key, ..}}`
    /// to `<mod>.notify(params)` through the outbox. `false`: already routed.
    /// A mod may only ask for `notify`, never another mod's other actions.
    pub async fn route_alert(&self, event: &ModEvent) -> Result<bool> {
        let p = &event.payload;
        let field = |k: &str| p.get(k).and_then(Value::as_str).unwrap_or_default();
        let target_mod = field("mod");
        let params = p.get("params").cloned().unwrap_or(Value::Null);
        let key = params
            .get("key")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let mod_ok = !target_mod.is_empty()
            && target_mod.len() <= 64
            && target_mod
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
        if field("action") != "notify" || !mod_ok || key.is_empty() || key.len() > 200 {
            return Err(Error::InvalidInput(format!(
                "alert from {}: needs mod, action notify and params.key",
                event.mod_name
            )));
        }
        let run_id: RunId = field("run_id")
            .parse()
            .map_err(|_| Error::InvalidInput("alert: bad run_id".into()))?;
        let mut tx = self.store.begin().await?;
        tx.get_run(run_id).await?;
        let item = OutboxItem {
            id: OutboxId::generate(),
            run_id: Some(run_id),
            idem_key: IdemKey::new(run_id, &StepKey::new("alert")?, key),
            target: format!("{target_mod}.notify"),
            payload: params,
            status: OutboxStatus::Pending,
            result: None,
        };
        if !tx.enqueue(&item).await? {
            return Ok(false);
        }
        tx.append_event(NewEvent::new(
            run_id,
            None,
            "alert",
            json!({ "rule": field("rule"), "from": event.mod_name, "target": item.target }),
        ))
        .await?;
        tx.commit().await?;
        self.outbox_wake.notify_one();
        Ok(true)
    }

    /// A review on a task's branch `sun/<KEY>-<slug>` becomes a correction,
    /// once per review: a change request is a `reject`, a plain
    /// comment (`state: commented`, non-empty body) a `review_comment`.
    /// `false`: not a task's branch, an approval, an empty comment or
    /// one Sun posted itself (it carries an idem marker), or already recorded.
    pub async fn record_review(&self, event: &ModEvent) -> Result<bool> {
        let p = &event.payload;
        let field = |k: &str| p.get(k).and_then(Value::as_str).unwrap_or_default();
        let text = field("body").trim();
        let (kind, verb) = match field("state") {
            "changes_requested" => (CorrectionKind::Reject, "asked for changes"),
            "commented" if !text.is_empty() && !text.contains("<!-- sun-idem:") => {
                (CorrectionKind::ReviewComment, "commented")
            }
            _ => return Ok(false),
        };
        let mut tx = self.store.begin().await?;
        let Some(task_id) = tasks::task_of_branch(&mut tx, field("branch")).await? else {
            return Ok(false);
        };
        let Some(run) = tx.runs_of_task(task_id).await?.pop() else {
            return Ok(false);
        };
        let body = format!("Review by {} {verb}: {text}", field("reviewer"));
        let key = format!("review:{task_id}:{}", occurrence(p));
        let recorded = crate::executor::corrections::record(
            &mut tx,
            run.id,
            None,
            kind,
            None,
            &body,
            Some(&key),
        )
        .await?;
        tx.commit().await?;
        Ok(recorded)
    }

    /// A `pr_state` on a task's branch `sun/<KEY>-<slug>`: journalled as a
    /// `pr_state` event `{repo, number, state}` on the task's latest run,
    /// whose waiting `merge_order` gate checks again (a closed PR fails it).
    /// `false`: not a task's branch, or a task without a run.
    pub async fn record_pr_state(&self, event: &ModEvent) -> Result<bool> {
        let p = &event.payload;
        let field = |k: &str| p.get(k).and_then(Value::as_str).unwrap_or_default();
        let (state, repo) = (field("state"), field("repo"));
        if !matches!(state, "closed" | "open") || repo.is_empty() || repo.len() > 255 {
            return Err(Error::InvalidInput(format!(
                "{}.pr_state: needs repo and state closed or open",
                event.mod_name
            )));
        }
        let mut tx = self.store.begin().await?;
        let Some(task_id) = tasks::task_of_branch(&mut tx, field("branch")).await? else {
            return Ok(false);
        };
        let payload = json!({ "repo": repo, "number": p.get("number").and_then(Value::as_u64), "state": state });
        let run = crate::quality::journal(&mut tx, task_id, PR_STATE_TRIGGER, payload).await?;
        tx.commit().await?;
        // A waiting `merge_order` gate reads it: a closed PR fails it.
        if let Some(run_id) = run {
            self.recheck(run_id).await;
        }
        Ok(run.is_some())
    }

    /// An `approval` from mod `m`'s button decides step `<step>` of run
    /// `<run>` (its `approval_id`) as the Sun member with the chat user's
    /// email, who needs the `approve` permission ([`Self::chat_actor`]): `who:
    /// [user:alice]` matches alice from Slack too, and `who: [role:slack]`
    /// any matched Slack user. Then the clicked message (`key`) is updated
    /// without its buttons. `false`: refused (the user is told) or already
    /// decided.
    pub async fn decide_from_chat(&self, event: &ModEvent) -> Result<bool> {
        let p = &event.payload;
        let field = |k: &str| p.get(k).and_then(Value::as_str).unwrap_or_default();
        let bad = || {
            Error::InvalidInput(format!(
                "{}.approval: needs approval_id <run>/<step>, decision and user",
                event.mod_name
            ))
        };
        let (run, step) = field("approval_id").split_once('/').ok_or_else(bad)?;
        let run_id: RunId = run.parse().map_err(|_| bad())?;
        let step_key = StepKey::new(step).map_err(|_| bad())?;
        let approved = match field("decision") {
            "approved" => true,
            "rejected" => false,
            _ => return Err(bad()),
        };
        let user = field("user");
        if user.is_empty() || user.len() > 64 {
            return Err(bad());
        }
        let occurrence = format!("{}:{}", field("approval_id"), field("decision"));
        let Some(by) = self
            .chat_actor(event, run_id, Permission::Approve, &occurrence)
            .await?
        else {
            return Ok(false);
        };
        match self.decide(run_id, step_key.clone(), approved, by).await {
            Err(Error::InvalidTransition { .. }) => return Ok(false),
            decided => decided?,
        }
        let (key, channel) = (field("key"), field("channel"));
        if key.is_empty() || channel.is_empty() {
            return Ok(true);
        }
        // ponytail: after the decision's commit; a crash in between leaves
        // the buttons up (a second click is a no-op).
        let mut tx = self.store.begin().await?;
        let task_id = tx.get_run(run_id).await?.task_id;
        let task = tx.get_task(task_id).await?;
        let verb = if approved { "approved" } else { "rejected" };
        let item = OutboxItem {
            id: OutboxId::generate(),
            run_id: Some(run_id),
            idem_key: IdemKey::new(run_id, &step_key, &format!("decided:{key}")),
            target: format!("{}.notify", event.mod_name),
            // `<@user>`: the chat's mention syntax (Slack's).
            payload: json!({ "key": key, "channel": channel,
                "text": format!("{}: step {step_key} {verb} by <@{user}>.", task.title) }),
            status: OutboxStatus::Pending,
            result: None,
        };
        tx.enqueue(&item).await?;
        tx.commit().await?;
        self.outbox_wake.notify_one();
        Ok(true)
    }

    /// A `steer` (a reply in the thread of a notify message) steers the run
    /// the message's `key` names (outbox keys start with the run id; alert
    /// keys end with it), once per reply, when the replying chat user's
    /// member has the `run` permission. `false`: the key names no run, the
    /// text is empty, the reply was already handled, or it was refused (the
    /// user is told).
    pub async fn steer_from_chat(&self, event: &ModEvent) -> Result<bool> {
        let p = &event.payload;
        let field = |k: &str| p.get(k).and_then(Value::as_str).unwrap_or_default();
        let run_id = field("key")
            .split(':')
            .find_map(|s| s.parse::<RunId>().ok());
        let text = field("text").trim();
        let Some(run_id) = run_id.filter(|_| !text.is_empty()) else {
            return Ok(false);
        };
        let claim = format!("steer:{}:{}", event.mod_name, occurrence(p));
        let mut tx = self.store.begin().await?;
        if tx.setting(&claim).await?.is_some() {
            return Ok(false);
        }
        tx.put_setting(&claim, &json!(run_id)).await?;
        tx.commit().await?;
        if self
            .chat_actor(event, run_id, Permission::Run, &occurrence(p))
            .await?
            .is_none()
        {
            return Ok(false);
        }
        // ponytail: claimed before steering, so at most once: a failed steer
        // (no agent running) is not retried.
        self.steer(run_id, text.to_owned()).await?;
        Ok(true)
    }

    /// Who chat user `payload.user` of mod `event.mod_name` acts as: their
    /// member with the same `payload.email` when its role has `permission`.
    /// The principal is the member's name and roles plus the role `<mod>`, so
    /// `who: [role:slack]` still admits any matched Slack user. Otherwise
    /// `None`, after queueing a polite reply only that user sees
    /// (`<mod>.ephemeral`, once per `occurrence`).
    async fn chat_actor(
        &self,
        event: &ModEvent,
        run_id: RunId,
        permission: Permission,
        occurrence: &str,
    ) -> Result<Option<Principal>> {
        let p = &event.payload;
        let field = |k: &str| p.get(k).and_then(Value::as_str).unwrap_or_default();
        let (user, channel) = (field("user"), field("channel"));
        let member = self.chat_member(field("email")).await?;
        let refusal = match &member {
            None => format!(
                "Sun doesn’t know who you are yet, so nothing changed. Use the same email in \
                 {} and Sun.",
                event.mod_name
            ),
            Some(m) => {
                let access = self.access_of(m.clone()).await?;
                let workspace = {
                    let mut tx = self.store.read().await?;
                    let task = tx.get_run(run_id).await?.task_id;
                    tx.get_task(task).await?.workspace_id
                };
                if access.can_in(workspace, permission) {
                    let mut principal = access.principal_in(workspace);
                    principal.roles.push(event.mod_name.clone());
                    return Ok(Some(principal));
                }
                let what = match permission {
                    Permission::Approve => "approve",
                    _ => "steer runs",
                };
                format!(
                    "Your Sun role in this workspace can't {what}, so nothing changed. Ask a \
                     workspace admin for a role with the `{permission}` permission."
                )
            }
        };
        if user.is_empty() || channel.is_empty() {
            return Ok(None);
        }
        let mut tx = self.store.begin().await?;
        let action = format!("refused:{user}:{occurrence}");
        let item = OutboxItem {
            id: OutboxId::generate(),
            run_id: Some(run_id),
            idem_key: IdemKey::new(run_id, &StepKey::new("chat")?, &action),
            target: format!("{}.ephemeral", event.mod_name),
            payload: json!({ "channel": channel, "user": user, "text": refusal }),
            status: OutboxStatus::Pending,
            result: None,
        };
        tx.enqueue(&item).await?;
        tx.commit().await?;
        self.outbox_wake.notify_one();
        Ok(None)
    }

    /// Feeds the event log to mod `mod_name` as `hooks` says until the
    /// store closes: catches up from the persisted cursor, then follows new
    /// commits. A failed call is retried with the same batch.
    pub async fn feed_observer(self, mod_name: String, hooks: Hooks) {
        let mut wake = self.store.subscribe();
        loop {
            match self.observe_once(&mod_name, &hooks).await {
                Ok(true) => {}
                Ok(false) => {
                    if let Err(RecvError::Closed) = wake.recv().await {
                        return;
                    }
                }
                Err(e) => {
                    eprintln!("sun: run hooks: {mod_name}.observe: {e}");
                    tokio::time::sleep(RETRY).await;
                }
            }
        }
    }

    /// Sends the next batch after `mod_name`'s cursor (events tagged with
    /// their `hook`, filtered by `hooks.events`), asks for `hooks.metrics`
    /// at each run end in it, then journals the metrics and moves the cursor
    /// past the batch in one transaction. `true` when a full batch was read
    /// (more may wait).
    pub async fn observe_once(&self, mod_name: &str, hooks: &Hooks) -> Result<bool> {
        let key = format!("mod_cursor:{mod_name}");
        let mut read = self.store.read().await?;
        let cursor = read.setting(&key).await?.and_then(|v| v.as_i64());
        let batch = read.events_after(None, cursor.unwrap_or(0), BATCH).await?;
        drop(read);
        let Some(last) = batch.last().map(|e| e.seq) else {
            return Ok(false);
        };
        let full = batch.len() == BATCH as usize;
        let call = |method: &str, params: Value| ModCall {
            mod_name: mod_name.to_owned(),
            method: method.into(),
            params,
            idem_key: None,
        };
        let mut sent = Vec::new();
        let mut ended = Vec::new();
        for event in &batch {
            let hook = hook_of(event);
            if hook == Some(hook::RUN_FINISHED) {
                ended.push(event.run_id);
            }
            let wanted = match &hooks.events {
                None => true,
                Some(names) => hook.is_some_and(|h| names.iter().any(|n| n == h)),
            };
            if wanted {
                let mut value = json!(event);
                if let Some(h) = hook {
                    value["hook"] = json!(h);
                }
                sent.push(value);
            }
        }
        if !sent.is_empty() {
            let params = json!({ "events": sent });
            self.mods.call(call(hook::OBSERVE, params)).await?;
        }
        let mut metrics = Vec::new();
        for run_id in ended.into_iter().filter(|_| !hooks.metrics.is_empty()) {
            let run = self.store.read().await?.get_run(run_id).await?;
            let values = self
                .mods
                .call(call(hook::METRICS, json!({ "run": run })))
                .await?;
            for name in &hooks.metrics {
                // Only declared names with numbers: the mod's reply is untrusted.
                if let Some(value) = values.get(name).filter(|v| v.is_number()) {
                    let payload = json!({ "mod": mod_name, "name": name, "value": value });
                    metrics.push(NewEvent::new(run_id, None, METRIC, payload));
                }
            }
        }
        let mut tx = self.store.begin().await?;
        for event in metrics {
            tx.append_event(event).await?;
        }
        tx.put_setting(&key, &json!(last)).await?;
        tx.commit().await?;
        Ok(full)
    }
}

/// Creates a task on `workflow_id` and queues its run, unless dedupe `key`
/// (a `kernel_setting` holding the task id) already made one: then that
/// task and `false`. In the caller's transaction. The `trigger` payload is
/// kept for the run's action steps (`${trigger.*}`, [`TRIGGER_OF`]).
async fn create_once(
    tx: &mut Tx,
    workflow_id: WorkflowId,
    title: String,
    body: String,
    key: &str,
    trigger: Option<&Value>,
) -> Result<(TaskId, bool)> {
    if let Some(made) = tx.setting(key).await? {
        let task = serde_json::from_value(made)
            .map_err(|e| Error::InvalidInput(format!("setting {key}: {e}")))?;
        return Ok((task, false));
    }
    let workspace = tx.get_workflow(workflow_id).await?.workspace_id;
    let task = Task {
        body,
        workflow_id: Some(workflow_id),
        status: TaskStatus::Backlog,
        ..Task::new(workspace, title)
    };
    tx.insert_task(&task).await?;
    let created = json!({ "status": task.status });
    crate::executor::tasks::log(tx, task.id, ActivityKind::Created, None, created).await?;
    tx.put_setting(key, &json!(task.id)).await?;
    if let Some(payload) = trigger {
        tx.put_setting(&format!("{TRIGGER_OF}{}", task.id), payload)
            .await?;
    }
    queue_run(tx, task.id, "trigger").await?;
    Ok((task.id, true))
}

/// The mod-event triggers of a workflow's YAML (none if it doesn't parse).
fn workflow_triggers(source: &str) -> impl Iterator<Item = ModTrigger> {
    sun_workflow::parse(source)
        .map(|w| w.on)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|t| ModTrigger::parse(&t))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(topic: &str, payload: Value) -> ModEvent {
        ModEvent {
            mod_name: "jira".into(),
            topic: topic.into(),
            payload,
        }
    }

    #[test]
    fn triggers_parse_and_match_filters_against_the_payload() {
        assert_eq!(ModTrigger::parse("manual"), None);
        assert_eq!(ModTrigger::parse("cron:0 9 * * 1"), None);
        let labeled = ModTrigger::parse("jira.issue_labeled:Agent").unwrap();
        assert_eq!(
            (labeled.mod_name.as_str(), labeled.topic.as_str()),
            ("jira", "issue_labeled")
        );
        let hit = event("issue_labeled", json!({"labels": ["bug", "agent"]}));
        assert!(labeled.matches(&hit));
        assert!(!labeled.matches(&event("issue_labeled", json!({"labels": ["bug"]}))));
        assert!(!labeled.matches(&event("card_moved", json!({"labels": ["agent"]}))));
        let any = ModTrigger::parse("jira.issue_labeled").unwrap();
        assert!(any.matches(&event("issue_labeled", json!({}))));
        let list = ModTrigger::parse("jira.card_moved:Doing").unwrap();
        assert!(list.matches(&event("card_moved", json!({"list": "doing"}))));
    }

    #[test]
    fn task_text_uses_title_or_text_and_keeps_the_payload() {
        let e = event(
            "task_requested",
            json!({"text": "fix login\nit 500s", "ts": "1.2", "channel": "C1"}),
        );
        let (title, body) = task_text(&e);
        assert_eq!(title, "fix login");
        assert!(body.contains("it 500s") && body.contains("\"ts\": \"1.2\""));
        assert_eq!(occurrence(&e.payload), "C1:1.2");
        let (title, _) = task_text(&event("card_moved", json!({})));
        assert_eq!(title, "jira card_moved");
    }

    /// A crash before the commit leaves nothing (the redelivered event
    /// creates the task); after it, the task is queued with its run, never
    /// stranded in the backlog.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_triggered_task_is_created_and_queued_in_one_transaction() {
        use sun_core::{HostId, RunStatus};
        use sun_store::Store;

        use crate::gates::tests::{LocalHost, mods};
        use crate::{EngineConfig, WatchdogConfig};

        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).await.unwrap();
        let config = EngineConfig {
            allow_bypass: false,
            host: HostId::generate(),
            adapter: "fake".into(),
            delivery_timeout: Duration::from_secs(5),
            watchdog: WatchdogConfig::default(),
        };
        let host = LocalHost::new(dir.path(), |_, _| {});
        let (engine, _task) = Engine::start(store.clone(), host, mods(json!({})), config)
            .await
            .unwrap();
        let yaml = sun_workflow::DEFAULT_WORKFLOW.replace("on: [manual]", "on: [jira.created]");
        engine.save_workflow(None, &yaml).await.unwrap();
        let created = event(
            "created",
            json!({"title": "Crash me", "url": "https://x/9"}),
        );
        let tasks =
            |store: Store| async move { store.read().await.unwrap().tasks().await.unwrap() };

        // Crash mid-way: the transaction never commits.
        let mut tx = store.begin().await.unwrap();
        assert_eq!(
            engine
                .create_triggered(&mut tx, &created, &Triage::default())
                .await
                .unwrap()
                .len(),
            1
        );
        drop(tx);
        let reopened = Store::open(dir.path()).await.unwrap();
        assert!(
            tasks(reopened.clone()).await.is_empty(),
            "a half-made task survived"
        );

        // Redelivery after the crash creates it, already queued.
        let made = engine.start_triggered(&created).await.unwrap();
        assert_eq!(made.len(), 1);
        let all = tasks(reopened.clone()).await;
        assert_eq!((all.len(), all[0].status), (1, TaskStatus::Todo));
        let runs = reopened
            .read()
            .await
            .unwrap()
            .runs_of_task(made[0])
            .await
            .unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].status, RunStatus::Queued);
        assert!(engine.start_triggered(&created).await.unwrap().is_empty());
    }

    /// A Reject button decides the waiting approval once, as the member with
    /// the Slack user's email, and updates the clicked message; a reply in
    /// its thread steers the run. Unknown users and missing permissions get
    /// a polite ephemeral refusal.
    #[tokio::test(flavor = "multi_thread")]
    async fn slack_reject_rejects_the_waiting_step_once() {
        use sun_core::{HostId, RunStatus, StepStatus};
        use sun_store::Store;

        use crate::gates::tests::{LocalHost, events, mods, seed, steps, until_run_ends};
        use crate::{EngineConfig, WatchdogConfig};

        const REVIEW: &str =
            "name: review\nsteps:\n  - id: review\n    type: approval\n    who: [role:slack]\n";
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).await.unwrap();
        let config = EngineConfig {
            allow_bypass: false,
            host: HostId::generate(),
            adapter: "fake".into(),
            delivery_timeout: Duration::from_secs(5),
            watchdog: WatchdogConfig::default(),
        };
        let host = LocalHost::new(dir.path(), |_, _| {});
        let mods = mods(json!({}));
        let (engine, _task) = Engine::start(store.clone(), host, mods.clone(), config)
            .await
            .unwrap();
        let run = seed(&store, REVIEW).await;
        engine.start_run(run).await.unwrap();
        let waiting = || async {
            let latest = crate::state::latest(steps(&store, run).await);
            latest
                .values()
                .any(|s| s.status == StepStatus::WaitingApproval)
        };
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while !waiting().await {
            assert!(std::time::Instant::now() < deadline, "never waiting");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        // ana (member) and vic (viewer) sign in with the email Slack reports.
        use sun_core::model::Role;
        use sun_store::{MemberRepo, MembershipRepo};
        let mut tx = store.begin().await.unwrap();
        let ana = tx
            .upsert_member("https://idp", "a", "ana", Some("ana@acme.io"), false)
            .await
            .unwrap();
        tx.add_member(sun_core::WorkspaceId::MAIN, ana.id, Role::Member)
            .await
            .unwrap();
        let vic = tx
            .upsert_member("https://idp", "v", "vic", Some("vic@acme.io"), false)
            .await
            .unwrap();
        tx.add_member(sun_core::WorkspaceId::MAIN, vic.id, Role::Viewer)
            .await
            .unwrap();
        tx.commit().await.unwrap();

        let key = format!("{run}:review:attention:7:slack:#eng");
        let click = |mod_name: &str, decision: &str, user: &str, email: &str| ModEvent {
            mod_name: mod_name.into(),
            topic: APPROVAL_TRIGGER.into(),
            payload: json!({"approval_id": format!("{run}/review"), "decision": decision,
                "user": user, "email": email, "channel": "C1", "key": key}),
        };
        let refusals = || async {
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            loop {
                let calls = mods.calls.lock().unwrap().clone();
                let texts: Vec<String> = calls
                    .iter()
                    .filter(|c| c.method == "ephemeral")
                    .map(|c| format!("{} {}", c.params["user"], c.params["text"]))
                    .collect();
                if texts.len() >= 2 || std::time::Instant::now() > deadline {
                    break texts;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        };
        // Unknown Slack user, and a viewer: refused politely, twice-clicked
        // once, nothing moves.
        for _ in 0..2 {
            let stranger = click("slack", "rejected", "U9", "eve@else.io");
            assert!(!engine.decide_from_chat(&stranger).await.unwrap());
        }
        let viewer = click("slack", "rejected", "U7", "vic@acme.io");
        assert!(!engine.decide_from_chat(&viewer).await.unwrap());
        let texts = refusals().await;
        assert_eq!(texts.len(), 2, "{texts:?}");
        assert!(texts[0].contains("U9") && texts[0].contains("doesn’t know who you are"));
        assert!(texts[1].contains("U7") && texts[1].contains("can't approve"));
        // Matched through jira, but not in `who:` (role jira): forbidden.
        assert!(matches!(
            engine
                .decide_from_chat(&click("jira", "rejected", "U42", "ana@acme.io"))
                .await,
            Err(Error::Forbidden(_))
        ));
        let mut bad = click("slack", "maybe", "U42", "ana@acme.io");
        assert!(engine.decide_from_chat(&bad).await.is_err());
        bad.payload["approval_id"] = json!("nope");
        assert!(engine.decide_from_chat(&bad).await.is_err());

        // ana, matched by email (any case), rejects once.
        let ana_click = |decision: &str| click("slack", decision, "U42", "Ana@Acme.io");
        assert!(
            engine
                .decide_from_chat(&ana_click("rejected"))
                .await
                .unwrap()
        );
        assert!(
            !engine
                .decide_from_chat(&ana_click("rejected"))
                .await
                .unwrap()
        );
        assert!(
            !engine
                .decide_from_chat(&ana_click("approved"))
                .await
                .unwrap()
        );
        assert_eq!(until_run_ends(&store, run).await.status, RunStatus::Failed);
        let approvals = events(&store, run, "approval").await;
        assert_eq!(approvals.len(), 1, "{approvals:?}");
        assert_eq!(
            (&approvals[0]["approved"], &approvals[0]["by"]),
            (&json!(false), &json!("ana"))
        );
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let update = loop {
            let calls = mods.calls.lock().unwrap().clone();
            if let Some(c) = calls.into_iter().find(|c| c.method == "notify") {
                break c;
            }
            assert!(std::time::Instant::now() < deadline, "never updated");
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        assert_eq!(update.mod_name, "slack");
        assert_eq!(update.params["key"], json!(key));
        assert_eq!(update.params["text"], "t: step review rejected by <@U42>.");
        assert!(update.params.get("approval_id").is_none(), "buttons gone");

        // A thread reply names the run through the key; once per reply, and
        // only from a member who may run.
        let reply = |user: &str, ts: &str| ModEvent {
            mod_name: "slack".into(),
            topic: STEER_TRIGGER.into(),
            payload: json!({"key": key, "text": "use the v2 api", "user": user,
                "email": "ana@acme.io", "channel": "C1", "ts": ts}),
        };
        let err = engine
            .steer_from_chat(&reply("U42", "9.1"))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no agent is running"), "{err}");
        assert!(!engine.steer_from_chat(&reply("U42", "9.1")).await.unwrap());
        // vic, a viewer, can't steer.
        let mut vic_reply = reply("U7", "9.3");
        vic_reply.payload["email"] = json!("vic@acme.io");
        assert!(!engine.steer_from_chat(&vic_reply).await.unwrap());
        let mut stray = reply("U42", "9.2");
        stray.payload["key"] = json!("someone:else");
        assert!(!engine.steer_from_chat(&stray).await.unwrap());
    }

    /// An engine on a fresh store with recording mods.
    async fn engine(
        dir: &std::path::Path,
    ) -> (
        Engine,
        sun_store::Store,
        std::sync::Arc<crate::gates::tests::Mods>,
    ) {
        use crate::gates::tests::{LocalHost, mods};
        let store = sun_store::Store::open(dir).await.unwrap();
        let config = crate::EngineConfig {
            allow_bypass: false,
            host: sun_core::HostId::generate(),
            adapter: "fake".into(),
            delivery_timeout: Duration::from_secs(5),
            watchdog: crate::WatchdogConfig::default(),
        };
        let mods = mods(json!({}));
        let host = LocalHost::new(dir, |_, _| {});
        let (engine, _) = Engine::start(store.clone(), host, mods.clone(), config)
            .await
            .unwrap();
        (engine, store, mods)
    }

    /// A plain review comment on Sun's PR is a `review_comment` correction,
    /// once; approvals, empty comments and Sun's own notes are not.
    #[tokio::test(flavor = "multi_thread")]
    async fn review_comment_records_a_correction() {
        let dir = tempfile::tempdir().unwrap();
        let (engine, store, _) = engine(dir.path()).await;
        let run = crate::gates::tests::seed(&store, sun_workflow::DEFAULT_WORKFLOW).await;
        let task_id = engine.run(run).await.unwrap().task_id;
        let branch = crate::gates::tests::branch_of(&store, task_id).await;
        let review = |state: &str, body: &str, url: &str| ModEvent {
            mod_name: "gitlab".into(),
            topic: REVIEW_TRIGGER.into(),
            payload: json!({"branch": branch, "state": state,
                "body": body, "reviewer": "ben", "url": url}),
        };
        let comment = review(
            "commented",
            "Rename `userId` to snake_case",
            "https://x/n/1",
        );
        assert!(engine.record_review(&comment).await.unwrap());
        assert!(!engine.record_review(&comment).await.unwrap(), "once");
        for skipped in [
            review("approved", "LGTM", "https://x/n/2"),
            review("commented", "  ", "https://x/n/3"),
            review(
                "commented",
                "Sun opened this.\n\n<!-- sun-idem:k -->",
                "https://x/n/4",
            ),
        ] {
            assert!(!engine.record_review(&skipped).await.unwrap());
        }
        let rows = engine.corrections_of(task_id).await.unwrap();
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(
            (rows[0].kind, rows[0].cause_label),
            (
                CorrectionKind::ReviewComment,
                sun_core::model::CorrectionCause::Style
            )
        );
        assert_eq!(
            rows[0].body,
            "Review by ben commented: Rename `userId` to snake_case"
        );
    }

    /// The scheduler's clock is the caller's: a due minute makes one queued
    /// task per workflow, once; other minutes make none.
    #[tokio::test(flavor = "multi_thread")]
    async fn cron_trigger_creates_a_task_on_schedule() {
        use chrono::TimeZone;
        let dir = tempfile::tempdir().unwrap();
        let (engine, _store, _) = engine(dir.path()).await;
        let yaml = sun_workflow::DEFAULT_WORKFLOW
            .replace("on: [manual]", "on: [manual, \"cron:0 9 * * 1\"]");
        let (workflow, _) = engine.save_workflow(None, &yaml).await.unwrap();

        // 2026-10-05 is a Monday.
        let monday_nine = Utc.with_ymd_and_hms(2026, 10, 5, 9, 0, 30).unwrap();
        let made = engine.fire_cron(monday_nine).await.unwrap();
        assert_eq!(made.len(), 1);
        let task = engine.task(made[0]).await.unwrap();
        assert_eq!(task.workflow_id, Some(workflow.id));
        assert_eq!(task.status, TaskStatus::Todo);
        assert_eq!(task.title, "default: scheduled run");
        assert_eq!(engine.runs_of_task(task.id).await.unwrap().len(), 1);

        // The same minute again (a restart): nothing new.
        let again = monday_nine + chrono::Duration::seconds(20);
        assert!(engine.fire_cron(again).await.unwrap().is_empty());
        let later = monday_nine + chrono::Duration::minutes(1);
        assert!(engine.fire_cron(later).await.unwrap().is_empty());
        let tuesday = monday_nine + chrono::Duration::days(1);
        assert!(engine.fire_cron(tuesday).await.unwrap().is_empty());
        let next_monday = monday_nine + chrono::Duration::days(7);
        assert_eq!(engine.fire_cron(next_monday).await.unwrap().len(), 1);
    }

    /// A trigger due while the kernel was down fires once on boot, for its
    /// latest missed minute; a second boot or the minute loop adds nothing.
    #[tokio::test(flavor = "multi_thread")]
    async fn cron_catches_up_missed_minutes_once_on_boot() {
        use chrono::TimeZone;
        let dir = tempfile::tempdir().unwrap();
        let (engine, _store, _) = engine(dir.path()).await;
        let yaml = sun_workflow::DEFAULT_WORKFLOW
            .replace("on: [manual]", "on: [manual, \"cron:0 9 * * *\"]");
        engine.save_workflow(None, &yaml).await.unwrap();

        // First start: nothing was missed.
        let sunday = Utc.with_ymd_and_hms(2026, 10, 4, 8, 0, 0).unwrap();
        assert!(engine.catch_up_cron(sunday).await.unwrap().is_empty());
        // Down from Sunday 08:00 to Tuesday 10:30: three 09:00s were missed;
        // the latest fires, once.
        let tuesday = Utc.with_ymd_and_hms(2026, 10, 6, 10, 30, 0).unwrap();
        let made = engine.catch_up_cron(tuesday).await.unwrap();
        assert_eq!(made.len(), 1);
        let task = engine.task(made[0]).await.unwrap();
        assert!(
            task.body.contains("2026-10-06T09:00Z, missed"),
            "{}",
            task.body
        );
        assert!(engine.catch_up_cron(tuesday).await.unwrap().is_empty());
        // Booting at 09:00 itself: the minute loop starts at 09:01, so the
        // catch-up fires 09:00; the minute loop then doesn't repeat it.
        let wednesday = Utc.with_ymd_and_hms(2026, 10, 7, 9, 0, 40).unwrap();
        assert_eq!(engine.catch_up_cron(wednesday).await.unwrap().len(), 1);
        assert!(engine.fire_cron(wednesday).await.unwrap().is_empty());
    }

    /// `POST /workflows/{id}/trigger`: only for a workflow with `webhook`,
    /// once per `key`.
    #[tokio::test(flavor = "multi_thread")]
    async fn webhook_trigger_creates_a_task_once_per_key() {
        let dir = tempfile::tempdir().unwrap();
        let (engine, _store, _) = engine(dir.path()).await;
        let (manual, _) = engine
            .save_workflow(None, sun_workflow::DEFAULT_WORKFLOW)
            .await
            .unwrap();
        let hook = json!({"title": "Deploy failed", "key": "build-77"});
        let err = engine.trigger_webhook(manual.id, &hook).await.unwrap_err();
        assert!(err.to_string().contains("no webhook trigger"), "{err}");

        let yaml = sun_workflow::DEFAULT_WORKFLOW
            .replace("name: default", "name: hooked")
            .replace("on: [manual]", "on: [webhook]");
        let (hooked, _) = engine.save_workflow(None, &yaml).await.unwrap();
        let (task, new) = engine.trigger_webhook(hooked.id, &hook).await.unwrap();
        assert!(new);
        assert_eq!(engine.task(task).await.unwrap().title, "Deploy failed");
        assert_eq!(
            engine.trigger_webhook(hooked.id, &hook).await.unwrap(),
            (task, false)
        );
        let (other, new) = engine
            .trigger_webhook(hooked.id, &json!({"title": "x"}))
            .await
            .unwrap();
        assert!(new && other != task);
        assert!(
            engine
                .trigger_webhook(hooked.id, &json!({"key": 7}))
                .await
                .is_err()
        );
    }

    /// A trigger that fails is journalled (`trigger_failed` in the audit
    /// log) and announced on every `notify_to` channel.
    #[tokio::test(flavor = "multi_thread")]
    async fn failed_trigger_notifies() {
        let dir = tempfile::tempdir().unwrap();
        let (engine, _store, mods) = engine(dir.path()).await;
        let attention = Attention {
            public_url: "https://sun.example".into(),
            to: vec!["slack:#ops".into()],
        };
        // A malformed alert from mod-otel: it can't be routed.
        let bad = ModEvent {
            mod_name: "otel".into(),
            topic: ALERT_TRIGGER.into(),
            payload: json!({"rule": "cost", "mod": "slack", "action": "open_pr"}),
        };
        engine.dispatch(&bad, Some(&attention)).await;

        let audit = engine.audit_log(None, 10).await.unwrap();
        let failed: Vec<_> = audit
            .iter()
            .filter(|a| a.action == AuditAction::TriggerFailed)
            .collect();
        assert_eq!(failed.len(), 1, "{audit:?}");
        assert_eq!(
            (failed[0].actor.as_str(), failed[0].target.as_str()),
            ("mod:otel", "otel.alert")
        );
        assert!(failed[0].detail.contains("action notify"), "{failed:?}");
        // The notice is a run-less outbox row, delivered after commit.
        let notices = || {
            let calls = mods.calls.lock().unwrap().clone();
            calls
                .into_iter()
                .filter(|c| c.method == "notify")
                .collect::<Vec<_>>()
        };
        // Wait for the row's last state, not the mod call: the call is seen
        // before the worker commits `delivered` (the flake under load).
        let key = format!("trigger_failed:{}:slack:#ops", failed[0].id);
        let mut delivered = false;
        for _ in 0..1000 {
            let rows = engine.outbox(OutboxStatus::Delivered).await.unwrap();
            delivered = rows
                .iter()
                .any(|row| row.run_id.is_none() && row.idem_key.as_str() == key);
            if delivered {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(delivered, "the notice was never delivered");
        let notify = notices();
        assert_eq!(notify.len(), 1, "{notify:?}");
        assert_eq!(
            notify[0].idem_key.as_ref().map(IdemKey::as_str),
            Some(key.as_str())
        );
        assert_eq!(notify[0].params["key"], key);
        assert_eq!(notify[0].mod_name, "slack");
        assert_eq!(notify[0].params["channel"], "#ops");
        let text = notify[0].params["text"].as_str().unwrap();
        assert!(
            text.starts_with("Sun couldn’t start work from otel.alert: "),
            "{text}"
        );

        // Without notify targets it is still journalled.
        engine.dispatch(&bad, None).await;
        let audit = engine.audit_log(None, 10).await.unwrap();
        assert_eq!(
            audit
                .iter()
                .filter(|a| a.action == AuditAction::TriggerFailed)
                .count(),
            2
        );
    }

    /// A Jira issue assigned to Sun's user starts a task; its run reports
    /// back to that issue: a comment with the PR link, then a transition.
    #[tokio::test(flavor = "multi_thread")]
    async fn jira_task_comments_back_with_pr_url() {
        use crate::gates::tests::until_run_ends;

        const FLOW: &str = r#"
name: jira-fix
on: ["jira.issue_assigned:Sun Bot"]
steps:
  - id: implement
    type: agent
    prompt: fix it
  - id: pr
    type: action
    use: github.open_pr
  - id: report
    type: action
    use: jira.comment
    with: { issue: "${trigger.issue}", body: "Sun opened ${pr.url}" }
  - id: status
    type: action
    use: jira.set_status
    with: { issue: "${trigger.issue}", status: In Review }
"#;
        let pr_url = "https://github.com/acme/web/pull/9";
        let dir = tempfile::tempdir().unwrap();
        let store = sun_store::Store::open(dir.path()).await.unwrap();
        let config = crate::EngineConfig {
            allow_bypass: false,
            host: sun_core::HostId::generate(),
            adapter: "fake".into(),
            delivery_timeout: Duration::from_secs(5),
            watchdog: crate::WatchdogConfig::default(),
        };
        let mods = crate::gates::tests::mods(json!({ "number": 9, "url": pr_url }));
        let host = crate::gates::tests::LocalHost::new(dir.path(), |_, _| {});
        let (engine, _) = Engine::start(store.clone(), host, mods.clone(), config)
            .await
            .unwrap();
        engine.save_workflow(None, FLOW).await.unwrap();

        let assigned = |assignee: &str| {
            event(
                "issue_assigned",
                json!({"issue": "APP-4", "title": "Login 500s", "assignee": assignee,
                    "url": "https://acme.atlassian.net/browse/APP-4"}),
            )
        };
        assert!(
            engine
                .start_triggered(&assigned("Ana"))
                .await
                .unwrap()
                .is_empty(),
            "assigned to someone else"
        );
        let made = engine.start_triggered(&assigned("sun bot")).await.unwrap();
        assert_eq!(made.len(), 1);
        let run = engine.runs_of_task(made[0]).await.unwrap()[0].id;
        engine.start_run(run).await.unwrap();
        let ended = until_run_ends(&store, run).await;
        assert_eq!(
            ended.status,
            sun_core::RunStatus::Succeeded,
            "{:?}",
            ended.end_reason
        );

        let calls = mods.calls.lock().unwrap().clone();
        let jira: Vec<_> = calls.iter().filter(|c| c.mod_name == "jira").collect();
        assert_eq!(jira.len(), 2, "{calls:?}");
        assert_eq!(
            (jira[0].method.as_str(), &jira[0].params),
            (
                "comment",
                &json!({ "issue": "APP-4", "body": format!("Sun opened {pr_url}") })
            )
        );
        assert_eq!(
            (jira[1].method.as_str(), &jira[1].params),
            (
                "set_status",
                &json!({ "issue": "APP-4", "status": "In Review" })
            )
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn triggers_start_tasks_route_alerts_record_reviews_and_feed_observers() {
        use sun_core::HostId;
        use sun_store::Store;

        use crate::gates::tests::{LocalHost, events, mods, seed};
        use crate::{EngineConfig, WatchdogConfig};

        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).await.unwrap();
        let mods = mods(json!({}));
        let config = EngineConfig {
            allow_bypass: false,
            host: HostId::generate(),
            adapter: "fake".into(),
            delivery_timeout: Duration::from_secs(5),
            watchdog: WatchdogConfig::default(),
        };
        let host = LocalHost::new(dir.path(), |_, _| {});
        let (engine, _task) = Engine::start(store.clone(), host, mods.clone(), config)
            .await
            .unwrap();
        let yaml = sun_workflow::DEFAULT_WORKFLOW
            .replace("on: [manual]", "on: [manual, jira.issue_labeled:agent]");
        let (workflow, _) = engine.save_workflow(None, &yaml).await.unwrap();

        // A matching event makes one task, once; others make none.
        let labeled = event(
            "issue_labeled",
            json!({"title": "Fix login", "url": "https://x/1", "labels": ["agent"]}),
        );
        let made = engine.start_triggered(&labeled).await.unwrap();
        assert_eq!(made.len(), 1);
        let task = engine.task(made[0]).await.unwrap();
        assert_eq!(
            (task.title.as_str(), task.workflow_id),
            ("Fix login", Some(workflow.id))
        );
        assert!(task.body.contains("https://x/1"), "{}", task.body);
        assert!(engine.start_triggered(&labeled).await.unwrap().is_empty());
        let other = event(
            "issue_labeled",
            json!({"url": "https://x/2", "labels": ["bug"]}),
        );
        assert!(engine.start_triggered(&other).await.unwrap().is_empty());

        // An alert goes through the outbox to `<mod>.notify`, once.
        let run = seed(&store, sun_workflow::DEFAULT_WORKFLOW).await;
        let alert = ModEvent {
            mod_name: "otel".into(),
            topic: ALERT_TRIGGER.into(),
            payload: json!({"rule": "cost", "run_id": run.to_string(), "mod": "slack",
                "action": "notify", "params": {"key": "alert:cost:1", "channel": "#ops", "text": "over"}}),
        };
        assert!(engine.route_alert(&alert).await.unwrap());
        assert!(!engine.route_alert(&alert).await.unwrap());
        let mut bad = alert.clone();
        bad.payload["action"] = json!("open_pr");
        assert!(engine.route_alert(&bad).await.is_err());
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let notify = loop {
            let calls = mods.calls.lock().unwrap().clone();
            let settled = !events(&store, run, "outbox_delivered").await.is_empty();
            if let Some(c) = calls.into_iter().find(|c| c.method == "notify")
                && settled
            {
                break c;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "alert never delivered"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        assert_eq!(
            (notify.mod_name.as_str(), notify.params["channel"].as_str()),
            ("slack", Some("#ops"))
        );

        // A change request on Sun's branch is a correction, once.
        let task_id = engine.run(run).await.unwrap().task_id;
        let branch = crate::gates::tests::branch_of(&store, task_id).await;
        let review = event(
            REVIEW_TRIGGER,
            json!({"branch": branch, "state": "changes_requested",
                "body": "missing tests for the empty cart", "reviewer": "ana", "url": "https://x/r/1"}),
        );
        assert!(engine.record_review(&review).await.unwrap());
        assert!(!engine.record_review(&review).await.unwrap());
        let rows = engine.corrections_of(task_id).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].kind, CorrectionKind::Reject);
        assert!(rows[0].body.contains("ana"), "{}", rows[0].body);

        // The observer gets the whole log, then only what is new.
        let all = Hooks::default();
        while engine.observe_once("otel", &all).await.unwrap() {}
        let fed = |mods: &crate::gates::tests::Mods| {
            mods.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|c| c.method == hook::OBSERVE)
                .flat_map(|c| c.params["events"].as_array().cloned().unwrap_or_default())
                .map(|e| e["seq"].as_i64().unwrap())
                .collect::<Vec<_>>()
        };
        let first = fed(&mods);
        assert!(!first.is_empty() && first.windows(2).all(|w| w[0] < w[1]));
        assert!(!engine.observe_once("otel", &all).await.unwrap());
        assert_eq!(fed(&mods), first, "nothing new, nothing sent");
        let cursor = store
            .read()
            .await
            .unwrap()
            .setting("mod_cursor:otel")
            .await
            .unwrap();
        assert_eq!(cursor, Some(json!(first.last().unwrap())));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn extension_hook_sees_run_finished() {
        use sun_core::HostId;
        use sun_store::Store;

        use crate::gates::tests::{LocalHost, events, mods, seed, until_run_ends};
        use crate::{EngineConfig, WatchdogConfig};

        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).await.unwrap();
        // Every call answers this: `observe` ignores it, `metrics` reads it.
        let mods = mods(json!({"lines": 42, "undeclared": 1}));
        let config = EngineConfig {
            allow_bypass: false,
            host: HostId::generate(),
            adapter: "fake".into(),
            delivery_timeout: Duration::from_secs(5),
            watchdog: WatchdogConfig::default(),
        };
        let host = LocalHost::new(dir.path(), |ws, _| {
            std::fs::write(ws.join("a"), "a").unwrap()
        });
        let (engine, _task) = Engine::start(store.clone(), host, mods.clone(), config)
            .await
            .unwrap();
        let yaml = "name: lint\nsteps:\n  - id: implement\n    type: agent\n    prompt: go\n  \
                    - id: verify\n    type: gate\n    gates: [diff_not_empty]\n";
        let run = seed(&store, yaml).await;
        engine.start_run(run).await.unwrap();
        assert_eq!(
            until_run_ends(&store, run).await.status,
            RunStatus::Succeeded
        );

        // The engine's hook names are the SDK's (it can't depend on it).
        assert_eq!(hook::EVENTS, sun_mod_sdk::hook::EVENTS);
        assert_eq!(hook::METRICS, sun_mod_sdk::hook::METRICS);
        assert_eq!(hook::OBSERVE, sun_mod_sdk::hook::OBSERVE);
        // An extension with `hooks: [gate_result, run_finished]`, `metrics: [lines]`.
        let hooks = Hooks {
            events: Some(vec![hook::GATE_RESULT.into(), hook::RUN_FINISHED.into()]),
            metrics: vec!["lines".into()],
        };
        let log = store
            .read()
            .await
            .unwrap()
            .events_after(Some(run), 0, 1000)
            .await;
        let mut all: Vec<_> = log.unwrap().iter().filter_map(hook_of).collect();
        all.dedup();
        let expected = [
            "step_started",
            "step_finished",
            "step_started",
            "gate_result",
            "step_finished",
            "run_finished",
        ];
        assert_eq!(all, expected, "each hook maps from the journal");
        while engine.observe_once("lint", &hooks).await.unwrap() {}

        let calls = mods.calls.lock().unwrap().clone();
        let seen: Vec<_> = calls
            .iter()
            .filter(|c| c.method == hook::OBSERVE)
            .flat_map(|c| c.params["events"].as_array().cloned().unwrap_or_default())
            .map(|e| e["hook"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(seen, ["gate_result", "run_finished"], "only declared hooks");
        let asked: Vec<_> = calls.iter().filter(|c| c.method == hook::METRICS).collect();
        assert_eq!(asked.len(), 1);
        assert_eq!(asked[0].params["run"]["id"], json!(run));
        assert_eq!(
            events(&store, run, METRIC).await,
            [json!({"mod": "lint", "name": "lines", "value": 42})]
        );

        // The cursor moved past the metric: nothing is sent or journalled twice.
        while engine.observe_once("lint", &hooks).await.unwrap() {}
        assert_eq!(mods.calls.lock().unwrap().len(), calls.len());
        assert_eq!(events(&store, run, METRIC).await.len(), 1);
    }
}
