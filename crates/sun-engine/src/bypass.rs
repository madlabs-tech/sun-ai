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
    use sun_store::{MembershipRepo, NewEvent, Store, TaskRepo};

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
        olu: Actor,
    }

    async fn owners_run_held(yaml: &str) -> Held {
        run_held(yaml, true).await
    }

    /// [`owners_run_held`], queued by the owner `olu` or else by `ana`, a
    /// member of Main (so a chat reply can match her).
    async fn run_held(yaml: &str, owner_queues: bool) -> Held {
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
        tx.add_member(
            sun_core::WorkspaceId::MAIN,
            ana.id,
            sun_core::model::Role::Member,
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        let actor = |member_id: MemberId, name: &str| Actor {
            member_id: Some(member_id),
            name: format!("user:{name}"),
        };
        let (ana, olu) = (actor(ana.id, "ana"), actor(olu.id, "olu"));
        let run_id = seed(&store, yaml).await;
        let mut queued = json!({ "from": null, "to": "queued", "reason": "created" });
        let queuer = if owner_queues { &olu } else { &ana };
        queued["by"] = by_of(Some(queuer)).unwrap();
        let mut tx = store.begin().await.unwrap();
        tx.append_event(NewEvent::new(run_id, None, "run_status", queued))
            .await
            .unwrap();
        tx.commit().await.unwrap();
        local.hold.store(true, std::sync::atomic::Ordering::SeqCst);
        engine.start_run(run_id).await.unwrap();
        let held = Held {
            _dir: dir,
            store,
            engine,
            local,
            run_id,
            ana,
            olu,
        };
        held.until_running("plan").await;
        held
    }

    impl Held {
        fn release(&self) {
            self.local
                .hold
                .store(false, std::sync::atomic::Ordering::SeqCst);
        }

        /// Lets the one held session go on; the next one is held again.
        fn pass_one(&self) {
            self.local
                .passes
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }

        /// Waits until step `key`'s latest attempt runs (its session live).
        async fn until_running(&self, key: &str) {
            self.until(key, sun_core::StepStatus::Running).await;
        }

        /// Waits until step `key`'s latest attempt is `status`.
        async fn until(&self, key: &str, status: sun_core::StepStatus) {
            let key = StepKey::new(key).unwrap();
            for _ in 0..2000 {
                let mut tx = self.store.read().await.unwrap();
                let steps = crate::state::latest(tx.steps_of_run(self.run_id).await.unwrap());
                if steps.get(&key).map(|s| s.status) == Some(status) {
                    return;
                }
                drop(tx);
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            panic!("`{key}` never {status}");
        }

        /// Ends the run: the modes its sessions got and its `bypass_capped`
        /// events, the thread told once.
        async fn finish(&self) -> (Vec<Option<String>>, Vec<Value>) {
            let (modes, capped, thread) = self.end().await;
            assert_eq!(thread, [capped_line("ana")]);
            (modes, capped)
        }

        /// [`Self::finish`] that leaves the thread to the caller.
        async fn end(&self) -> (Vec<Option<String>>, Vec<Value>, Vec<String>) {
            let run = until_run_ends(&self.store, self.run_id).await;
            assert_eq!(run.status, RunStatus::Succeeded, "{:?}", run.end_reason);
            let modes = self.local.modes.lock().unwrap().clone();
            let capped = events(&self.store, self.run_id, BYPASS_CAPPED).await;
            let (thread, _) = thread_and_audit(&self.store, self.run_id).await;
            (modes, capped, thread)
        }

        /// `acts_for` of every event of the run that has one, in order.
        async fn stamps(&self) -> Vec<(String, Value)> {
            let mut tx = self.store.read().await.unwrap();
            let all = tx.events_after(Some(self.run_id), 0, 10_000).await.unwrap();
            all.into_iter()
                .filter_map(|e| Some((e.kind, e.payload.get(ACTS_FOR)?.clone())))
                .collect()
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

    // ------------------------------------------------ BLO-167 QA pass

    use sun_core::model::{Assignee, Priority};
    use sun_core::{AgentId, TaskId, TaskStatus, WorkspaceId};

    use crate::{AgentFields, NewTask, WorkflowPick};

    /// A [`BYPASS`]-shaped workflow every queue path can start: a webhook
    /// and the board.
    const HOOKED: &str = "name: hooked
on: [manual, webhook]
exec_env: { kind: container, image: 'node:22' }
steps:
  - id: implement
    type: agent
    prompt: fix it
    permission_mode: bypass_permissions
";

    /// One engine over a [`LocalHost`] that records each session's mode,
    /// with `ana` (a member) and `olu` (a deployment owner).
    struct World {
        engine: Engine,
        store: Store,
        local: Arc<LocalHost>,
        ana: sun_core::model::Member,
        olu: sun_core::model::Member,
        _dir: tempfile::TempDir,
    }

    use std::sync::Arc;

    async fn world(policy: impl FnOnce(MemberId) -> BypassPolicy, allow_bypass: bool) -> World {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).await.unwrap();
        let local = LocalHost::new(dir.path(), |_, _| {});
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
        engine
            .set_bypass_policy(policy(ana.id), "user:olu")
            .await
            .unwrap();
        World {
            engine,
            store,
            local,
            ana,
            olu,
            _dir: dir,
        }
    }

    fn actor_of(member: &sun_core::model::Member) -> Option<Actor> {
        Some(Actor {
            member_id: Some(member.id),
            name: format!("user:{}", member.username),
        })
    }

    impl World {
        /// Starts the task's latest run (no queue pump here) and waits for
        /// its end: the modes of the sessions it started and its
        /// `bypass_capped` payloads.
        async fn run_latest(&self, task: TaskId) -> (RunStatus, Vec<Option<String>>, Vec<Value>) {
            let before = self.local.modes.lock().unwrap().len();
            let run = {
                let mut tx = self.store.read().await.unwrap();
                tx.runs_of_task(task)
                    .await
                    .unwrap()
                    .pop()
                    .expect("a queued run")
            };
            assert_eq!(run.status, RunStatus::Queued, "the path queued no new run");
            self.engine.start_run(run.id).await.unwrap();
            let ended = until_run_ends(&self.store, run.id).await;
            let modes = self.local.modes.lock().unwrap()[before..].to_vec();
            let capped = events(&self.store, run.id, BYPASS_CAPPED).await;
            (ended.status, modes, capped)
        }

        async fn task_by(&self, by: &sun_core::model::Member, pick: WorkflowPick) -> TaskId {
            let new = NewTask {
                workspace_id: Some(WorkspaceId::MAIN),
                title: "Fix the login".into(),
                body: String::new(),
                workflow: pick,
                exec_env: None,
                priority: Priority::None,
                status: TaskStatus::Todo,
                project_id: None,
                actor: actor_of(by),
            };
            self.engine.create_task(new).await.unwrap().id
        }

        async fn bypass_agent(&self) -> AgentId {
            let fields = AgentFields {
                name: Some("Rex".into()),
                instructions: Some("You are Rex.".into()),
                runtime_chain: Some(vec!["fake".into()]),
                permission_mode: Some(Some(PermissionMode::BypassPermissions)),
                ..AgentFields::default()
            };
            let plan = sun_core::entitlement::Entitlement::community(
                sun_core::entitlement::EntitlementStatus::Community,
            );
            self.engine
                .create_agent(WorkspaceId::MAIN, fields, "user:olu", &plan)
                .await
                .unwrap()
                .id
        }

        async fn hooked(&self) -> sun_core::WorkflowId {
            self.engine.save_workflow(None, HOOKED).await.unwrap().0.id
        }
    }

    fn off(_: MemberId) -> BypassPolicy {
        BypassPolicy::default()
    }

    const CAPPED: [Option<&str>; 1] = [Some("accept_edits")];
    const BYPASS_ONE: [Option<&str>; 1] = [Some("bypass_permissions")];

    /// Case 1: the owner, the token holder and the engine bypass under every
    /// mode, `members` with nobody listed included.
    #[tokio::test(flavor = "multi_thread")]
    async fn qa_owners_bypass_under_off_members_and_everyone() {
        for mode in [BypassMode::Off, BypassMode::Members, BypassMode::Everyone] {
            for by in [By::Owner, By::TokenHolder, By::Engine] {
                let (status, got, capped, thread, audit) = run(by, mode, false, true).await;
                assert_eq!(status, RunStatus::Succeeded, "{mode:?}");
                assert_eq!(modes(&got), BYPASSED, "{mode:?}");
                assert!(capped.is_empty() && thread.is_empty() && audit.is_empty());
            }
        }
    }

    /// Case 2: the cap's journal, audit and thread line carry the member's
    /// name and the two modes, nothing else: no email, no token.
    #[tokio::test(flavor = "multi_thread")]
    async fn qa_the_cap_payload_names_the_member_and_leaks_nothing() {
        let (_, _, capped, thread, audit) = run(By::Member, BypassMode::Off, false, true).await;
        assert_eq!(capped.len(), 2, "one per session: {capped:?}");
        for payload in &capped {
            let mut keys: Vec<_> = payload.as_object().unwrap().keys().cloned().collect();
            keys.sort();
            assert_eq!(keys, ["effective", "member", "requested", "step_id"]);
        }
        let all = format!("{capped:?} {thread:?} {audit:?}");
        for secret in ["ana@sun.test", "@", "token", "session-"] {
            assert!(!all.contains(secret), "`{secret}` in {all}");
        }
    }

    /// Case 2: the thread is told once per run, not once per task: a second
    /// capped run of the same task adds a second line.
    #[tokio::test(flavor = "multi_thread")]
    async fn qa_each_capped_run_says_so_once_on_the_thread() {
        let w = world(off, true).await;
        let task = w.task_by(&w.ana, WorkflowPick::Id(w.hooked().await)).await;
        let (_, got, _) = w.run_latest(task).await;
        assert_eq!(modes(&got), CAPPED);
        w.engine
            .move_task_manually(task, TaskStatus::Todo, actor_of(&w.ana))
            .await
            .unwrap();
        let (_, got, _) = w.run_latest(task).await;
        assert_eq!(modes(&got), CAPPED);
        let mut tx = w.store.read().await.unwrap();
        let lines = tx
            .activity_of(task, 0)
            .await
            .unwrap()
            .into_iter()
            .filter(|a| a.body["text"] == capped_line("ana"))
            .count();
        assert_eq!(lines, 2);
    }

    /// Case 3: a member taken off the allowlist, or listed and then
    /// deactivated, is capped on their next run.
    #[tokio::test(flavor = "multi_thread")]
    async fn qa_unlisting_or_deactivating_a_listed_member_caps_them() {
        let listed = |ana| BypassPolicy {
            mode: BypassMode::Members,
            member_ids: vec![ana],
        };
        let w = world(listed, true).await;
        let hooked = w.hooked().await;
        let task = w.task_by(&w.ana, WorkflowPick::Id(hooked)).await;
        let (_, got, _) = w.run_latest(task).await;
        assert_eq!(modes(&got), BYPASS_ONE, "listed");

        let unlisted = BypassPolicy {
            mode: BypassMode::Members,
            member_ids: vec![],
        };
        w.engine
            .set_bypass_policy(unlisted, "user:olu")
            .await
            .unwrap();
        let task = w.task_by(&w.ana, WorkflowPick::Id(hooked)).await;
        let (_, got, capped) = w.run_latest(task).await;
        assert_eq!(
            (modes(&got), capped.len()),
            (CAPPED.to_vec(), 1),
            "unlisted"
        );

        w.engine
            .set_bypass_policy(listed(w.ana.id), "user:olu")
            .await
            .unwrap();
        let task = w.task_by(&w.ana, WorkflowPick::Id(hooked)).await;
        let mut tx = w.store.begin().await.unwrap();
        assert!(tx.set_member_active(w.ana.id, false).await.unwrap());
        tx.commit().await.unwrap();
        let (_, got, capped) = w.run_latest(task).await;
        assert_eq!(modes(&got), CAPPED, "listed but deactivated");
        assert_eq!(capped[0]["member"], "ana");
    }

    /// Case 3: a run queued by someone the store no longer knows is capped
    /// and named as removed; a `by` that is no id fails the session rather
    /// than run uncapped.
    #[tokio::test(flavor = "multi_thread")]
    async fn qa_an_unknown_or_garbled_by_never_bypasses() {
        let everyone = |_| BypassPolicy {
            mode: BypassMode::Everyone,
            member_ids: vec![],
        };
        let w = world(everyone, true).await;
        for (by, ok) in [
            (json!(MemberId::generate()), true),
            (json!("not-an-id"), false),
        ] {
            let run_id = seed(&w.store, BYPASS).await;
            let queued = json!({ "from": null, "to": "queued", "reason": "created", "by": by });
            let mut tx = w.store.begin().await.unwrap();
            tx.append_event(NewEvent::new(run_id, None, "run_status", queued))
                .await
                .unwrap();
            tx.commit().await.unwrap();
            let started = w.engine.start_run(run_id).await;
            if ok {
                started.unwrap();
                let run = until_run_ends(&w.store, run_id).await;
                assert_eq!(run.status, RunStatus::Succeeded);
                let capped = events(&w.store, run_id, BYPASS_CAPPED).await;
                assert_eq!(capped[0]["member"], "a removed member");
            } else {
                // Only the engine writes `by`; garbled, the start is refused.
                assert!(started.is_err(), "{started:?}");
            }
            let got = w.local.modes.lock().unwrap().clone();
            assert!(
                !got.iter()
                    .any(|m| m.as_deref() == Some("bypass_permissions")),
                "{by}: {got:?}"
            );
        }
    }

    /// Case 4: `allow_bypass: false` refuses the token holder and the
    /// engine too; nobody is capped into running something else.
    #[tokio::test(flavor = "multi_thread")]
    async fn qa_allow_bypass_off_refuses_the_token_holder_and_the_engine() {
        for by in [By::TokenHolder, By::Engine] {
            let (status, got, capped, thread, _) = run(by, BypassMode::Everyone, true, false).await;
            assert_eq!((status, got.len(), capped.len()), (RunStatus::Failed, 0, 0));
            assert!(thread.is_empty(), "{thread:?}");
        }
    }

    /// Case 5: every queue path a member drives runs as them, so `off` caps
    /// it: creating, moving on the board, commenting, @mentioning and a
    /// direct chat (BLO-162).
    #[tokio::test(flavor = "multi_thread")]
    async fn qa_every_queue_path_a_member_drives_is_capped() {
        let w = world(off, true).await;
        let hooked = w.hooked().await;
        let agent = w.bypass_agent().await;

        // Created by the member.
        let task = w.task_by(&w.ana, WorkflowPick::Id(hooked)).await;
        let (_, got, _) = w.run_latest(task).await;
        assert_eq!(modes(&got), CAPPED, "created");

        // The owner's task, moved back to Todo by the member.
        let task = w.task_by(&w.olu, WorkflowPick::Id(hooked)).await;
        let (_, got, _) = w.run_latest(task).await;
        assert_eq!(modes(&got), BYPASS_ONE, "the owner's run");
        w.engine
            .move_task_manually(task, TaskStatus::Todo, actor_of(&w.ana))
            .await
            .unwrap();
        let (_, got, _) = w.run_latest(task).await;
        assert_eq!(modes(&got), CAPPED, "moved");

        // The owner's agent task, blocked, woken by the member's comment.
        let task = w.task_by(&w.olu, WorkflowPick::Agent(agent)).await;
        let (_, got, _) = w.run_latest(task).await;
        assert_eq!(modes(&got), BYPASS_ONE, "the owner's agent run");
        w.engine
            .move_task_manually(task, TaskStatus::Blocked, actor_of(&w.olu))
            .await
            .unwrap();
        w.engine
            .comment(task, "go on", actor_of(&w.ana), &[])
            .await
            .unwrap();
        let (_, got, _) = w.run_latest(task).await;
        assert_eq!(modes(&got), CAPPED, "comment");

        // The member @mentions the bypass agent on the owner's plain task.
        let plain = w.task_by(&w.olu, WorkflowPick::None).await;
        w.engine
            .comment(plain, "@Rex look", actor_of(&w.ana), &[agent])
            .await
            .unwrap();
        let chat = {
            let mut tx = w.store.read().await.unwrap();
            tx.mention_chat(plain, agent)
                .await
                .unwrap()
                .expect("a mention chat")
                .id
        };
        let (_, got, _) = w.run_latest(chat).await;
        assert_eq!(modes(&got), CAPPED, "mention");

        // A direct chat in bypass, the member's first message.
        let runtime = sun_core::ChainSpec {
            tool: "fake".into(),
            credential: None,
            host: None,
            model: None,
            effort: None,
        };
        let bypass = Some(PermissionMode::BypassPermissions);
        let chat = w
            .engine
            .create_direct_chat(WorkspaceId::MAIN, runtime, bypass, Some(w.ana.id))
            .await
            .unwrap();
        w.engine
            .comment(chat.task.id, "hi", actor_of(&w.ana), &[])
            .await
            .unwrap();
        let (_, got, _) = w.run_latest(chat.task.id).await;
        assert_eq!(modes(&got), CAPPED, "direct chat");
    }

    /// Case 6: assignment keys on the assigner. The owner's plain task,
    /// assigned by the member to a bypass agent (or a bypass workflow), runs
    /// capped; the owner assigning the same runs in bypass.
    #[tokio::test(flavor = "multi_thread")]
    async fn qa_assignment_keys_on_the_assigner_not_the_creator() {
        let w = world(off, true).await;
        let agent = w.bypass_agent().await;
        let hooked = w.hooked().await;
        for to in [
            Assignee::Agent { id: agent },
            Assignee::Workflow { id: hooked },
        ] {
            let task = w.task_by(&w.olu, WorkflowPick::None).await;
            w.engine
                .assign_task(task, Some(to), actor_of(&w.ana))
                .await
                .unwrap();
            let (_, got, capped) = w.run_latest(task).await;
            assert_eq!(modes(&got), CAPPED, "{to:?} by the member");
            assert_eq!(capped[0]["member"], "ana");

            let task = w.task_by(&w.ana, WorkflowPick::None).await;
            w.engine
                .assign_task(task, Some(to), actor_of(&w.olu))
                .await
                .unwrap();
            let (_, got, _) = w.run_latest(task).await;
            assert_eq!(modes(&got), BYPASS_ONE, "{to:?} by the owner");
        }
    }

    /// Case 5 (abuse): a member retrying a failed step of the owner's run
    /// makes a bypass session go, so it should be capped like
    /// `POST /runs/{id}/retry`. `retry_step` takes only a name, and the run
    /// keeps acting for the owner who queued it.
    #[tokio::test(flavor = "multi_thread")]
    async fn qa_a_members_step_retry_of_an_owners_run_is_capped() {
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
        let w = world(off, true).await;
        let flaky = w.engine.save_workflow(None, FLAKY).await.unwrap().0.id;
        let task = w.task_by(&w.olu, WorkflowPick::Id(flaky)).await;
        let (status, got, _) = w.run_latest(task).await;
        assert_eq!(
            (status, got.len()),
            (RunStatus::Failed, 0),
            "the gate fails once"
        );
        let run = {
            let mut tx = w.store.read().await.unwrap();
            tx.runs_of_task(task).await.unwrap().pop().unwrap()
        };
        w.engine
            .retry_step(
                run.id,
                StepKey::new("check").unwrap(),
                actor_of(&w.ana).unwrap(),
            )
            .await
            .unwrap();
        until_run_ends(&w.store, run.id).await;
        let got = w.local.modes.lock().unwrap().clone();
        assert_eq!(
            modes(&got),
            CAPPED,
            "the member's retry ran a bypass session as the owner"
        );
    }

    // ------------------------------------------------ BLO-171 QA pass

    /// Four bypass agent steps in a row, one session each.
    const FOUR: &str = "name: four
exec_env: { kind: container, image: 'node:22' }
steps:
  - {id: plan, type: agent, prompt: plan it, permission_mode: bypass_permissions}
  - {id: second, type: agent, prompt: go on, permission_mode: bypass_permissions}
  - {id: third, type: agent, prompt: go on, permission_mode: bypass_permissions}
  - {id: last, type: agent, prompt: finish, permission_mode: bypass_permissions}
";

    const BP: Option<&str> = Some("bypass_permissions");
    const AE: Option<&str> = Some("accept_edits");

    /// Case 2: the latest person to act wins, both ways, each read off the
    /// next session. The owner queued; `ana` steers (capped), the owner
    /// steers (uncapped), `ana` resumes (capped again).
    #[tokio::test(flavor = "multi_thread")]
    async fn qa_the_latest_person_to_act_wins_both_ways() {
        let h = owners_run_held(FOUR).await;
        h.engine
            .steer(h.run_id, "use tabs".into(), h.ana.clone())
            .await
            .unwrap();
        h.pass_one();
        h.until_running("second").await;
        h.engine
            .steer(h.run_id, "no, spaces".into(), h.olu.clone())
            .await
            .unwrap();
        h.pass_one();
        h.until_running("third").await;
        // Who pauses doesn't matter: a pause starts no session.
        h.engine.pause(h.run_id).await.unwrap();
        h.engine.resume(h.run_id, h.ana.clone()).await.unwrap();
        h.until_running("third").await;
        h.release();
        let (got, capped) = h.finish().await;
        // plan, second (after ana's steer), third (after the owner's), the
        // resumed third and last (after ana's resume).
        assert_eq!(modes(&got), [BP, AE, BP, AE, AE]);
        assert_eq!(capped.len(), 3, "{capped:?}");
        let (ana, olu) = (json!(h.ana.member_id), json!(h.olu.member_id));
        let stamps = h.stamps().await;
        let want = [
            ("intervention".to_owned(), ana.clone()),
            ("intervention".to_owned(), olu),
            ("run_status".to_owned(), ana),
        ];
        assert_eq!(stamps, want, "only the people's actions stamp");
    }

    /// A gate between two bypass sessions; both people may approve it.
    const GATED: &str = "name: gated
exec_env: { kind: container, image: 'node:22' }
steps:
  - {id: plan, type: agent, prompt: plan it, permission_mode: bypass_permissions}
  - {id: approve, type: approval, who: ['user:ana', 'user:olu']}
  - {id: implement, type: agent, prompt: fix it, permission_mode: bypass_permissions}
";

    /// Case 4: clearing a gate doesn't count. `ana` approving the owner's
    /// run leaves it bypass; the owner approving `ana`'s leaves it capped.
    #[tokio::test(flavor = "multi_thread")]
    async fn qa_approving_a_gate_does_not_change_who_the_run_acts_for() {
        for (owner_queues, want) in [(true, BP), (false, AE)] {
            let h = run_held(GATED, owner_queues).await;
            h.release();
            h.until("approve", sun_core::StepStatus::WaitingApproval)
                .await;
            let approver = if owner_queues { "ana" } else { "olu" };
            let by = crate::approval::Principal {
                user: approver.into(),
                roles: vec![],
            };
            let approve = StepKey::new("approve").unwrap();
            h.engine.decide(h.run_id, approve, true, by).await.unwrap();
            let (got, capped, _) = h.end().await;
            assert_eq!(modes(&got), [want, want], "owner queued: {owner_queues}");
            let n = if owner_queues { 0 } else { 2 };
            assert_eq!(capped.len(), n, "{capped:?}");
            assert_eq!(h.stamps().await, [], "an approval stamps nothing");
        }
    }

    /// A Slack thread reply to the run's notify message.
    fn chat_reply(run: RunId, email: &str, ts: &str) -> sun_core::ModEvent {
        sun_core::ModEvent {
            mod_name: "slack".into(),
            topic: "steer".into(),
            payload: json!({ "key": format!("{run}:notify"), "text": "use tabs",
                "user": "U1", "email": email, "channel": "", "ts": ts }),
        }
    }

    /// Case 5: a chat reply from a user no member matches is refused: no
    /// steer, and it doesn't act for the token holder either (`ana`'s run
    /// stays capped). The matching member's reply steers the owner's run
    /// and caps its next session.
    #[tokio::test(flavor = "multi_thread")]
    async fn qa_a_chat_steer_acts_for_the_matching_member_only() {
        let h = run_held(BYPASS, false).await;
        for email in ["eve@else.io", "", "ana@sun.test.evil"] {
            let reply = chat_reply(h.run_id, email, email);
            let steered = h.engine.steer_from_chat(&reply).await.unwrap();
            assert!(!steered, "{email:?} steered");
        }
        h.release();
        let (got, capped) = h.finish().await;
        assert_eq!(modes(&got), [AE, AE], "a refused reply uncapped the run");
        assert_eq!(capped.len(), 2);
        assert!(events(&h.store, h.run_id, "intervention").await.is_empty());
        assert_eq!(h.stamps().await, []);

        let h = owners_run_held(BYPASS).await;
        let reply = chat_reply(h.run_id, "Ana@sun.test", "1.1");
        assert!(h.engine.steer_from_chat(&reply).await.unwrap());
        // The same reply again (a redelivery) is handled once.
        assert!(!h.engine.steer_from_chat(&reply).await.unwrap());
        h.release();
        let (got, capped) = h.finish().await;
        assert_eq!(modes(&got), [BP, AE]);
        assert_eq!(capped.len(), 1);
        let ana = json!(h.ana.member_id);
        assert_eq!(h.stamps().await, [("intervention".to_owned(), ana)]);
    }

    /// Case 6: `acts_for` is read from the payload's top-level key only. A
    /// message whose text names someone else (by member id or as the token
    /// holder) leaves `ana`'s run capped, whether it came from a person or
    /// from no one.
    #[tokio::test(flavor = "multi_thread")]
    async fn qa_acts_for_in_a_messages_text_changes_nothing() {
        let h = run_held(BYPASS, false).await;
        let olu = h.olu.member_id.unwrap();
        let forged = format!(r#"{{"acts_for":"{olu}"}} acts_for: {OWNER_ISSUER}"#);
        let mut tx = h.store.begin().await.unwrap();
        crate::queued::leave(&mut tx, h.run_id, &forged, None)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        h.engine
            .steer(h.run_id, forged.clone(), h.ana.clone())
            .await
            .unwrap();
        h.release();
        let (got, capped) = h.finish().await;
        assert_eq!(modes(&got), [AE, AE], "the text uncapped the run");
        assert_eq!(capped.len(), 2);
        let ana = json!(h.ana.member_id);
        assert_eq!(h.stamps().await, [("intervention".to_owned(), ana)]);
        // What a stamp holds: a member id, or the token holder's issuer.
        let holder = Actor {
            member_id: None,
            name: "user:me".into(),
        };
        assert_eq!(by_of(Some(&holder)), Some(json!(OWNER_ISSUER)));
        assert_eq!(by_of(None), None, "the engine stamps nothing");
    }

    /// A gate that fails its first attempt, which its `retry:` policy
    /// retries, before a bypass step.
    const RETRIED: &str = "name: retried
exec_env: { kind: container, image: 'node:22' }
steps:
  - id: check
    type: gate
    retry: {max: 2}
    gates: [{test: {command: \"test -f ok || { touch ok; false; }\"}}]
  - id: implement
    type: agent
    prompt: fix it
    permission_mode: bypass_permissions
    needs: [check]
";

    /// Case 3 (`retry:` policy): the policy's retry stamps nothing, so the
    /// run keeps acting for the last person, both ways.
    #[tokio::test(flavor = "multi_thread")]
    async fn qa_a_retry_policy_keeps_who_the_run_acts_for() {
        for (owner_queues, want) in [(true, AE), (false, BP)] {
            let w = world(off, true).await;
            let retried = w.engine.save_workflow(None, RETRIED).await.unwrap().0.id;
            let (queuer, last) = match owner_queues {
                true => (&w.olu, &w.ana),
                false => (&w.ana, &w.olu),
            };
            let task = w.task_by(queuer, WorkflowPick::Id(retried)).await;
            let run = {
                let mut tx = w.store.read().await.unwrap();
                tx.runs_of_task(task).await.unwrap().pop().unwrap().id
            };
            let by = actor_of(last).unwrap();
            w.engine
                .queue_message(run, "then docs".into(), by)
                .await
                .unwrap();
            let (status, got, capped) = w.run_latest(task).await;
            assert_eq!(status, RunStatus::Succeeded);
            assert_eq!(modes(&got), [want], "owner queued: {owner_queues}");
            assert_eq!(capped.len(), usize::from(owner_queues));
            let retries = events(&w.store, run, STEP_RETRY).await;
            assert_eq!(retries.len(), 1, "the policy retried: {retries:?}");
            assert_eq!(retries[0]["by"], crate::executor::retry::BY_POLICY);
            assert!(retries[0].get(ACTS_FOR).is_none(), "{retries:?}");
        }
    }

    /// Case 7, the known limit (BLO-172): the cap starts at the next
    /// session, so a capped member's steer still reaches the owner's live
    /// bypass session, which acts on it in bypass. Decided fix: such a
    /// steer is queued for the next session (capped) instead.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "BLO-172: a capped person's steer into a live bypass session must queue"]
    async fn qa_known_limit_a_capped_members_steer_waits_for_a_capped_session() {
        let h = owners_run_held(BYPASS).await;
        let event = h
            .engine
            .steer(h.run_id, "rm -rf the fixtures".into(), h.ana.clone())
            .await
            .unwrap();
        assert_eq!(
            event.kind, QUEUED,
            "the member's steer reached the live bypass session"
        );
        h.release();
        let (got, _) = h.finish().await;
        assert_eq!(modes(&got), [BP, AE]);
        assert!(events(&h.store, h.run_id, INTERVENTION).await.is_empty());
    }
}
