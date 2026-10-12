//! Decision engine: the `decide` step and its `/v1/systemone`
//! client.
//!
//! **Pattern:** **Adapter**. [`DecisionClient`] adapts the System One wire
//! format (a Laya sidecar, or Jev, which speaks the same `/v1/systemone`) to
//! a typed [`Decision`]. A child module of `executor`, like `failover`, so it
//! reuses the executor's private helpers.
//!
//! Lifecycle of a `decide` step attempt (the executor drives it):
//! `queued → running`, the question is asked off the engine task (again
//! after a restart: asking is a read), then in one transaction a `decision`
//! event `{decides, threshold, acted, reason, answer, confidence, model}` and:
//! - confidence ≥ `threshold`: `running → verifying → succeeded`;
//! - below it, a backend error, or no backend installed: `running →
//!   waiting_approval`, and a human in `who:` decides through
//!   [`Engine::decide`].
//!
//! ## The boundary
//! A [`Decision`] can route, triage, flag or add a check
//! ([`sun_workflow::DecisionUse`]). It has no conversion into a
//! [`GateResult`] or an approval, and a `GateResult` can't be built outside
//! the engine, so a decision never passes a gate, marks work done or skips an
//! approval. These don't compile:
//!
//! ```compile_fail,E0277
//! fn pass(decision: sun_engine::Decision) -> sun_engine::GateResult {
//!     decision.into()
//! }
//! ```
//! ```compile_fail,E0639
//! fn pass(decision: &sun_engine::Decision) -> sun_engine::GateResult {
//!     sun_engine::GateResult {
//!         name: "test".into(),
//!         pass: decision.confidence() > 0.5,
//!         output: String::new(),
//!         file: None,
//!     }
//! }
//! ```
//! ```compile_fail,E0277
//! fn approved(decision: sun_engine::Decision) -> bool {
//!     decision.into()
//! }
//! ```
//! ```compile_fail,E0308
//! async fn approve(
//!     engine: &sun_engine::Engine,
//!     run: sun_core::RunId,
//!     step: sun_core::StepKey,
//!     decision: sun_engine::Decision,
//!     by: sun_engine::Principal,
//! ) {
//!     let _ = engine.decide(run, step, decision, by).await;
//! }
//! ```
//! The same code with the conversion taken out compiles (the control):
//! ```
//! fn read(decision: &sun_engine::Decision, gate: &sun_engine::GateResult) -> (f64, bool) {
//!     (decision.confidence(), gate.pass)
//! }
//! async fn approve(
//!     engine: &sun_engine::Engine,
//!     run: sun_core::RunId,
//!     step: sun_core::StepKey,
//!     by: sun_engine::Principal,
//! ) {
//!     let _ = engine.decide(run, step, true, by).await;
//! }
//! ```
//!
//! [`Engine::decide`]: super::Engine::decide
//! [`GateResult`]: crate::GateResult

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use serde::Serialize;
use serde_json::{Value, json};
use sun_core::model::Step;
use sun_core::redact::redact;
use sun_core::{
    Error, ModCall, ModLink, Result, RunId, RunStatus, StepId, StepKey, StepStatus, TaskId,
};
use sun_store::{EventLog, NewEvent, RunRepo, SettingRepo, TaskRepo, Tx};
use sun_workflow::{DecisionUse, GateSpec, Question, QuestionKind, StepKind};

use super::{Effect, Executor, append, move_step};
use crate::approval::Principal;
use crate::gates::{self, GateResult};

// The boundary in every build, not only in the doc tests: should `Decision` ever
// convert into a `GateResult` or a `bool` (what an approval takes), the call
// below turns ambiguous and the crate stops compiling.
const _: fn() = || {
    trait AmbiguousIfInto<A> {
        fn check() {}
    }
    impl<T: ?Sized> AmbiguousIfInto<()> for T {}
    struct IntoGate;
    struct IntoApproval;
    impl<T: Into<GateResult>> AmbiguousIfInto<IntoGate> for T {}
    impl<T: Into<bool>> AmbiguousIfInto<IntoApproval> for T {}
    <Decision as AmbiguousIfInto<_>>::check();
};

/// Event kind: a decision (or an escalation) on the decide step.
pub(super) const DECISION: &str = "decision";
/// The question's id in the request; one question per call.
const QUESTION_ID: &str = "decision";
/// A System One answer takes tens of milliseconds; past this, a human decides.
const TIMEOUT: Duration = Duration::from_secs(10);

/// Client for `/v1/systemone`: a Laya sidecar or Jev over HTTP, or a mod
/// that speaks it (the Jev plugin path, [`DecisionClient::plugin`]). Cheap
/// to clone.
#[derive(Clone)]
pub struct DecisionClient {
    target: Target,
}

#[derive(Clone)]
enum Target {
    Http {
        base_url: String,
        api_key: Option<String>,
    },
    /// `<mod>.systemone(request)` answers with the `/v1/systemone` reply.
    Mod {
        mods: Arc<dyn ModLink>,
        name: String,
    },
}

impl std::fmt::Debug for DecisionClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.target {
            Target::Http { base_url, .. } => write!(f, "DecisionClient({base_url})"),
            Target::Mod { name, .. } => write!(f, "DecisionClient(mod {name})"),
        }
    }
}

/// The method a decision plugin mod provides: `/v1/systemone` over JSON-RPC
/// (params: the request, result: the reply).
pub const PLUGIN_METHOD: &str = "systemone";

impl DecisionClient {
    /// `base_url` like `http://127.0.0.1:8080`; `api_key` is sent as a bearer
    /// token (Laya's `LAYA_API_KEY`, or a Jev key).
    pub fn new(base_url: &str, api_key: Option<String>) -> Result<Self> {
        let base_url = base_url.trim_end_matches('/');
        if !(base_url.starts_with("http://") || base_url.starts_with("https://")) {
            return Err(Error::InvalidInput(format!(
                "decision backend `{base_url}`: use an http:// or https:// URL"
            )));
        }
        Ok(Self {
            target: Target::Http {
                base_url: base_url.to_owned(),
                api_key: api_key.filter(|k| !k.is_empty()),
            },
        })
    }

    /// A mod (e.g. `jev`) whose [`PLUGIN_METHOD`] answers the
    /// `/v1/systemone` request ("Jev through a plugin").
    pub fn plugin(mods: Arc<dyn ModLink>, name: &str) -> Self {
        Self {
            target: Target::Mod {
                mods,
                name: name.to_owned(),
            },
        }
    }

    /// The same backend at `base_url`, with the same key: a candidate model
    /// version to replay ([`super::Engine::replay_decisions`]).
    pub fn at(&self, base_url: &str) -> Result<Self> {
        let key = match &self.target {
            Target::Http { api_key, .. } => api_key.clone(),
            Target::Mod { .. } => None,
        };
        Self::new(base_url, key)
    }

    /// The HTTP backend's base URL (`None` for a plugin).
    pub fn url(&self) -> Option<&str> {
        match &self.target {
            Target::Http { base_url, .. } => Some(base_url),
            Target::Mod { .. } => None,
        }
    }

