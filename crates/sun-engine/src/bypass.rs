//! Who may run a `bypass_permissions` session (BLO-164). A **Chain of
//! Responsibility**, first refusal wins:
//!
//! 1. sun.yaml `runtimes.allow_bypass` off: nobody, the owners included
//!    (refused at placement and start, [`crate::failover::bypass_disabled`]).
//! 2. The owner's [`BypassPolicy`] (Settings → Security, kernel setting
//!    [`POLICY`]): `off`, `members` (an allowlist) or `everyone`.
//! 3. The person the run acts for ([`trigger`]) — a deployment owner always
//!    may. Anyone else not allowed is **capped**: the session runs as
//!    `accept_edits`, journalled `bypass_capped {member, requested,
//!    effective}`, audited, and said once on the task's thread. Never silent.
//! 4. The exec env is a container or VM, at placement.
//! 5. The host refuses bypass on bare metal.
//!
//! The person is whoever last made the run go: the commenter, mover or
//! assigner (each queue path's actor), the retry's caller, the webhook's
//! caller, or the latest person to act on the run since ([`ACTS_FOR`] on
//! the event of a step retry, a resume, a runtime switch, a steer or a
//! message left for the next session; see [`STAMPED`]). What the engine
//! does by itself (a `retry:` policy, a failover, crash recovery) records
//! nothing. Approving a gate doesn't count either: a yes/no carries no
//! instructions, and the approvers are named by whoever saved the workflow.
//! A run no person queued (a cron schedule, a mod event, an eval) acts for
//! the deployment: it runs what was saved, which only the bypass gate's
//! holders could save. The cap applies from the next session on: a steer
//! reaches a live session in the mode it started with.

use serde_json::{Value, json};
use sun_core::model::{ActivityKind, AuditAction, BypassPolicy, OWNER_ISSUER, Run};
use sun_core::{Error, MemberId, PermissionMode, Result, StepKey};
use sun_store::{AuditRepo, EventLog, EventQuery, MemberRepo, RunRepo, SettingRepo, Tx};

use crate::Engine;
use crate::executor::Actor;
use crate::executor::failover::HANDOFF_STARTED;
use crate::executor::kind::{INTERVENTION, RUN_STATUS};
use crate::executor::retry::STEP_RETRY;
use crate::queued::QUEUED;

/// `kernel_setting` key of the [`BypassPolicy`].
const POLICY: &str = "bypass_policy";
/// Journal kind: a session asked for bypass ran capped.
pub(crate) const BYPASS_CAPPED: &str = "bypass_capped";
/// Its key in the first `run_status` event: who queued the run.
const BY: &str = "by";
/// Its key in an event a person's action journals: whom the run acts for
/// from there ([`stamp`]).
pub(crate) const ACTS_FOR: &str = "acts_for";
/// The kinds [`ACTS_FOR`] is read from: a step retry, a resume, a runtime
/// switch, a steer and a message left for the next session.
const STAMPED: [&str; 5] = [
    STEP_RETRY,
    RUN_STATUS,
    HANDOFF_STARTED,
    INTERVENTION,
    QUEUED,
];
/// What a capped session runs as.
const CAPPED_TO: PermissionMode = PermissionMode::AcceptEdits;

/// The `by` of a run's first `run_status` event (or an [`ACTS_FOR`]) for
/// `actor`: their member id, the token holder's [`OWNER_ISSUER`], nothing
/// for the engine.
pub(crate) fn by_of(actor: Option<&Actor>) -> Option<Value> {
    let actor = actor?;
    Some(match actor.member_id {
        Some(member) => json!(member),
        None => json!(OWNER_ISSUER),
    })
}

/// Stamps the `payload` of a [`STAMPED`] event with whom the run acts for
/// from there: `by` (nothing for the engine).
pub(crate) fn stamp(payload: &mut Value, by: Option<&Actor>) {
    if let Some(acts_for) = by_of(by) {
        payload[ACTS_FOR] = acts_for;
    }
}

