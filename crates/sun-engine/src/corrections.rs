//! Corrections: human corrections of agent work, captured
//! where they happen and labelled by cause. Child module of `executor`.
//!
//! **Capture hooks** (each writes the row in the caller's transaction, so a
//! correction commits with the change that caused it):
//! - a gate step's results with a failure (`on_gated`);
//! - a rejected approval or escalated decision, with the reason (`decide`);
//! - a steer that contradicts the agent (`steer`, [`contradicts`]);
//! - an escalated `decide` step answered differently from the decision
//!   engine ([`Engine::correct_decision`]);
//! - a revert pushed to the task branch `sun/<task_id>`
//!   ([`Engine::record_revert`], fed by git host push webhooks).
//!
//! **Classification** is rule based ([`classify`], [`gate_cause`]) and the
//! row says so (`classified_by: "rules"`). With a decision backend,
//! [`Engine::classify_corrections`] asks it after commit and relabels the
//! confident answers (`classified_by: "model:<model>"`), which
//! re-runs the memory proposal trigger for the new cause.

use chrono::Utc;
use serde_json::{Value, json};
use sun_core::model::{AuditAction, Correction, CorrectionCause, CorrectionKind, Step};
use sun_core::{CorrectionId, Error, Result, RunId, StepKey, StepStatus, TaskId};
use sun_store::{AuditRepo, CorrectionRepo, EventLog, RunRepo, RuntimeRepo, SettingRepo, Tx};
use sun_workflow::{QuestionKind, StepKind};
use tokio::sync::oneshot;

use super::decision::DECISION;
use super::{Command, Decision, Engine, Executor, Hook, append, move_step, state};
use crate::approval::{self, Principal};
use crate::gates::GateResult;

/// `classified_by` of rule-based labels.
const RULES: &str = "rules";
/// Longest stored body, in chars; gate output keeps its tail.
const MAX_BODY: usize = 4000;
/// Output tail kept per failed gate.
const GATE_TAIL: usize = 1500;

/// `correct_decision` replies.
pub(super) type Reply = oneshot::Sender<Result<()>>;

/// The cause a failed gate points at, by its name (`None`: read the output).
pub(crate) fn gate_cause(name: &str) -> Option<CorrectionCause> {
    use CorrectionCause::*;
    Some(match name {
        "test" | "ci_green" => TestFailure,
        "lint" | "diagnostics_clean" => Lint,
        "format" | "image_size" => Style,
        "diff_not_empty" | "pr_exists" | "sections_present" | "length" | "terms"
        | "links_resolve" | "cites_sources" => MissingRequirement,
        _ => return None,
    })
}

/// Rule-based cause of a correction's text: the first rule with a matching
/// phrase wins, in the order scope, missing, tests, lint, style.
// ponytail: English keyword rules; the decision engine relabels through
// `set_correction_cause` once it classifies corrections.
pub(crate) fn classify(text: &str) -> CorrectionCause {
    use CorrectionCause::*;
    const RULES: &[(CorrectionCause, &[&str])] = &[
        (
            WrongScope,
            &[
                "scope",
                "unrelated",
                "only change",
                "don't touch",
                "do not touch",
                "too many changes",
                "too much",
                "shouldn't have changed",
                "not asked",
                "didn't ask",
            ],
        ),
        (
            MissingRequirement,
            &[
                "missing",
                "forgot",
                "forgotten",
                "also need",
                "also needs",
                "requirement",
                "not implemented",
                "incomplete",
                "doesn't handle",
                "does not handle",
                "didn't",
                "edge case",
            ],
        ),
        (
            TestFailure,
            &[
                "test",
                "tests",
                "failing",
                "fails",
                "failed",
                "assert",
                "assertion",
                "panicked",
                "ci",
                "regression",
                "broken",
                "breaks",
            ],
        ),
        (
            Lint,
            &[
                "lint",
                "clippy",
                "eslint",
                "warning",
                "warnings",
                "diagnostics",
                "type error",
                "unused",
            ],
        ),
        (
            Style,
            &[
                "style",
                "format",
                "formatting",
                "naming",
                "rename",
                "typo",
                "indent",
                "indentation",
                "snake_case",
                "camelcase",
                "readability",
                "nit",
                "wording",
                "tone",
            ],
        ),
    ];
    let words = words(text);
    RULES
        .iter()
        .find(|(_, phrases)| phrases.iter().any(|p| words.contains(&format!(" {p} "))))
        .map_or(Other, |(cause, _)| *cause)
}