    /// Asks one typed question about `state`.
    pub(crate) async fn ask(&self, state: Value, question: &Question) -> Result<Decision> {
        let body = json!({ "state": state, "questions": { QUESTION_ID: question } });
        let reply = match &self.target {
            Target::Http { base_url, api_key } => {
                let (url, key) = (base_url.clone(), api_key.clone());
                tokio::task::spawn_blocking(move || post(&url, key.as_deref(), &body))
                    .await
                    .map_err(|e| Error::Unavailable(format!("decision request: {e}")))??
            }
            Target::Mod { mods, name } => {
                let call = ModCall {
                    mod_name: name.clone(),
                    method: PLUGIN_METHOD.into(),
                    params: body,
                    idem_key: None,
                };
                tokio::time::timeout(TIMEOUT, mods.call(call))
                    .await
                    .map_err(|_| {
                        Error::Unavailable(format!("decision plugin `{name}` timed out"))
                    })??
            }
        };
        Decision::parse(question, &reply)
    }
}

/// Blocking request; run on the blocking pool.
fn post(base_url: &str, api_key: Option<&str>, body: &Value) -> Result<Value> {
    let url = format!("{base_url}/v1/systemone");
    let agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(TIMEOUT))
        .build()
        .new_agent();
    let mut request = agent.post(&url).content_type("application/json");
    if let Some(key) = api_key {
        request = request.header("authorization", format!("Bearer {key}"));
    }
    // Messages carry the status only: no key, no echoed task text.
    let what = format!("decision request to {url}");
    let mut response = request
        .send(body.to_string())
        .map_err(|e| Error::Unavailable(format!("{what}: {e}")))?;
    let status = response.status().as_u16();
    let text = response
        .body_mut()
        .read_to_string()
        .map_err(|e| Error::Unavailable(format!("{what}: {e}")))?;
    match status {
        200..=299 => serde_json::from_str(&text)
            .map_err(|e| Error::Protocol(format!("{what}: not JSON: {e}"))),
        429 | 500.. => Err(Error::Unavailable(format!("{what}: HTTP {status}"))),
        _ => Err(Error::InvalidInput(format!("{what}: HTTP {status}"))),
    }
}

/// The installed backend, shared by the engine handle and the executor, so
/// a model switch ([`super::Engine::replay_decisions`]) reaches both.
#[derive(Debug, Clone, Default)]
pub(crate) struct Backend(Arc<RwLock<Option<DecisionClient>>>);

impl Backend {
    pub(crate) fn new(client: Option<DecisionClient>) -> Self {
        Self(Arc::new(RwLock::new(client)))
    }

    /// The backend now, if one is installed.
    pub(crate) fn get(&self) -> Option<DecisionClient> {
        self.0.read().map(|c| c.clone()).unwrap_or_default()
    }

    fn set(&self, client: DecisionClient) {
        if let Ok(mut current) = self.0.write() {
            *current = Some(client);
        }
    }
}

/// A typed answer from the decision engine, with its calibrated confidence
/// and the model that gave it. Built only from a backend reply.
/// It converts into nothing: see the module docs.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Decision {
    answer: Answer,
    confidence: f64,
    model: String,
}

/// The answer to a typed question, keyed like the wire format.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Answer {
    /// The chosen label.
    Choice(String),
    /// The ordinal level, `0..levels-1` (may be fractional).
    Score(f64),
    /// Probability that the statement is true.
    Noul(f64),
}

impl Decision {
    /// The answer.
    pub fn answer(&self) -> &Answer {
        &self.answer
    }

    /// Calibrated probability of the answer, `0..=1`.
    pub fn confidence(&self) -> f64 {
        self.confidence
    }

    /// The model (checkpoint) that answered.
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Validates a `/v1/systemone` reply against the question asked (trust
    /// boundary: the label must be one of the criteria, numbers in range).
    pub(super) fn parse(question: &Question, reply: &Value) -> Result<Self> {
        let bad = |why: &str| Error::Protocol(format!("decision reply: {why}"));
        let got = &reply["answers"][QUESTION_ID];
        let unit = |v: &Value| v.as_f64().filter(|x| (0.0..=1.0).contains(x));
        let answer = match question.kind {
            QuestionKind::Choice => {
                let label = got["choice"].as_str().ok_or_else(|| bad("no choice"))?;
                if question.criteria.get(label).is_none() {
                    return Err(bad(&format!("`{label}` is not one of the criteria")));
                }
                Answer::Choice(label.to_owned())
            }
            QuestionKind::Score => {
                let top = question.criteria.as_array().map_or(0, Vec::len);
                let level = got["score"]
                    .as_f64()
                    .filter(|x| (0.0..=(top.saturating_sub(1) as f64)).contains(x))
                    .ok_or_else(|| bad("score missing or out of range"))?;
                Answer::Score(level)
            }
            QuestionKind::Noul => {
                Answer::Noul(unit(&got["noul"]).ok_or_else(|| bad("noul missing or not 0..=1"))?)
            }
        };
        // `answer_confidence` is Laya's calibrated probability; `confidence`
        // (distribution concentration) is the fallback.
        let confidence = unit(&got["answer_confidence"])
            .or_else(|| unit(&got["confidence"]))
            .ok_or_else(|| bad("confidence missing or not 0..=1"))?;
        let model = reply["routing"]["model"]
            .as_str()
            .or_else(|| reply["model"].as_str())
            .unwrap_or("unknown")
            .to_owned();
        Ok(Self {
            answer,
            confidence,
            model,
        })
    }
}

/// Act on `asked`, or the reason `fallback` (a human, the rule) decides
/// instead.
fn judge(
    asked: &Result<Decision>,
    threshold: f64,
    fallback: &str,
) -> std::result::Result<(), String> {
    match asked {
        Ok(d) if d.confidence >= threshold => Ok(()),
        Ok(d) => Err(format!(
            "confidence {:.2} is below the threshold {threshold:.2}: {fallback} decides",
            d.confidence
        )),
        Err(e) => Err(format!("decision backend failed ({e}): {fallback} decides")),
    }
}

// ------------------------------------------------------------ decision hook

/// Threshold of the engine's own questions: below it the rule decides.
// ponytail: one threshold for every hook; per-hook settings when a team
// needs one stricter than another.
pub(crate) const HOOK_THRESHOLD: f64 = 0.8;

/// A routine classification asked from engine code (the decision
/// hook): the watchdog, failover, the gate scope flag, correction causes
/// and trigger triage. The rule-based answer stands with no
/// backend installed, on a backend error, or below [`HOOK_THRESHOLD`].
#[derive(Debug, Clone)]
pub(crate) struct Hook {
    /// What it decides, on the timeline (`watchdog`, `failover`, ...).
    pub(crate) decides: &'static str,
    pub(crate) state: Value,
    pub(crate) question: Question,
    /// The rule-based answer, wire shape (`{"choice": "stuck"}`), or null.
    pub(crate) rule: Value,
}

/// What a [`Hook`] settled on.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Settled {
    /// The answer to act on, wire shape: the model's, else the rule's.
    pub(crate) answer: Value,
    /// The `decision` event to journal; `None` when no backend was asked.
    pub(crate) event: Option<Value>,
}

impl Hook {
    /// `kind` question with `instructions` and `criteria` about `state`.
    pub(crate) fn new(
        decides: &'static str,
        kind: QuestionKind,
        instructions: &str,
        criteria: Value,
        state: Value,
        rule: Value,
    ) -> Self {
        let question = Question {
            kind,
            instructions: instructions.to_owned(),
            criteria,
        };
        Self {
            decides,
            state,
            question,
            rule,
        }
    }