/// Who `run` acts for: the latest person to act on it ([`stamp`]), else
/// whoever queued it. `None` for the deployment (the token holder, or no
/// person); else the member.
async fn trigger(tx: &mut Tx, run: &Run) -> Result<Option<MemberId>> {
    // The text filter only narrows the scan; the key is read from the payload.
    let stamped = tx
        .search_events(run.id, None, Some(ACTS_FOR), 0, u32::MAX)
        .await?;
    let latest = stamped
        .iter()
        .rev()
        .filter(|e| STAMPED.contains(&e.kind.as_str()))
        .find_map(|e| e.payload.get(ACTS_FOR));
    let by = match latest {
        Some(by) => Some(by.clone()),
        None => {
            let first = tx.events_after(Some(run.id), 0, 1).await?;
            first.first().and_then(|e| e.payload.get(BY)).cloned()
        }
    };
    match by.as_ref().and_then(Value::as_str) {
        None | Some(OWNER_ISSUER) => Ok(None),
        Some(id) => id
            .parse()
            .map(Some)
            .map_err(|_| Error::InvalidInput(format!("run {}: queued by `{id}`", run.id))),
    }
}

/// The saved policy (`off` until an owner saves one).
async fn policy_of(tx: &mut Tx) -> Result<BypassPolicy> {
    match tx.setting(POLICY).await? {
        Some(v) => serde_json::from_value(v).map_err(|e| Error::InvalidInput(e.to_string())),
        None => Ok(BypassPolicy::default()),
    }
}

/// The mode a session of `step` of `run` runs in, and the name of the
/// member capped (step 3): [`crate::agents::step_mode`], with `bypass_permissions` capped
/// to `accept_edits` for a member the policy doesn't allow. With
/// `allow_bypass` off the mode is left as asked, for step 1 to refuse.
pub(crate) async fn session_mode(
    tx: &mut Tx,
    run: &Run,
    workflow: &sun_workflow::Workflow,
    step: &StepKey,
    allow_bypass: bool,
) -> Result<(Option<PermissionMode>, Option<String>)> {
    let asked = crate::agents::step_mode(tx, run.workflow_id, workflow, step).await?;
    if asked != Some(PermissionMode::BypassPermissions) || !allow_bypass {
        return Ok((asked, None));
    }
    let Some(id) = trigger(tx, run).await? else {
        return Ok((asked, None));
    };
    // A member since removed may no longer.
    let member = tx.get_member(id).await.ok();
    let allowed = match &member {
        Some(m) => m.deactivated_at.is_none() && policy_of(tx).await?.allows(m.is_owner, Some(id)),
        None => false,
    };
    let name = member.map_or_else(|| "a removed member".to_owned(), |m| m.username);
    match allowed {
        true => Ok((asked, None)),
        false => Ok((Some(CAPPED_TO), Some(name))),
    }
}

/// Journals and audits a capped session of `step`, and says so on the task's
/// thread the first time in its run.
pub(crate) async fn capped(tx: &mut Tx, step: &sun_core::model::Step, name: &str) -> Result<()> {
    let payload = json!({
        "step_id": step.id,
        "member": name,
        "requested": PermissionMode::BypassPermissions,
        "effective": CAPPED_TO,
    });
    let first = !tx.run_has_event(step.run_id, BYPASS_CAPPED).await?;
    crate::executor::append(tx, step, BYPASS_CAPPED, payload).await?;
    let detail = format!(
        "run {}: bypass_permissions ran as accept_edits",
        step.run_id
    );
    tx.append_audit("sun", AuditAction::PolicyChange, name, &detail)
        .await?;
    if first {
        let run = tx.get_run(step.run_id).await?;
        let text = capped_line(name);
        let on = crate::conversation::thread_of(tx, run.task_id).await?;
        let body = json!({ "text": text, "run_id": step.run_id });
        crate::executor::tasks::log(tx, on, ActivityKind::Comment, None, body).await?;
    }
    Ok(())
}

/// The visible line of a capped session, on the thread and the run log.
pub fn capped_line(member: &str) -> String {
    format!(
        "bypass permissions is not enabled for {member}; ran as Edit files. \
         Owner can enable it in Settings → Security."
    )
}

impl Engine {
    /// The owner's bypass policy (`off` until saved).
    pub async fn bypass_policy(&self) -> Result<BypassPolicy> {
        policy_of(&mut self.store.read().await?).await
    }