/// True when a steer pushes back on what the agent is doing (vs. adding to it).
// ponytail: English keyword rules, same ceiling as `classify`.
pub(crate) fn contradicts(text: &str) -> bool {
    const PHRASES: &[&str] = &[
        "no",
        "nope",
        "stop",
        "don't",
        "do not",
        "instead",
        "wrong",
        "undo",
        "revert",
        "shouldn't",
        "should not",
        "not what",
        "not that",
        "rather than",
        "wasn't asked",
    ];
    let words = words(text);
    PHRASES.iter().any(|p| words.contains(&format!(" {p} ")))
}

/// Lower-case words between single spaces, padded, for whole-phrase search.
fn words(text: &str) -> String {
    let text = text.to_lowercase().replace('’', "'");
    let mut out = String::from(" ");
    for word in text.split(|c: char| !(c.is_alphanumeric() || c == '\'' || c == '_')) {
        if !word.is_empty() {
            out.push_str(word);
            out.push(' ');
        }
    }
    out
}

/// The last `n` chars of `text`.
fn tail(text: &str, n: usize) -> &str {
    let skip = text.chars().count().saturating_sub(n);
    text.char_indices()
        .nth(skip)
        .map_or("", |(i, _)| &text[i..])
}

/// Records a correction of run `run_id` in the caller's transaction, linked
/// to the run's workflow and to the runtime of its latest agent attempt.
/// `cause: None` classifies `body` by the rules. `false`: `dedupe_key` was
/// already recorded.
pub(crate) async fn record(
    tx: &mut Tx,
    run_id: RunId,
    step_key: Option<&StepKey>,
    kind: CorrectionKind,
    cause: Option<CorrectionCause>,
    body: &str,
    dedupe_key: Option<&str>,
) -> Result<bool> {
    let run = tx.get_run(run_id).await?;
    let agent = tx
        .steps_of_run(run_id)
        .await?
        .into_iter()
        .filter_map(|s| Some((s.started_at, s.runtime_id?)))
        .max_by_key(|(at, _)| *at);
    let runtime = match agent {
        Some((_, id)) => Some(tx.get_runtime(id).await?.name),
        None => None,
    };
    let body: String = body.trim().chars().take(MAX_BODY).collect();
    let correction = Correction {
        id: CorrectionId::generate(),
        task_id: run.task_id,
        run_id,
        workflow_id: run.workflow_id,
        step_key: step_key.cloned(),
        runtime,
        kind,
        cause_label: cause.unwrap_or_else(|| classify(&body)),
        classified_by: RULES.into(),
        body,
        created_at: Utc::now(),
    };
    let recorded = tx.insert_correction(&correction, dedupe_key).await?;
    if recorded {
        crate::memory::on_correction(tx, &correction).await?;
    }
    Ok(recorded)
}

/// Hook: a gate step's results; records one correction when any failed.
pub(super) async fn on_gated(tx: &mut Tx, step: &Step, results: &[GateResult]) -> Result<()> {
    let failed: Vec<&GateResult> = results.iter().filter(|r| !r.pass).collect();
    let Some(first) = failed.first() else {
        return Ok(());
    };
    let body: Vec<String> = failed
        .iter()
        .map(|r| format!("Gate `{}` failed: {}", r.name, tail(&r.output, GATE_TAIL)))
        .collect();
    let body = body.join("\n");
    let cause = gate_cause(&first.name).unwrap_or_else(|| classify(&body));
    let kind = CorrectionKind::GateFail;
    record(
        tx,
        step.run_id,
        Some(&step.step_key),
        kind,
        Some(cause),
        &body,
        None,
    )
    .await?;
    Ok(())
}

/// Hook: a human rejected `step` (an approval or an escalated decision).
pub(super) async fn on_rejected(tx: &mut Tx, step: &Step, reason: Option<&str>) -> Result<()> {
    let body = reason
        .filter(|r| !r.trim().is_empty())
        .unwrap_or("Rejected without a reason.");
    let kind = CorrectionKind::Reject;
    record(
        tx,
        step.run_id,
        Some(&step.step_key),
        kind,
        None,
        body,
        None,
    )
    .await?;
    Ok(())
}