    /// Asks `client`; with none the rule stands and nothing is journalled.
    pub(crate) async fn ask(self, client: Option<DecisionClient>) -> Settled {
        let asked = match &client {
            Some(client) => Some(client.ask(self.state.clone(), &self.question).await),
            None => None,
        };
        self.settle(asked)
    }

    fn settle(self, asked: Option<Result<Decision>>) -> Settled {
        let Some(asked) = asked else {
            return Settled {
                answer: self.rule,
                event: None,
            };
        };
        let verdict = judge(&asked, HOOK_THRESHOLD, "the rule");
        let decision = asked.as_ref().ok();
        let answer = match (&verdict, decision) {
            (Ok(()), Some(d)) => json!(d.answer()),
            _ => self.rule.clone(),
        };
        let event = json!({
            "decides": self.decides,
            "threshold": HOOK_THRESHOLD,
            "acted": verdict.is_ok(),
            "reason": verdict.err(),
            "answer": decision.map(Decision::answer),
            "confidence": decision.map(Decision::confidence),
            "model": decision.map(Decision::model),
            "rule": self.rule,
            "question": self.question,
            "state": self.state,
        });
        Settled {
            answer,
            event: Some(event),
        }
    }
}

/// Hook names, as `decides` on the timeline.
pub(super) const WATCHDOG: &str = "watchdog";
pub(super) const FAILOVER: &str = "failover";
const SCOPE: &str = "scope";

/// The task as a question's state (redacted).
pub(crate) fn task_state(task: &sun_core::model::Task) -> Value {
    json!({ "title": redact(&task.title, &[]), "body": redact(&task.body, &[]) })
}

/// "Is the diff in scope for the task?" (gates). The gate step's
/// job adds the `changes`. Raised, it pauses the run for review; it never
/// passes or fails a gate. Rule: in scope.
pub(super) fn scope_hook(task: &sun_core::model::Task) -> Hook {
    Hook::new(
        SCOPE,
        QuestionKind::Noul,
        "Do these changes go beyond what the task asks for?",
        json!("The changes include work the task did not ask for."),
        task_state(task),
        json!({ "noul": 0.0 }),
    )
}

/// Trigger triage: what an incoming issue or message is.
#[derive(Debug, Default)]
pub(crate) struct Triage {
    /// `bug`, `feature`, `chore` or `question`; the rule leaves it unset.
    pub(crate) kind: Option<String>,
    /// Task priority, 0 (low) to 3 (urgent); the rule says 0.
    pub(crate) priority: i32,
    /// What each question settled on, to journal on the new run.
    pub(crate) settled: Vec<Settled>,
}

/// Triages `state` (the new task's title and body): its type and priority.
/// The repos a run works in come from the text (`repo_resolve`), not from
/// a question.
pub(crate) async fn triage(client: Option<DecisionClient>, state: Value) -> Triage {
    let kind = Hook::new(
        "triage_type",
        QuestionKind::Choice,
        "What kind of work is this?",
        json!({
            "bug": "something is broken or wrong",
            "feature": "new or changed behaviour",
            "chore": "maintenance: dependencies, docs, cleanup",
            "question": "a question or a request for information",
        }),
        state.clone(),
        Value::Null,
    );
    let priority = Hook::new(
        "triage_priority",
        QuestionKind::Score,
        "How urgent is this work?",
        json!(["low", "normal", "high", "urgent"]),
        state,
        json!({ "score": 0 }),
    );
    let mut out = Triage::default();
    let kind = kind.ask(client.clone()).await;
    out.kind = kind.choice().map(str::to_owned);
    let priority = priority.ask(client).await;
    out.priority = priority_of(&priority.answer).unwrap_or_default();
    out.settled = vec![kind, priority];
    out
}

impl Executor {
    /// A step's decision hook answered ([`Effect::Ask`]).
    pub(super) async fn on_hooked(
        &mut self,
        step_id: StepId,
        decides: &'static str,
        settled: Settled,
    ) -> Result<()> {
        match decides {
            WATCHDOG => self.on_stall_judged(step_id, settled).await,
            FAILOVER => self.on_limit_judged(step_id, settled).await,
            _ => Ok(()),
        }
    }
}

impl Settled {
    /// The chosen label, if the answer is a choice.
    pub(crate) fn choice(&self) -> Option<&str> {
        self.answer["choice"].as_str()
    }

    /// Journals the decision on run `run_id` (and step), if a backend was asked.
    pub(crate) async fn journal(
        &self,
        tx: &mut Tx,
        run_id: RunId,
        step_key: Option<&StepKey>,
    ) -> Result<()> {
        if let Some(event) = &self.event {
            let event = NewEvent::new(run_id, step_key.cloned(), DECISION, event.clone());
            tx.append_event(event).await?;
        }
        Ok(())
    }
}

/// One question to ask off the engine task.
pub(super) struct Job {
    client: DecisionClient,
    state: Value,
    question: Question,
}

impl Job {
    pub(super) async fn run(self) -> Result<Decision> {
        self.client.ask(self.state, &self.question).await
    }
}

impl Executor {
    /// A running decide step asks on start and after a restart. With no
    /// backend installed it escalates on the spot.
    pub(super) async fn ask_decision(
        &mut self,
        tx: &mut Tx,
        task_id: TaskId,
        question: &Question,
        step: &mut Step,
        effects: &mut Vec<Effect>,
    ) -> Result<()> {
        let Some(client) = self.decisions.get() else {
            self.gating.remove(&step.id);
            let asked = Err(Error::Unavailable("no decision backend installed".into()));
            return self.decided(tx, step, asked, effects).await;
        };
        let task = tx.get_task(task_id).await?;
        let state = task_state(&task);
        effects.push(Effect::Decide {
            step_id: step.id,
            job: Job {
                client,
                state,
                question: question.clone(),
            },
        });
        Ok(())
    }

    /// Records an answer (or a failure to get one) and moves its step.
    pub(super) async fn on_decided(
        &mut self,
        step_id: StepId,
        asked: Result<Decision>,
    ) -> Result<()> {
        self.gating.remove(&step_id);
        let mut tx = self.store.begin().await?;
        let mut step = tx.get_step(step_id).await?;
        let mut effects = Vec::new();
        // Anything else: the run ended (or the step was cancelled) meanwhile.
        if step.status == StepStatus::Running {
            self.decided(&mut tx, &mut step, asked, &mut effects)
                .await?;
            self.advance(&mut tx, step.run_id, &mut effects).await?;
        }
        tx.commit().await?;
        self.apply(effects).await
    }