    /// Saves the bypass policy, audited as `actor`; every named member must
    /// exist. The saved policy, its member ids deduplicated.
    pub async fn set_bypass_policy(
        &self,
        mut policy: BypassPolicy,
        actor: &str,
    ) -> Result<BypassPolicy> {
        policy.member_ids.sort_by_key(ToString::to_string);
        policy.member_ids.dedup();
        if policy.member_ids.len() > BypassPolicy::MAX_MEMBERS {
            return Err(Error::InvalidInput(format!(
                "At most {} members.",
                BypassPolicy::MAX_MEMBERS
            )));
        }
        let mut tx = self.store.begin().await?;
        for id in &policy.member_ids {
            tx.get_member(*id).await?;
        }
        tx.put_setting(POLICY, &json!(policy)).await?;
        let detail = format!(
            "bypass permissions: {} ({} member(s) listed)",
            json!(policy.mode).as_str().unwrap_or("?"),
            policy.member_ids.len()
        );
        tx.append_audit(actor, AuditAction::SettingsChange, POLICY, &detail)
            .await?;
        tx.commit().await?;
        Ok(policy)
    }

    /// May this person (`is_owner`, `member`) start bypass sessions under
    /// the saved policy? Step 1 (`allow_bypass`) is the caller's.
    pub async fn may_bypass(&self, is_owner: bool, member: Option<MemberId>) -> Result<bool> {
        Ok(self.bypass_policy().await?.allows(is_owner, member))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use std::time::Duration;

    use sun_core::model::BypassMode;
    use sun_core::{HostId, RunId, RunStatus};
    use sun_store::{NewEvent, Store, TaskRepo};

    use super::*;
    use crate::gates::tests::{LocalHost, events, mods, seed, until_run_ends};

    /// Two bypass agent steps in a container.
    const BYPASS: &str = "name: agent
exec_env: { kind: container, image: 'node:22' }
steps:
  - id: plan
    type: agent
    prompt: plan it
    permission_mode: bypass_permissions
  - id: implement
    type: agent
    prompt: fix it
    permission_mode: bypass_permissions
    needs: [plan]
";

    /// An engine on `store` with a local host recording session modes.
    async fn start(
        store: &Store,
        dir: &std::path::Path,
        allow_bypass: bool,
    ) -> (Engine, std::sync::Arc<LocalHost>) {
        let local = LocalHost::new(dir, |_, _| {});
        let config = crate::EngineConfig {
            allow_bypass,
            host: HostId::generate(),
            adapter: "fake".into(),
            delivery_timeout: Duration::from_secs(5),
            watchdog: crate::WatchdogConfig::default(),
        };
        let (engine, _task) = Engine::start(store.clone(), local.clone(), mods(json!({})), config)
            .await
            .unwrap();
        (engine, local)
    }

    /// Who queues the run in each case.
    enum By {
        Engine,
        TokenHolder,
        Owner,
        Member,
    }

    /// Runs [`BYPASS`] queued by `by` under `policy` (the member, `ana`, is
    /// listed when `listed`): the modes its sessions got, the `bypass_capped`
    /// payloads, the task's thread and the audit details.
    async fn run(
        by: By,
        mode: BypassMode,
        listed: bool,
        allow_bypass: bool,
    ) -> (
        RunStatus,
        Vec<Option<String>>,
        Vec<Value>,
        Vec<String>,
        Vec<String>,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).await.unwrap();
        let (engine, local) = start(&store, dir.path(), allow_bypass).await;
        let mut tx = store.begin().await.unwrap();
        let ana = tx
            .upsert_member("local", "ana@sun.test", "ana", Some("ana@sun.test"), false)
            .await
            .unwrap();
        let olu = tx
            .upsert_member("local", "olu@sun.test", "olu", Some("olu@sun.test"), true)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let member_ids = if listed { vec![ana.id] } else { vec![] };
        engine
            .set_bypass_policy(BypassPolicy { mode, member_ids }, "user:olu")
            .await
            .unwrap();
        let run_id = seed(&store, BYPASS).await;
        let actor = |member_id| Actor {
            member_id,
            name: "user:x".into(),
        };
        let by = match by {
            By::Engine => None,
            By::TokenHolder => by_of(Some(&actor(None))),
            By::Owner => by_of(Some(&actor(Some(olu.id)))),
            By::Member => by_of(Some(&actor(Some(ana.id)))),
        };
        let mut queued = json!({ "from": null, "to": "queued", "reason": "created" });
        if let Some(by) = by {
            queued["by"] = by;
        }
        let mut tx = store.begin().await.unwrap();
        tx.append_event(NewEvent::new(run_id, None, "run_status", queued))
            .await
            .unwrap();
        tx.commit().await.unwrap();

        engine.start_run(run_id).await.unwrap();
        let run = until_run_ends(&store, run_id).await;
        let modes = local.modes.lock().unwrap().clone();
        let capped = events(&store, run_id, BYPASS_CAPPED).await;
        let (thread, audit) = thread_and_audit(&store, run_id).await;
        (run.status, modes, capped, thread, audit)
    }