/// Hook: a human steered `step`'s agent; recorded when it contradicts it.
pub(super) async fn on_steer(tx: &mut Tx, step: &Step, text: &str) -> Result<()> {
    if contradicts(text) {
        let kind = CorrectionKind::Steer;
        record(
            tx,
            step.run_id,
            Some(&step.step_key),
            kind,
            None,
            text,
            None,
        )
        .await?;
    }
    Ok(())
}

/// Kernel setting: the `created_at` of the newest correction the decision
/// engine has classified.
const CLASSIFIED_UNTIL: &str = "decisions:classified_until";
/// Corrections one classifier pass looks at.
const CLASSIFY_SCAN: u32 = 500;
/// Wait between classifier passes.
const CLASSIFY_EVERY: std::time::Duration = std::time::Duration::from_secs(60);

/// "What caused this correction?". Rule: its rule-based cause.
fn cause_hook(c: &Correction) -> Hook {
    use CorrectionCause::*;
    let criteria: serde_json::Map<String, Value> = [
        (TestFailure, "tests or CI failed"),
        (Lint, "lint, type or diagnostics errors"),
        (
            MissingRequirement,
            "something the task asked for is missing",
        ),
        (WrongScope, "changed more, or other things, than asked"),
        (Style, "naming, formatting or wording"),
        (Other, "none of these"),
    ]
    .into_iter()
    .map(|(cause, what)| (cause.as_str().to_owned(), json!(what)))
    .collect();
    Hook::new(
        "correction_cause",
        QuestionKind::Choice,
        "A person corrected an AI agent's work. What caused the correction?",
        json!(criteria),
        json!({ "kind": c.kind, "step": c.step_key, "body": c.body }),
        json!({ "choice": c.cause_label.as_str() }),
    )
}

impl Engine {
    /// One classifier pass: asks the decision engine the cause of every
    /// rule-labelled correction recorded since the last pass, oldest first,
    /// journals each answer on the correction's run and relabels the
    /// confident ones (`classified_by: model:<model>`, which re-runs the
    /// memory proposal trigger). Returns how many were relabelled; without a
    /// backend, none (the rule labels stand).
    // ponytail: the cursor is a timestamp; two corrections in the same
    // instant across a pass boundary lose the second's question.
    pub async fn classify_corrections(&self) -> Result<usize> {
        let Some(client) = self.decisions.get() else {
            return Ok(0);
        };
        let mut read = self.store.read().await?;
        let since = read.setting(CLASSIFIED_UNTIL).await?;
        let since: Option<sun_core::Timestamp> = since.and_then(|v| serde_json::from_value(v).ok());
        let fresh: Vec<Correction> = read
            .corrections(CLASSIFY_SCAN)
            .await?
            .into_iter()
            .filter(|c| c.classified_by == RULES && since.is_none_or(|s| c.created_at > s))
            .collect();
        drop(read);
        let mut relabelled = 0;
        for c in fresh.iter().rev() {
            let settled = cause_hook(c).ask(Some(client.clone())).await;
            let event = settled.event.as_ref();
            let acted = event.is_some_and(|e| e["acted"] == true);
            let model = event.and_then(|e| e["model"].as_str()).unwrap_or("unknown");
            let cause = settled
                .choice()
                .and_then(|l| l.parse::<CorrectionCause>().ok());
            let mut tx = self.store.begin().await?;
            settled
                .journal(&mut tx, c.run_id, c.step_key.as_ref())
                .await?;
            if let Some(cause) = cause.filter(|cause| acted && *cause != c.cause_label) {
                crate::memory::relabel(&mut tx, c.id, cause, &format!("model:{model}")).await?;
                relabelled += 1;
            }
            tx.put_setting(CLASSIFIED_UNTIL, &json!(c.created_at))
                .await?;
            tx.commit().await?;
        }
        Ok(relabelled)
    }

    /// Runs [`Engine::classify_corrections`] every minute, forever.
    pub async fn follow_corrections(self) {
        loop {
            if let Err(e) = self.classify_corrections().await {
                eprintln!("sun: classifying corrections: {e}");
            }
            tokio::time::sleep(CLASSIFY_EVERY).await;
        }
    }

    /// Every task's latest `limit` corrections, newest first.
    pub async fn corrections(&self, limit: u32) -> Result<Vec<Correction>> {
        Ok(self.store.read().await?.corrections(limit).await?)
    }