    /// Journals the decision, then acts or escalates.
    async fn decided(
        &mut self,
        tx: &mut Tx,
        step: &mut Step,
        asked: Result<Decision>,
        effects: &mut Vec<Effect>,
    ) -> Result<()> {
        let run = tx.get_run(step.run_id).await?;
        let workflow = self.workflow(tx, &run).await?;
        let (decides, threshold, question) = workflow
            .steps
            .iter()
            .find(|d| d.id == step.step_key)
            .and_then(|d| match &d.kind {
                StepKind::Decide {
                    decides,
                    threshold,
                    question,
                    ..
                } => Some((*decides, *threshold, question.clone())),
                _ => None,
            })
            .ok_or_else(|| Error::NotFound(format!("decide step `{}`", step.step_key)))?;
        let verdict = judge(&asked, threshold, "a human");
        let decision = asked.as_ref().ok();
        let payload = json!({
            "decides": decides,
            "threshold": threshold,
            "acted": verdict.is_ok(),
            "reason": verdict.as_ref().err(),
            "answer": decision.map(Decision::answer),
            "confidence": decision.map(Decision::confidence),
            "model": decision.map(Decision::model),
            // What a human may answer instead (the UI's choices).
            "question": question,
        });
        append(tx, step, DECISION, payload).await?;
        match (verdict, decision) {
            (Ok(()), Some(decision)) => {
                self.finish(tx, step, Ok(())).await?;
                let answer = json!(decision.answer());
                self.act(tx, step, decides, &answer, effects).await
            }
            (Ok(()), None) => self.finish(tx, step, Ok(())).await,
            (Err(why), _) => move_step(tx, step, StepStatus::WaitingApproval, Some(&why)).await,
        }
    }

    /// A human approved an escalated decide step: Sun acts on the decision
    /// engine's suggestion, if it gave one (none: the step just succeeds).
    pub(super) async fn accept_suggestion(
        &mut self,
        tx: &mut Tx,
        step: &Step,
        by: &Principal,
        effects: &mut Vec<Effect>,
    ) -> Result<()> {
        let run = tx.get_run(step.run_id).await?;
        let workflow = self.workflow(tx, &run).await?;
        let Some(decides) = workflow
            .steps
            .iter()
            .find(|d| d.id == step.step_key)
            .and_then(|d| match &d.kind {
                StepKind::Decide { decides, .. } => Some(*decides),
                _ => None,
            })
        else {
            return Ok(());
        };
        let suggested = tx
            .events_after(Some(step.run_id), 0, u32::MAX)
            .await?
            .into_iter()
            .rfind(|e| e.kind == DECISION && e.step_key.as_ref() == Some(&step.step_key))
            .map(|e| e.payload["answer"].clone())
            .filter(|answer| !answer.is_null());
        let Some(answer) = suggested else {
            return Ok(());
        };
        let payload = json!({
            "decides": decides,
            "acted": true,
            "reason": "suggestion approved by a human",
            "answer": answer,
            "by": by.user,
        });
        append(tx, step, DECISION, payload).await?;
        self.act(tx, step, decides, &answer, effects).await
    }

    /// Acts on the answer (wire shape, `{"choice": "x"}`) of a decide step
    /// that just succeeded. `triage` sets the task priority from a score
    /// level or a numeric label; `flag`, when raised, pauses the run for a
    /// human (task `needs_attention`, resume to go on); `route` and
    /// `add_check` are read back from the log where they apply ([`routed`],
    /// [`added_checks`]).
    pub(super) async fn act(
        &mut self,
        tx: &mut Tx,
        step: &Step,
        decides: DecisionUse,
        answer: &Value,
        effects: &mut Vec<Effect>,
    ) -> Result<()> {
        match decides {
            DecisionUse::Triage => {
                if let Some(priority) = priority_of(answer) {
                    let task_id = tx.get_run(step.run_id).await?.task_id;
                    let level = sun_core::model::Priority::from_level(priority);
                    super::tasks::set_priority(tx, task_id, level, None).await?;
                    append(tx, step, TRIAGED, json!({ "priority": priority })).await?;
                }
            }
            DecisionUse::Flag => {
                let running = tx.get_run(step.run_id).await?.status == RunStatus::Running;
                if running && raised(answer) {
                    let why = format!("flagged for review by `{}`: {answer}", step.step_key);
                    self.pause_in(tx, step.run_id, &why, effects).await?;
                }
            }
            DecisionUse::Route | DecisionUse::AddCheck => {}
        }
        Ok(())
    }
}

/// Event kind: a triage decision set the task priority `{priority}`.
const TRIAGED: &str = "triaged";

/// Labels that answer "nothing to flag" or "no extra check".
fn is_none(label: &str) -> bool {
    ["none", "no", "ok"]
        .iter()
        .any(|n| label.eq_ignore_ascii_case(n))
}

/// Task priority from a triage answer: the score level, or a numeric label.
fn priority_of(answer: &Value) -> Option<i32> {
    match (answer["score"].as_f64(), answer["choice"].as_str()) {
        (Some(level), _) => Some(level.round() as i32),
        (_, Some(label)) => label.trim().parse().ok(),
        _ => None,
    }
}

/// Whether a flag answer raises the flag: a likely statement (noul ≥ 0.5),
/// a score of 1 or more, or a label other than none/no/ok.
fn raised(answer: &Value) -> bool {
    match (answer["noul"].as_f64(), answer["score"].as_f64()) {
        (Some(p), _) => p >= 0.5,
        (_, Some(level)) => level >= 1.0,
        _ => answer["choice"]
            .as_str()
            .is_some_and(|label| !is_none(label)),
    }
}

/// Answers of the run's acted `decides` decisions, oldest first.
async fn acted(tx: &mut Tx, run_id: RunId, decides: DecisionUse) -> Result<Vec<Value>> {
    let decides = json!(decides);
    Ok(tx
        .events_after(Some(run_id), 0, u32::MAX)
        .await?
        .into_iter()
        .filter(|e| e.kind == DECISION && e.payload["acted"] == true)
        .filter(|e| e.payload["decides"] == decides)
        .map(|e| e.payload["answer"].clone())
        .collect())
}

/// The runtime the run's latest `route` decision picked: its choice label.
pub(super) async fn routed(tx: &mut Tx, run_id: RunId) -> Result<Option<String>> {
    Ok(acted(tx, run_id, DecisionUse::Route)
        .await?
        .into_iter()
        .filter_map(|answer| answer["choice"].as_str().map(str::to_owned))
        .next_back())
}

/// Gates the run's `add_check` decisions asked for (choice labels naming a
/// gate that runs without params, e.g. `test`), for every later verify.
pub(super) async fn added_checks(tx: &mut Tx, run_id: RunId) -> Result<Vec<GateSpec>> {
    Ok(acted(tx, run_id, DecisionUse::AddCheck)
        .await?
        .into_iter()
        .filter_map(|answer| answer["choice"].as_str().map(str::to_owned))
        .filter(|name| !is_none(name))
        .map(|name| GateSpec { name, params: None })
        .filter(gates::resolves)
        .collect())
}

// ------------------------------------------------------ export and replay

/// Kernel setting: the base URL of the backend a replay switched in. It
/// beats sun.yaml's `decisions.url` on the next boot.
const SWITCHED_URL: &str = "decisions_url";
/// Most past decisions one replay asks again, newest first.
const MAX_REPLAY: usize = 500;

/// One past decision a model answered, with the human's answer if a human
/// gave one (the ground truth).
struct Past {
    state: Value,
    question: Question,
    threshold: f64,
    acted: bool,
    answer: Value,
    confidence: f64,
    model: String,
    truth: Option<Value>,
}