    async fn thread_and_audit(store: &Store, run_id: RunId) -> (Vec<String>, Vec<String>) {
        let mut tx = store.read().await.unwrap();
        let task = tx.get_run(run_id).await.unwrap().task_id;
        let thread = tx.activity_of(task, 0).await.unwrap();
        let thread = thread
            .into_iter()
            .filter(|a| a.kind == ActivityKind::Comment)
            .map(|a| a.body["text"].as_str().unwrap_or_default().to_owned())
            .collect();
        let audit = tx.audit_log(None, 100).await.unwrap();
        let audit = audit
            .into_iter()
            .filter(|a| a.action == AuditAction::PolicyChange)
            .map(|a| format!("{} {}", a.target, a.detail))
            .collect();
        (thread, audit)
    }

    const BYPASSED: [Option<&str>; 2] = [Some("bypass_permissions"), Some("bypass_permissions")];

    fn modes(got: &[Option<String>]) -> Vec<Option<&str>> {
        got.iter().map(Option::as_deref).collect()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn owners_the_token_holder_and_the_engine_always_bypass_even_when_off() {
        for by in [By::Owner, By::TokenHolder, By::Engine] {
            let (status, got, capped, thread, _) = run(by, BypassMode::Off, false, true).await;
            assert_eq!(status, RunStatus::Succeeded);
            assert_eq!(modes(&got), BYPASSED);
            assert!(
                capped.is_empty() && thread.is_empty(),
                "{capped:?} {thread:?}"
            );
        }
    }

    /// `off` caps a member: every session runs as `accept_edits`, each one
    /// journalled and audited, the thread told once per run.
    #[tokio::test(flavor = "multi_thread")]
    async fn off_caps_a_member_with_one_visible_line() {
        let (status, got, capped, thread, audit) =
            run(By::Member, BypassMode::Off, true, true).await;
        assert_eq!(status, RunStatus::Succeeded);
        assert_eq!(
            modes(&got),
            [Some("accept_edits"), Some("accept_edits")],
            "listed is ignored while off"
        );
        assert_eq!(capped.len(), 2, "{capped:?}");
        assert_eq!(capped[0]["member"], "ana");
        assert_eq!(capped[0]["requested"], "bypass_permissions");
        assert_eq!(capped[0]["effective"], "accept_edits");
        assert_eq!(thread, [capped_line("ana")]);
        assert!(thread[0].contains("Settings → Security"), "{thread:?}");
        assert_eq!(audit.len(), 2, "{audit:?}");
        assert!(audit[0].starts_with("ana run "), "{audit:?}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn members_lets_the_listed_and_caps_the_rest_and_everyone_lets_all() {
        let (_, got, capped, ..) = run(By::Member, BypassMode::Members, true, true).await;
        assert_eq!((modes(&got), capped.len()), (BYPASSED.to_vec(), 0));
        let (_, got, capped, ..) = run(By::Member, BypassMode::Members, false, true).await;
        assert_eq!(modes(&got), [Some("accept_edits"), Some("accept_edits")]);
        assert_eq!(capped.len(), 2);
        let (_, got, capped, ..) = run(By::Member, BypassMode::Everyone, false, true).await;
        assert_eq!((modes(&got), capped.len()), (BYPASSED.to_vec(), 0));
    }

    /// sun.yaml `allow_bypass: false` beats the policy: nobody runs bypass,
    /// and nobody is capped into running something else either.
    #[tokio::test(flavor = "multi_thread")]
    async fn allow_bypass_off_beats_the_policy_for_owners_and_members() {
        for by in [By::Owner, By::Member] {
            let (status, got, capped, ..) = run(by, BypassMode::Everyone, true, false).await;
            assert_eq!((status, got.len(), capped.len()), (RunStatus::Failed, 0, 0));
        }
    }

    /// `POST /workflows/{id}/trigger` acts for its caller: under `off` a
    /// member's webhook run of a bypass workflow is capped, said on the
    /// thread and audited. A webhook with no caller acts for the deployment.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_webhook_run_acts_for_its_caller() {
        let hooked = BYPASS.replace("name: agent\n", "name: agent\non: [webhook]\n");
        for (member, want) in [(true, "accept_edits"), (false, "bypass_permissions")] {
            let dir = tempfile::tempdir().unwrap();
            let store = Store::open(dir.path()).await.unwrap();
            let (engine, local) = start(&store, dir.path(), true).await;
            let mut tx = store.begin().await.unwrap();
            let ana = tx
                .upsert_member("local", "ana@sun.test", "ana", Some("ana@sun.test"), false)
                .await
                .unwrap();
            tx.commit().await.unwrap();
            let (workflow, _) = engine.save_workflow(None, &hooked).await.unwrap();
            let by = Actor {
                member_id: Some(ana.id),
                name: "user:ana".into(),
            };
            let payload = json!({ "title": "deploy failed" });
            let (task, _) = engine
                .trigger_webhook(workflow.id, &payload, member.then_some(&by))
                .await
                .unwrap();
            let run_id = {
                let mut tx = store.read().await.unwrap();
                tx.runs_of_task(task).await.unwrap().pop().unwrap().id
            };
            engine.start_run(run_id).await.unwrap();
            let run = until_run_ends(&store, run_id).await;
            assert_eq!(run.status, RunStatus::Succeeded);
            let got = local.modes.lock().unwrap().clone();
            assert_eq!(modes(&got), [Some(want), Some(want)]);
            let capped = events(&store, run_id, BYPASS_CAPPED).await;
            let (thread, audit) = thread_and_audit(&store, run_id).await;
            match member {
                true => {
                    assert_eq!(capped.len(), 2, "{capped:?}");
                    assert_eq!(thread, [capped_line("ana")]);
                    assert_eq!(audit.len(), 2, "{audit:?}");
                }
                false => assert!(capped.is_empty() && thread.is_empty() && audit.is_empty()),
            }
        }
    }

    /// A step retry makes the run act for its retrier: under `off` a member
    /// retrying the failed gate of the owner's run gets the bypass step
    /// capped, and the owner retrying a member's run gets it uncapped.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_step_retry_acts_for_its_retrier() {
        const FLAKY: &str = "name: flaky
exec_env: { kind: container, image: 'node:22' }
steps:
  - id: check
    type: gate
    gates: [{test: {command: \"test -f ok || { touch ok; false; }\"}}]
  - id: implement
    type: agent
    prompt: fix it
    permission_mode: bypass_permissions
    needs: [check]
";
        for (owner_queues, want) in [(true, "accept_edits"), (false, "bypass_permissions")] {
            let dir = tempfile::tempdir().unwrap();
            let store = Store::open(dir.path()).await.unwrap();
            let (engine, local) = start(&store, dir.path(), true).await;
            let mut tx = store.begin().await.unwrap();
            let ana = tx
                .upsert_member("local", "ana@sun.test", "ana", Some("ana@sun.test"), false)
                .await
                .unwrap();
            let olu = tx
                .upsert_member("local", "olu@sun.test", "olu", Some("olu@sun.test"), true)
                .await
                .unwrap();
            tx.commit().await.unwrap();
            let actor = |member_id: MemberId, name: &str| Actor {
                member_id: Some(member_id),
                name: format!("user:{name}"),
            };
            let (queuer, retrier) = match owner_queues {
                true => (actor(olu.id, "olu"), actor(ana.id, "ana")),
                false => (actor(ana.id, "ana"), actor(olu.id, "olu")),
            };
            let run_id = seed(&store, FLAKY).await;
            let mut queued = json!({ "from": null, "to": "queued", "reason": "created" });
            queued["by"] = by_of(Some(&queuer)).unwrap();
            let mut tx = store.begin().await.unwrap();
            tx.append_event(NewEvent::new(run_id, None, "run_status", queued))
                .await
                .unwrap();
            tx.commit().await.unwrap();

            engine.start_run(run_id).await.unwrap();
            let run = until_run_ends(&store, run_id).await;
            assert_eq!(run.status, RunStatus::Failed, "the gate fails once");
            let check = StepKey::new("check").unwrap();
            engine.retry_step(run_id, check, retrier).await.unwrap();
            let run = until_run_ends(&store, run_id).await;
            assert_eq!(run.status, RunStatus::Succeeded);
            let got = local.modes.lock().unwrap().clone();
            assert_eq!(modes(&got), [Some(want)], "owner queued: {owner_queues}");
            let capped = events(&store, run_id, BYPASS_CAPPED).await;
            assert_eq!(capped.len(), usize::from(owner_queues), "{capped:?}");
        }
    }

    /// The owner's run of [`BYPASS`] under `off`, its `plan` session live
    /// and held (`LocalHost::hold`) for member `ana` to act on.
    struct Held {
        _dir: tempfile::TempDir,
        store: Store,
        engine: Engine,
        local: std::sync::Arc<LocalHost>,
        run_id: RunId,
        ana: Actor,
    }

    async fn owners_run_held(yaml: &str) -> Held {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).await.unwrap();
        let (engine, local) = start(&store, dir.path(), true).await;
        let mut tx = store.begin().await.unwrap();
        let ana = tx
            .upsert_member("local", "ana@sun.test", "ana", Some("ana@sun.test"), false)
            .await
            .unwrap();
        let olu = tx
            .upsert_member("local", "olu@sun.test", "olu", Some("olu@sun.test"), true)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let actor = |member_id: MemberId, name: &str| Actor {
            member_id: Some(member_id),
            name: format!("user:{name}"),
        };
        let run_id = seed(&store, yaml).await;
        let mut queued = json!({ "from": null, "to": "queued", "reason": "created" });
        queued["by"] = by_of(Some(&actor(olu.id, "olu"))).unwrap();
        let mut tx = store.begin().await.unwrap();
        tx.append_event(NewEvent::new(run_id, None, "run_status", queued))
            .await
            .unwrap();
        tx.commit().await.unwrap();
        local.hold.store(true, std::sync::atomic::Ordering::SeqCst);
        engine.start_run(run_id).await.unwrap();
        let plan = StepKey::new("plan").unwrap();
        for _ in 0..2000 {
            let mut tx = store.read().await.unwrap();
            let steps = crate::state::latest(tx.steps_of_run(run_id).await.unwrap());
            if steps.get(&plan).map(|s| s.status) == Some(sun_core::StepStatus::Running) {
                return Held {
                    _dir: dir,
                    store,
                    engine,
                    local,
                    run_id,
                    ana: actor(ana.id, "ana"),
                };
            }
            drop(tx);
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("`plan` never ran");
    }

    impl Held {
        fn release(&self) {
            self.local
                .hold
                .store(false, std::sync::atomic::Ordering::SeqCst);
        }

        /// Ends the run: the modes its sessions got and its `bypass_capped`
        /// events, the thread told once.
        async fn finish(&self) -> (Vec<Option<String>>, Vec<Value>) {
            let run = until_run_ends(&self.store, self.run_id).await;
            assert_eq!(run.status, RunStatus::Succeeded, "{:?}", run.end_reason);
            let modes = self.local.modes.lock().unwrap().clone();
            let capped = events(&self.store, self.run_id, BYPASS_CAPPED).await;
            let (thread, _) = thread_and_audit(&self.store, self.run_id).await;
            assert_eq!(thread, [capped_line("ana")]);
            (modes, capped)
        }
    }

    /// BLO-169: a member resuming the owner's paused bypass run under `off`
    /// gets every session from there capped.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_resume_acts_for_its_resumer() {
        let h = owners_run_held(BYPASS).await;
        h.engine.pause(h.run_id).await.unwrap();
        h.release();
        h.engine.resume(h.run_id, h.ana.clone()).await.unwrap();
        let (got, capped) = h.finish().await;
        let capped_twice = [Some("accept_edits"), Some("accept_edits")];
        assert_eq!(
            modes(&got),
            [&[Some("bypass_permissions")][..], &capped_twice].concat()
        );
        assert_eq!(capped.len(), 2, "{capped:?}");
        let resumed = events(&h.store, h.run_id, "run_status").await;
        let stamped: Vec<_> = resumed.iter().filter_map(|e| e.get(ACTS_FOR)).collect();
        assert_eq!(
            stamped,
            [&json!(h.ana.member_id)],
            "only the person's resume"
        );
    }

    /// BLO-169: a member switching the owner's live bypass step to another
    /// runtime under `off` gets the new session, and the next, capped.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_runtime_switch_acts_for_its_switcher() {
        let chained = BYPASS.replace("steps:\n", "runtime: { chain: [fake, spare] }\nsteps:\n");
        let h = owners_run_held(&chained).await;
        let plan = StepKey::new("plan").unwrap();
        h.engine
            .switch_runtime(h.run_id, plan, "spare".into(), h.ana.clone())
            .await
            .unwrap();
        h.release();
        let (got, capped) = h.finish().await;
        let capped_twice = [Some("accept_edits"), Some("accept_edits")];
        assert_eq!(
            modes(&got),
            [&[Some("bypass_permissions")][..], &capped_twice].concat()
        );
        assert_eq!(capped.len(), 2, "{capped:?}");
    }

    /// BLO-169: a member steering the owner's live bypass session under
    /// `off` gets the run's next session capped. The live one keeps the mode
    /// it started with.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_steer_acts_for_its_steerer() {
        let h = owners_run_held(BYPASS).await;
        h.engine
            .steer(h.run_id, "use tabs".into(), h.ana.clone())
            .await
            .unwrap();
        h.release();
        let (got, capped) = h.finish().await;
        assert_eq!(
            modes(&got),
            [Some("bypass_permissions"), Some("accept_edits")]
        );
        assert_eq!(capped.len(), 1, "{capped:?}");
    }