    /// The task's corrections, oldest first.
    pub async fn corrections_of(&self, task_id: TaskId) -> Result<Vec<Correction>> {
        Ok(self.store.read().await?.corrections_of(task_id).await?)
    }

    /// Records that commit `commit` (`message`) pushed to the task branch
    /// reverts earlier work, against the task's latest run. Idempotent per
    /// commit; `false` when already recorded or the task never ran.
    // ponytail: every revert pushed to `sun/<task_id>` counts as reverting
    // Sun's work; check the reverted sha's author if humans share the branch.
    pub async fn record_revert(
        &self,
        task_id: TaskId,
        commit: &str,
        message: &str,
    ) -> Result<bool> {
        let mut tx = self.store.begin().await?;
        let Some(run) = tx.runs_of_task(task_id).await?.pop() else {
            return Ok(false);
        };
        let body = format!("Commit {} reverted: {message}", tail(commit, 12));
        let key = format!("revert:{task_id}:{commit}");
        let kind = CorrectionKind::Revert;
        let recorded = record(&mut tx, run.id, None, kind, None, &body, Some(&key)).await?;
        tx.commit().await?;
        Ok(recorded)
    }

    /// A human in the step's `who:` answers an escalated `decide` step
    /// (`waiting_approval`) with `answer` in the wire shape (`{"choice":
    /// "billing"}`, `{"score": 2}` or `{"noul": 0.1}`), validated against the
    /// question. The step succeeds with that answer; when it differs from
    /// the decision engine's, a `decision_override` correction is recorded.
    pub async fn correct_decision(
        &self,
        run_id: RunId,
        step_key: StepKey,
        by: Principal,
        answer: Value,
    ) -> Result<()> {
        self.request(|reply| Command::CorrectDecision {
            run_id,
            step_key,
            by,
            answer,
            reply,
        })
        .await
    }
}