/// Every journalled decision a backend answered, oldest first.
// ponytail: one scan of every `decision` event per export or replay; page
// it when the journal holds more than a few thousand.
async fn past_decisions(tx: &mut Tx) -> Result<Vec<Past>> {
    let events = tx.events_of_kind(DECISION).await?;
    let mut truth: HashMap<(RunId, StepKey), Value> = HashMap::new();
    for e in &events {
        if let (Some(_), Some(key)) = (e.payload["by"].as_str(), &e.step_key) {
            truth.insert((e.run_id, key.clone()), e.payload["answer"].clone());
        }
    }
    let mut out = Vec::new();
    for e in events {
        let p = &e.payload;
        let question = serde_json::from_value::<Question>(p["question"].clone());
        let (Some(confidence), Ok(question)) = (p["confidence"].as_f64(), question) else {
            continue;
        };
        // A decide step asked about its task; hooks journal their state.
        let state = match p.get("state").filter(|s| !s.is_null()) {
            Some(state) => state.clone(),
            None => {
                let task_id = tx.get_run(e.run_id).await?.task_id;
                let task = tx.get_task(task_id).await?;
                task_state(&task)
            }
        };
        let truth = (e.step_key.as_ref()).and_then(|k| truth.get(&(e.run_id, k.clone())));
        out.push(Past {
            state,
            question,
            threshold: p["threshold"].as_f64().unwrap_or(HOOK_THRESHOLD),
            acted: p["acted"] == true,
            answer: p["answer"].clone(),
            confidence,
            model: p["model"].as_str().unwrap_or("unknown").to_owned(),
            truth: truth.cloned(),
        });
    }
    Ok(out)
}

/// One model's record over past decisions.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Tally {
    /// Decisions asked.
    pub asked: u32,
    /// Below the threshold (or failed): a human or the rule decided.
    pub escalated: u32,
    /// Answers a human answered differently.
    pub overridden: u32,
}

impl Tally {
    fn count(&mut self, escalated: bool, answer: &Value, truth: Option<&Value>) {
        self.asked += 1;
        self.escalated += u32::from(escalated);
        let wrong = truth.is_some_and(|t| !answer.is_null() && t != answer);
        self.overridden += u32::from(wrong);
    }
}

/// A candidate model replayed against the past decisions.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DecisionReplay {
    /// The candidate's base URL.
    pub candidate_url: String,
    /// The model(s) that answered then.
    pub current: Tally,
    /// The candidate on the same questions.
    pub candidate: Tally,
    /// The switch rule held and the candidate is now the backend.
    pub switched: bool,
}

/// The switch rule: the candidate escalates less without more overrides.
fn better(current: Tally, candidate: Tally) -> bool {
    candidate.escalated < current.escalated && candidate.overridden <= current.overridden
}

/// sun.yaml's backend, or the one a replay switched in (same key).
pub(super) async fn switched(
    tx: &mut Tx,
    configured: Option<DecisionClient>,
) -> Result<Option<DecisionClient>> {
    let url = tx.setting(SWITCHED_URL).await?;
    Ok(match (configured, url.as_ref().and_then(Value::as_str)) {
        (Some(client), Some(url)) if client.url().is_some() => Some(client.at(url)?),
        (configured, _) => configured,
    })
}

impl super::Engine {
    /// Human-answered decisions as JSONL fine-tuning rows for the
    /// typed-decisions model, oldest first, one per line:
    /// `{state, question, answer, model_answer, model, confidence,
    /// overridden}`. `answer` is the human's; `overridden` marks a
    /// `decision_override` correction. Task text is redacted.
    pub async fn export_decisions(&self) -> Result<String> {
        let past = past_decisions(&mut self.store.read().await?).await?;
        let mut out = String::new();
        for p in past {
            let Some(truth) = p.truth else { continue };
            let row = json!({
                "state": p.state,
                "question": p.question,
                "overridden": truth != p.answer,
                "answer": truth,
                "model_answer": p.answer,
                "model": p.model,
                "confidence": p.confidence,
            });
            out.push_str(&row.to_string());
            out.push('\n');
        }
        Ok(out)
    }