    /// BLO-169: so does a message a member leaves for the next session (a
    /// comment when no agent is live falls back to this).
    #[tokio::test(flavor = "multi_thread")]
    async fn a_queued_message_acts_for_its_author() {
        let h = owners_run_held(BYPASS).await;
        h.engine
            .queue_message(h.run_id, "use tabs".into(), h.ana.clone())
            .await
            .unwrap();
        h.release();
        let (got, capped) = h.finish().await;
        assert_eq!(
            modes(&got),
            [Some("bypass_permissions"), Some("accept_edits")]
        );
        assert_eq!(capped.len(), 1, "{capped:?}");
    }

    #[tokio::test]
    async fn a_policy_naming_an_unknown_member_is_refused_and_ids_deduplicate() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).await.unwrap();
        let (engine, _) = start(&store, dir.path(), true).await;
        assert_eq!(
            engine.bypass_policy().await.unwrap(),
            BypassPolicy::default()
        );
        let ghost = BypassPolicy {
            mode: BypassMode::Members,
            member_ids: vec![MemberId::generate()],
        };
        assert!(engine.set_bypass_policy(ghost, "user:olu").await.is_err());
        let mut tx = store.begin().await.unwrap();
        let ana = tx
            .upsert_member("local", "ana@sun.test", "ana", Some("ana@sun.test"), false)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let twice = BypassPolicy {
            mode: BypassMode::Members,
            member_ids: vec![ana.id, ana.id],
        };
        let saved = engine.set_bypass_policy(twice, "user:olu").await.unwrap();
        assert_eq!(saved.member_ids, [ana.id]);
        assert_eq!(engine.bypass_policy().await.unwrap(), saved);
        assert!(engine.may_bypass(false, Some(ana.id)).await.unwrap());
        assert!(
            !engine
                .may_bypass(false, Some(MemberId::generate()))
                .await
                .unwrap()
        );
    }
}