impl Executor {
    pub(super) async fn correct_decision(
        &mut self,
        run_id: RunId,
        step_key: StepKey,
        by: Principal,
        answer: Value,
    ) -> Result<()> {
        let mut tx = self.store.begin().await?;
        let run = tx.get_run(run_id).await?;
        let workflow = self.workflow(&mut tx, &run).await?;
        let allowed_as = approval::authorize(&workflow, &step_key, &by)?;
        let (decides, question) = workflow
            .steps
            .iter()
            .find(|d| d.id == step_key)
            .and_then(|d| match &d.kind {
                StepKind::Decide {
                    decides, question, ..
                } => Some((*decides, question)),
                _ => None,
            })
            .ok_or_else(|| Error::InvalidInput(format!("`{step_key}` is not a decide step")))?;
        let mut step = state::latest(tx.steps_of_run(run_id).await?)
            .remove(&step_key)
            .ok_or_else(|| Error::NotFound(format!("step `{step_key}` of run {run_id}")))?;
        if step.status != StepStatus::WaitingApproval {
            return Err(Error::InvalidInput(format!(
                "`{step_key}` is not waiting for a human decision"
            )));
        }
        // The validation of backend replies, applied to the human's answer.
        let mut reply = answer;
        if let Some(fields) = reply.as_object_mut() {
            fields.insert("answer_confidence".into(), json!(1.0));
        }
        let human = Decision::parse(question, &json!({ "answers": { "decision": reply } }))
            .map_err(|e| Error::InvalidInput(e.to_string()))?;
        let human = json!(human.answer());
        let asked = tx
            .events_after(Some(run_id), 0, u32::MAX)
            .await?
            .into_iter()
            .rfind(|e| e.kind == DECISION && e.step_key.as_ref() == Some(&step_key))
            .map(|e| e.payload)
            .unwrap_or_default();
        move_step(&mut tx, &mut step, StepStatus::Succeeded, None).await?;
        let payload = json!({
            "decides": decides,
            "acted": true,
            "reason": "answered by a human",
            "answer": human,
            "by": by.user,
            "as": allowed_as,
        });
        append(&mut tx, &step, DECISION, payload).await?;
        let mut effects = Vec::new();
        self.act(&mut tx, &step, decides, &human, &mut effects)
            .await?;
        tx.append_audit(
            &format!("user:{}", by.user),
            AuditAction::Approval,
            &format!("{run_id}/{step_key}"),
            "answered",
        )
        .await?;
        if !asked["answer"].is_null() && asked["answer"] != human {
            let body = format!(
                "The decision engine answered {} (confidence {}); {} answered {human}.",
                asked["answer"], asked["confidence"], by.user
            );
            let kind = CorrectionKind::DecisionOverride;
            record(&mut tx, run_id, Some(&step_key), kind, None, &body, None).await?;
        }
        self.advance(&mut tx, run_id, &mut effects).await?;
        tx.commit().await?;
        self.apply(effects).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use CorrectionCause::*;

    #[test]
    fn correction_rules_classify_by_cause() {
        let cases = [
            (
                "You changed unrelated files in the billing module",
                WrongScope,
            ),
            (
                "Missing tests for the empty-cart edge case",
                MissingRequirement,
            ),
            ("It doesn't handle expired tokens", MissingRequirement),
            ("Tests fail on CI: assertion left == right", TestFailure),
            ("clippy warnings everywhere", Lint),
            ("Rename this to snake_case, and fix the typo", Style),
            ("Looks off to me", Other),
            // Whole words only: "latest" is not "test", "cinema" not "ci".
            ("use the latest cinema data", Other),
        ];
        for (text, cause) in cases {
            assert_eq!(classify(text), cause, "{text}");
        }
        assert_eq!(gate_cause("ci_green"), Some(TestFailure));
        assert_eq!(gate_cause("diagnostics_clean"), Some(Lint));
        assert_eq!(gate_cause("sections_present"), Some(MissingRequirement));
        assert_eq!(gate_cause("vendor.check"), None);
    }

    #[test]
    fn correction_steers_count_only_when_they_push_back() {
        assert!(contradicts("No, use the v2 endpoint instead."));
        assert!(contradicts("Don’t touch the migrations"));
        assert!(contradicts("stop and revert the config change"));
        assert!(!contradicts("Also add a changelog entry, please."));
        assert!(!contradicts("Notice the nominal case is fine"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn correction_recorded_when_a_gate_sends_work_back() {
        use std::time::Duration;

        use sun_core::HostId;
        use sun_store::Store;

        use crate::gates::tests::{LocalHost, mods, seed, until_run_ends};
        use crate::{EngineConfig, WatchdogConfig};

        const FLOW: &str = r#"
name: fix
steps:
  - id: implement
    type: agent
    prompt: fix it
  - id: verify
    type: gate
    gates: [{test: {command: "test -f fixed.txt || (echo 'fixed.txt missing'; exit 1)"}}]
    on_fail: { send_back_to: implement, max: 2 }
"#;
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).await.unwrap();
        // Fixes things only once the gate said what failed.
        let host = LocalHost::new(dir.path(), |ws, envelope| {
            if envelope.contains("fixed.txt missing") {
                std::fs::write(ws.join("fixed.txt"), "ok").unwrap();
            }
        });
        let config = EngineConfig {
            allow_bypass: false,
            host: HostId::generate(),
            adapter: "fake".into(),
            delivery_timeout: Duration::from_secs(5),
            watchdog: WatchdogConfig::default(),
        };
        let (engine, _task) = Engine::start(store.clone(), host, mods(json!({})), config)
            .await
            .unwrap();
        let run = seed(&store, FLOW).await;
        engine.start_run(run).await.unwrap();
        let ended = until_run_ends(&store, run).await;
        assert_eq!(ended.status, sun_core::RunStatus::Succeeded);

        let rows = engine.corrections_of(ended.task_id).await.unwrap();
        assert_eq!(rows.len(), 1, "only the failed check: {rows:?}");
        let c = &rows[0];
        assert_eq!(
            (c.kind, c.cause_label),
            (CorrectionKind::GateFail, TestFailure)
        );
        assert_eq!(c.step_key.as_ref().map(StepKey::as_str), Some("verify"));
        assert_eq!((c.run_id, c.workflow_id), (run, ended.workflow_id));
        assert_eq!(c.runtime.as_deref(), Some("fake"));
        assert!(c.body.starts_with("Gate `test` failed: "), "{}", c.body);
        assert!(c.body.contains("fixed.txt missing"), "{}", c.body);
        assert_eq!(engine.corrections(10).await.unwrap(), rows);
    }

    #[test]
    fn correction_bodies_keep_the_output_tail() {
        assert_eq!(tail("abcdef", 3), "def");
        assert_eq!(tail("ab", 3), "ab");
        assert_eq!(tail("héllo", 4), "éllo");
    }
}