    /// Replays the latest past decisions on the backend at `candidate_url`
    /// (a new model version) and switches it in, for this process and the
    /// next boot, only when it escalates less without more overrides.
    pub async fn replay_decisions(&self, candidate_url: &str) -> Result<DecisionReplay> {
        let candidate = match self.decisions.get() {
            Some(current) => current.at(candidate_url)?,
            None => DecisionClient::new(candidate_url, None)?,
        };
        let mut past = past_decisions(&mut self.store.read().await?).await?;
        let skip = past.len().saturating_sub(MAX_REPLAY);
        past.drain(..skip);
        let (mut current, mut next) = (Tally::default(), Tally::default());
        for p in &past {
            current.count(!p.acted, &p.answer, p.truth.as_ref());
            let asked = candidate.ask(p.state.clone(), &p.question).await;
            let answer = asked.as_ref().map_or(Value::Null, |d| json!(d.answer()));
            let escalated = judge(&asked, p.threshold, "").is_err();
            next.count(escalated, &answer, p.truth.as_ref());
        }
        let switched = better(current, next);
        if switched {
            let mut tx = self.store.begin().await?;
            tx.put_setting(SWITCHED_URL, &json!(candidate_url)).await?;
            tx.commit().await?;
            self.decisions.set(candidate);
        }
        Ok(DecisionReplay {
            candidate_url: candidate_url.to_owned(),
            current,
            candidate: next,
            switched,
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    use sun_core::{HostId, RunStatus, StepKey};
    use sun_store::Store;

    use super::*;
    use crate::gates::tests::{LocalHost, events, mods, seed, steps, until_run_ends};
    use crate::{Engine, EngineConfig, Principal};

    /// A fake `/v1/systemone`: answers every request with `status` and
    /// `body`, and keeps each request (head and body).
    fn fake(status: u16, body: &'static str) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let requests = seen.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let (mut request, mut len) = (String::new(), 0);
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if let Some(n) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        len = n.trim().parse().unwrap();
                    }
                    if line == "\r\n" {
                        break;
                    }
                    request.push_str(&line);
                }
                let mut payload = vec![0; len];
                reader.read_exact(&mut payload).unwrap();
                request.push_str(&String::from_utf8(payload).unwrap());
                seen.lock().unwrap().push(request);
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        (url, requests)
    }

    /// A Laya reply choosing `billing`.
    const BILLING: &str = r#"{"answers": {"decision": {"choice": "billing", "confidence": 0.7,
        "answer_confidence": 0.93, "probabilities": {"billing": 0.93, "tech": 0.07}}},
        "routing": {"model": "english", "reason": "auto"}, "usage": {"input_tokens": 9, "output_tokens": 1}}"#;
    /// The same with a low calibrated confidence.
    const UNSURE: &str = r#"{"answers": {"decision": {"choice": "tech", "answer_confidence": 0.41}},
        "routing": {"model": "english"}}"#;

    fn choice() -> Question {
        serde_json::from_value(json!({
            "type": "choice", "instructions": "which team?",
            "criteria": {"billing": "refunds", "tech": "bugs"},
        }))
        .unwrap()
    }

    #[tokio::test]
    async fn decision_client_asks_systemone_and_reads_a_typed_answer() {
        let (url, requests) = fake(200, BILLING);
        let client = DecisionClient::new(&format!("{url}/"), Some("k3y".into())).unwrap();
        let decision = client
            .ask(json!({"title": "billed twice"}), &choice())
            .await
            .unwrap();
        assert_eq!(decision.answer(), &Answer::Choice("billing".into()));
        assert_eq!(decision.confidence(), 0.93, "the calibrated confidence");
        assert_eq!(decision.model(), "english");

        let request = requests.lock().unwrap()[0].clone();
        assert!(
            request.starts_with("POST /v1/systemone HTTP/1.1"),
            "{request}"
        );
        assert!(
            request
                .to_ascii_lowercase()
                .contains("authorization: bearer k3y")
        );
        let body: Value = serde_json::from_str(&request[request.find('{').unwrap()..]).unwrap();
        assert_eq!(body["state"]["title"], "billed twice");
        assert_eq!(body["questions"]["decision"]["type"], "choice");
        assert_eq!(body["questions"]["decision"]["criteria"]["tech"], "bugs");
    }

    #[tokio::test]
    async fn decision_client_rejects_bad_replies_and_maps_http_errors() {
        let cases: [(u16, &'static str, &str); 5] = [
            (
                200,
                r#"{"answers": {"decision": {"choice": "sales", "answer_confidence": 0.9}}}"#,
                "not one of the criteria",
            ),
            (
                200,
                r#"{"answers": {"decision": {"choice": "tech", "answer_confidence": 1.7}}}"#,
                "confidence",
            ),
            (200, "not json", "not JSON"),
            (503, "{}", "HTTP 503"),
            (401, "{}", "HTTP 401"),
        ];
        for (status, body, want) in cases {
            let (url, _) = fake(status, body);
            let client = DecisionClient::new(&url, None).unwrap();
            let err = client.ask(json!({}), &choice()).await.unwrap_err();
            assert!(err.to_string().contains(want), "{status} {body}: {err}");
            let unavailable = matches!(err, Error::Unavailable(_));
            assert_eq!(unavailable, status == 503, "{err}");
        }
        let score: Question = serde_json::from_value(json!({
            "type": "score", "instructions": "how urgent?", "criteria": ["low", "high"],
        }))
        .unwrap();
        let reply = json!({"answers": {"decision": {"score": 2.5, "answer_confidence": 0.9}}});
        assert!(
            Decision::parse(&score, &reply).is_err(),
            "level past the scale"
        );
        assert!(DecisionClient::new("ftp://laya", None).is_err());
    }

    #[test]
    fn decision_threshold_acts_at_or_above_and_escalates_below() {
        let at = |confidence| {
            Ok(Decision {
                answer: Answer::Noul(0.9),
                confidence,
                model: "m".into(),
            })
        };
        assert_eq!(judge(&at(0.8), 0.8, "a human"), Ok(()));
        assert_eq!(judge(&at(0.95), 0.8, "a human"), Ok(()));
        assert!(
            judge(&at(0.79), 0.8, "a human")
                .unwrap_err()
                .contains("below the threshold 0.80")
        );
        let down = Err(Error::Unavailable("down".into()));
        assert!(
            judge(&down, 0.0, "a human")
                .unwrap_err()
                .contains("a human decides")
        );
    }

    // ------------------------------------------------------------ engine

    /// Triage, then a human approval the decision must never skip.
    const TRIAGE: &str = r#"
name: triage
steps:
  - id: triage
    type: decide
    decides: triage
    question: { type: choice, instructions: which team?, criteria: { billing: refunds, tech: bugs } }
    threshold: 0.8
    who: [role:lead]
  - id: review
    type: approval
    who: [role:lead]
"#;

    async fn start(
        store: &Store,
        dir: &std::path::Path,
        decisions: Option<DecisionClient>,
    ) -> Engine {
        let config = EngineConfig {
            host: HostId::generate(),
            adapter: "fake".into(),
            delivery_timeout: Duration::from_secs(5),
            watchdog: crate::WatchdogConfig::default(),
        };
        let host = LocalHost::new(dir, |_, _| {});
        let (engine, _task) = Engine::start_with(
            store.clone(),
            host,
            mods(json!({})),
            config,
            decisions,
            None,
        )
        .await
        .unwrap();
        engine
    }

    async fn status(store: &Store, run: sun_core::RunId, key: &str) -> StepStatus {
        crate::state::latest(steps(store, run).await)[&StepKey::new(key).unwrap()].status
    }

    async fn until(store: &Store, run: sun_core::RunId, key: &str, want: StepStatus) {
        for _ in 0..2_000 {
            let latest = crate::state::latest(steps(store, run).await);
            if latest.get(&StepKey::new(key).unwrap()).map(|s| s.status) == Some(want) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("`{key}` never {want}");
    }

    fn lead() -> Principal {
        Principal {
            user: "ana".into(),
            roles: vec!["lead".into()],
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn decision_above_threshold_acts_but_never_skips_the_approval() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).await.unwrap();
        let (url, requests) = fake(200, BILLING);
        let engine = start(
            &store,
            dir.path(),
            Some(DecisionClient::new(&url, None).unwrap()),
        )
        .await;
        let run = seed(&store, TRIAGE).await;
        engine.start_run(run).await.unwrap();

        until(&store, run, "triage", StepStatus::Succeeded).await;
        until(&store, run, "review", StepStatus::WaitingApproval).await;
        let decision = &events(&store, run, DECISION).await[0];
        assert_eq!(decision["acted"], true);
        assert_eq!(decision["decides"], "triage");
        assert_eq!(decision["answer"], json!({"choice": "billing"}));
        assert_eq!(decision["confidence"], 0.93);
        assert_eq!(decision["model"], "english");
        assert!(
            requests.lock().unwrap()[0].contains(r#""title":"t""#),
            "task is the state"
        );
        // The decision moved nothing else: the approval waits for a human.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            status(&store, run, "review").await,
            StepStatus::WaitingApproval
        );
        engine
            .decide(run, StepKey::new("review").unwrap(), true, lead())
            .await
            .unwrap();
        assert_eq!(
            until_run_ends(&store, run).await.status,
            RunStatus::Succeeded
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn decision_below_threshold_or_without_backend_escalates_to_a_human() {
        for backend in [Some(UNSURE), None] {
            let dir = tempfile::tempdir().unwrap();
            let store = Store::open(dir.path()).await.unwrap();
            let client = backend.map(|body| DecisionClient::new(&fake(200, body).0, None).unwrap());
            let engine = start(&store, dir.path(), client).await;
            let run = seed(&store, TRIAGE).await;
            engine.start_run(run).await.unwrap();

            until(&store, run, "triage", StepStatus::WaitingApproval).await;
            let decision = &events(&store, run, DECISION).await[0];
            assert_eq!(decision["acted"], false);
            let reason = decision["reason"].as_str().unwrap();
            match backend {
                Some(_) => {
                    assert!(reason.contains("confidence 0.41 is below"), "{reason}");
                    assert_eq!(decision["model"], "english");
                }
                None => assert!(reason.contains("no decision backend"), "{reason}"),
            }
            // Only an authorized human moves it on.
            let intruder = Principal {
                user: "bob".into(),
                roles: vec![],
            };
            let triage = StepKey::new("triage").unwrap();
            assert!(
                engine
                    .decide(run, triage.clone(), true, intruder)
                    .await
                    .is_err()
            );
            engine.decide(run, triage, true, lead()).await.unwrap();
            until(&store, run, "review", StepStatus::WaitingApproval).await;
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn correction_of_an_escalated_decision_and_a_reject_are_recorded() {
        use sun_core::model::CorrectionCause::{Other, WrongScope};
        use sun_core::model::CorrectionKind::{DecisionOverride, Reject};

        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).await.unwrap();
        let (url, _) = fake(200, UNSURE);
        let backend = DecisionClient::new(&url, None).unwrap();
        let engine = start(&store, dir.path(), Some(backend)).await;
        let run = seed(&store, TRIAGE).await;
        engine.start_run(run).await.unwrap();
        until(&store, run, "triage", StepStatus::WaitingApproval).await;

        // The answer is validated like a backend reply; only `who:` answers.
        let triage = StepKey::new("triage").unwrap();
        let answer = |label: &str| json!({ "choice": label });
        let bad = engine
            .correct_decision(run, triage.clone(), lead(), answer("sales"))
            .await;
        assert!(matches!(bad, Err(Error::InvalidInput(_))), "{bad:?}");
        let intruder = Principal {
            user: "bob".into(),
            roles: vec![],
        };
        let denied = engine
            .correct_decision(run, triage.clone(), intruder, answer("billing"))
            .await;
        assert!(denied.is_err());
        engine
            .correct_decision(run, triage, lead(), answer("billing"))
            .await
            .unwrap();
        until(&store, run, "review", StepStatus::WaitingApproval).await;
        let why = "This touches unrelated files in the auth module".to_owned();
        let review = StepKey::new("review").unwrap();
        engine.reject(run, review, lead(), why).await.unwrap();
        until_run_ends(&store, run).await;

        let decisions = events(&store, run, DECISION).await;
        assert_eq!(decisions[1]["answer"], answer("billing"));
        assert_eq!(decisions[1]["by"], "ana");
        let task_id = store
            .read()
            .await
            .unwrap()
            .get_run(run)
            .await
            .unwrap()
            .task_id;
        let rows = engine.corrections_of(task_id).await.unwrap();
        let got: Vec<_> = rows
            .iter()
            .map(|c| {
                (
                    c.kind,
                    c.cause_label,
                    c.step_key.clone().unwrap().to_string(),
                )
            })
            .collect();
        assert_eq!(
            got,
            [
                (DecisionOverride, Other, "triage".to_owned()),
                (Reject, WrongScope, "review".to_owned()),
            ]
        );
        let body = &rows[0].body;
        assert!(
            body.contains(r#"answered {"choice":"tech"} (confidence 0.41)"#),
            "{body}"
        );
        assert!(
            body.contains(r#"ana answered {"choice":"billing"}"#),
            "{body}"
        );
        assert_eq!(
            rows[1].body,
            "This touches unrelated files in the auth module"
        );
        assert_eq!(rows[0].classified_by, "rules");
    }

    #[test]
    fn triage_and_flag_read_every_answer_kind() {
        assert_eq!(priority_of(&json!({"score": 2.4})), Some(2));
        assert_eq!(priority_of(&json!({"choice": "3"})), Some(3));
        assert_eq!(priority_of(&json!({"choice": "billing"})), None);
        assert_eq!(priority_of(&json!({"noul": 0.9})), None);
        assert!(raised(&json!({"noul": 0.5})) && !raised(&json!({"noul": 0.49})));
        assert!(raised(&json!({"score": 1})) && !raised(&json!({"score": 0})));
        assert!(raised(&json!({"choice": "scope"})) && !raised(&json!({"choice": "None"})));
    }

    /// Every purpose, through each way an answer lands: the model acting
    /// (triage, route), a human's own answer (add_check) and a human
    /// approving the model's suggestion (flag).
    const PURPOSES: &str = r#"
name: purposes
runtime:
  chain: [fake-a, fake-b]
steps:
  - id: triage
    type: decide
    decides: triage
    question: { type: score, instructions: how urgent?, criteria: [low, mid, high] }
    threshold: 0.8
    who: [role:lead]
  - id: route
    type: decide
    decides: route
    question: { type: choice, instructions: which runtime?, criteria: { fake-a: a, fake-b: b } }
    threshold: 0.8
    who: [role:lead]
    needs: [triage]
  - id: check
    type: decide
    decides: add_check
    question: { type: choice, instructions: more checks?, criteria: { fake-b: b, diff_not_empty: a change, none: no } }
    threshold: 1.0
    who: [role:lead]
    needs: [route]
  - id: implement
    type: agent
    prompt: do it
    needs: [check]
  - id: verify
    type: gate
    gates: [{test: {command: "true"}}]
    needs: [implement]
  - id: flag
    type: decide
    decides: flag
    question: { type: noul, instructions: risky?, criteria: it is risky }
    threshold: 1.0
    who: [role:lead]
    needs: [verify]
"#;

    #[tokio::test(flavor = "multi_thread")]
    async fn decisions_triage_route_add_checks_and_flag() {
        use sun_core::TaskStatus;

        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).await.unwrap();
        // One reply that fits every question kind, at 0.95.
        let (url, _) = fake(
            200,
            r#"{"answers": {"decision": {"choice": "fake-b", "score": 2, "noul": 0.9, "answer_confidence": 0.95}}}"#,
        );
        let config = EngineConfig {
            host: HostId::generate(),
            adapter: "fake".into(),
            delivery_timeout: Duration::from_secs(5),
            watchdog: crate::WatchdogConfig::default(),
        };
        let host = LocalHost::new(dir.path(), |dir, _| {
            std::fs::write(dir.join("change.txt"), "x").unwrap();
        });
        let client = DecisionClient::new(&url, None).unwrap();
        let (engine, _task) = Engine::start_with(
            store.clone(),
            host,
            mods(json!({})),
            config,
            Some(client),
            None,
        )
        .await
        .unwrap();
        let run = seed(&store, PURPOSES).await;
        let task_id = engine.run(run).await.unwrap().task_id;
        engine.start_run(run).await.unwrap();

        // The model acts on triage (priority = level) and route.
        until(&store, run, "check", StepStatus::WaitingApproval).await;
        assert_eq!(
            engine.task(task_id).await.unwrap().priority,
            sun_core::model::Priority::High
        );
        // A human answers add_check: the gate step also runs that gate.
        let answer = json!({ "choice": "diff_not_empty" });
        engine
            .correct_decision(run, StepKey::new("check").unwrap(), lead(), answer)
            .await
            .unwrap();
        // The passed gate's diff is out of scope (0.9): flagged, the run
        // pauses for review, and the gate still passed.
        until(&store, run, "verify", StepStatus::Succeeded).await;
        for _ in 0..500 {
            if engine.run(run).await.unwrap().status == RunStatus::Paused {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let scope = events(&store, run, DECISION).await;
        let scope = scope.iter().find(|d| d["decides"] == SCOPE).unwrap();
        assert_eq!(
            (scope["acted"].clone(), scope["rule"].clone()),
            (json!(true), json!({"noul": 0.0}))
        );
        assert!(
            scope["state"]["changes"]
                .as_str()
                .unwrap()
                .contains("change.txt")
        );
        engine.resume(run).await.unwrap();
        until(&store, run, "flag", StepStatus::WaitingApproval).await;
        let assigned = events(&store, run, "session_assigned").await;
        assert_eq!(assigned[0]["adapter"], "fake-b", "routed past fake-a");
        let gates: Vec<_> = events(&store, run, "gate_result")
            .await
            .iter()
            .map(|e| e["result"]["name"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(gates, ["test", "diff_not_empty"]);

        // Approving the suggested flag (0.9: risky) pauses the run.
        engine
            .decide(run, StepKey::new("flag").unwrap(), true, lead())
            .await
            .unwrap();
        let paused = engine.run(run).await.unwrap();
        assert_eq!(paused.status, RunStatus::Paused);
        let task = engine.task(task_id).await.unwrap();
        // A raised flag waits for a person to decide: Needs you, not Blocked.
        assert_eq!(task.status, TaskStatus::NeedsYou);
        engine.resume(run).await.unwrap();
        assert_eq!(
            until_run_ends(&store, run).await.status,
            RunStatus::Succeeded
        );
    }

    // ------------------------------------------------------------ hooks

    #[tokio::test]
    async fn no_backend_keeps_rule_results() {
        let task = sun_core::model::Task {
            title: "fix login".into(),
            workflow_id: Some(sun_core::WorkflowId::generate()),
            status: sun_core::TaskStatus::Backlog,
            ..sun_core::model::Task::new(sun_core::WorkspaceId::MAIN, "")
        };
        let settled = scope_hook(&task).ask(None).await;
        assert_eq!(settled.answer, json!({"noul": 0.0}), "in scope by the rule");
        assert_eq!(settled.event, None, "no backend, nothing journalled");

        let rules = triage(None, json!({})).await;
        assert_eq!((rules.kind, rules.priority), (None, 0));
        assert!(rules.settled.iter().all(|s| s.event.is_none()));

        // Below the threshold or failing, the rule decides; the model's
        // answer still goes on the timeline.
        let unsure = Decision {
            answer: Answer::Noul(0.9),
            confidence: 0.5,
            model: "m".into(),
        };
        let low = scope_hook(&task).settle(Some(Ok(unsure)));
        assert_eq!(low.answer, json!({"noul": 0.0}));
        let event = low.event.unwrap();
        assert_eq!(event["acted"], false);
        assert_eq!(event["answer"], json!({"noul": 0.9}));
        assert!(
            event["reason"]
                .as_str()
                .unwrap()
                .contains("the rule decides")
        );
        let down = scope_hook(&task).settle(Some(Err(Error::Unavailable("down".into()))));
        assert_eq!(down.answer, json!({"noul": 0.0}));
    }

    #[tokio::test]
    async fn triage_asks_type_and_priority() {
        let (url, _) = fake(
            200,
            r#"{"answers": {"decision": {"choice": "acme/web", "score": 2, "answer_confidence": 0.9}},
                "model": "laya-2"}"#,
        );
        let client = DecisionClient::new(&url, None).unwrap();
        let got = triage(Some(client), json!({"title": "500 on login"})).await;
        // `acme/web` is no type: the type question fails, its rule stands.
        assert_eq!(got.kind, None);
        assert_eq!(got.priority, 2);
        let acted: Vec<_> = got
            .settled
            .iter()
            .map(|s| s.event.as_ref().unwrap()["acted"] == true)
            .collect();
        assert_eq!(acted, [false, true]);
    }

    #[tokio::test]
    async fn decision_plugin_answers_through_a_mod() {
        let jev = mods(serde_json::from_str(BILLING).unwrap());
        let client = DecisionClient::plugin(jev.clone(), "jev");
        let decision = client.ask(json!({"title": "x"}), &choice()).await.unwrap();
        assert_eq!(decision.answer(), &Answer::Choice("billing".into()));
        let call = jev.calls.lock().unwrap()[0].clone();
        assert_eq!(
            (call.mod_name.as_str(), call.method.as_str()),
            ("jev", PLUGIN_METHOD)
        );
        assert_eq!(call.params["questions"]["decision"]["type"], "choice");
    }

    // ------------------------------------------------- export and replay

    async fn journal(store: &Store, run: RunId, step: &str, payload: Value) {
        let mut tx = store.begin().await.unwrap();
        let key = Some(StepKey::new(step).unwrap());
        tx.append_event(NewEvent::new(run, key, DECISION, payload))
            .await
            .unwrap();
        tx.commit().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn candidate_with_more_overrides_is_not_switched() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).await.unwrap();
        let (current_url, _) = fake(200, UNSURE);
        let current = DecisionClient::new(&current_url, None).unwrap();
        let engine = start(&store, dir.path(), Some(current)).await;
        let run = seed(&store, TRIAGE).await;
        let asked = |label: &str, confidence: f64| {
            json!({ "decides": "triage", "threshold": 0.8, "acted": false,
                    "answer": {"choice": label}, "confidence": confidence,
                    "model": "v1", "question": choice() })
        };
        let human =
            |label: &str| json!({ "acted": true, "answer": {"choice": label}, "by": "ana" });
        // `a`: v1 said tech, a human overrode it with billing.
        journal(&store, run, "a", asked("tech", 0.41)).await;
        journal(&store, run, "a", human("billing")).await;
        // `b`: v1 said billing, a human agreed.
        journal(&store, run, "b", asked("billing", 0.6)).await;
        journal(&store, run, "b", human("billing")).await;

        let jsonl = engine.export_decisions().await.unwrap();
        let rows: Vec<Value> = jsonl
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["answer"], json!({"choice": "billing"}));
        assert_eq!(rows[0]["model_answer"], json!({"choice": "tech"}));
        assert_eq!(rows[0]["overridden"], true);
        assert_eq!(rows[1]["overridden"], false);
        assert_eq!(rows[0]["state"]["title"], "t", "the task is the state");

        // The candidate never escalates, but says tech to both: one more
        // override than v1, so it stays out.
        let (candidate, _) = fake(
            200,
            r#"{"answers": {"decision": {"choice": "tech", "answer_confidence": 0.95}}}"#,
        );
        let replay = engine.replay_decisions(&candidate).await.unwrap();
        let tally = |escalated, overridden| Tally {
            asked: 2,
            escalated,
            overridden,
        };
        assert_eq!(replay.current, tally(2, 1));
        assert_eq!(replay.candidate, tally(0, 2));
        assert!(!replay.switched);
        assert_eq!(
            engine.decisions.get().unwrap().url(),
            Some(current_url.as_str())
        );
        let setting = store.read().await.unwrap().setting(SWITCHED_URL).await;
        assert_eq!(setting.unwrap(), None);

        // The rule itself: fewer escalations, and no more overrides.
        assert!(better(tally(2, 1), tally(1, 1)));
        assert!(!better(tally(2, 1), tally(2, 0)), "no fewer escalations");

        // A candidate that is right on both is switched in, here and on boot.
        let (right, _) = fake(200, BILLING);
        let replay = engine.replay_decisions(&right).await.unwrap();
        assert_eq!(replay.candidate, tally(0, 0));
        assert!(replay.switched);
        assert_eq!(engine.decisions.get().unwrap().url(), Some(right.as_str()));
        let mut read = store.read().await.unwrap();
        let configured = DecisionClient::new(&current_url, None).unwrap();
        let booted = switched(&mut read, Some(configured)).await.unwrap();
        assert_eq!(booted.unwrap().url(), Some(right.as_str()));
    }
}
