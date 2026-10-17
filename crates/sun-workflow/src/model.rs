//! The validated workflow: what the engine runs. Built only by [`crate::parse`],
//! so every value here already passed validation.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sun_core::model::Budget;
use sun_core::{ExecEnvSpec, McpGrant, StepKey};

/// Built-in code gates.
pub const CODE_GATES: &[&str] = &[
    "diff_not_empty",
    "test",
    "lint",
    "diagnostics_clean",
    "pr_exists",
    "ci_green",
    "data_touched",
    "integration",
    "merge_order",
    "visual_diff",
];

/// Built-in content gates for non-code work.
pub const CONTENT_GATES: &[&str] = &[
    "sections_present",
    "length",
    "format",
    "links_resolve",
    "terms",
    "image_size",
    "cites_sources",
];

/// Built-in step types. Any dotted name (`vendor.step`) is an extension step.
pub const STEP_TYPES: &[&str] = &[
    "agent", "gate", "approval", "wait", "notify", "action", "decide",
];

/// A parsed, validated workflow file.
///
/// The YAML `version:` field is not kept: Sun assigns versions on save
/// (`workflow_version.version`). Its identity is [`Workflow::hash`].
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Workflow {
    /// `name:`
    pub name: String,
    /// `description:` — what the workflow is for; with the task's text it
    /// names the repos a run works in. Left out of the hash when empty.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// `needs_repo:` — the run must resolve at least one repo even without
    /// a git action or gate. Left out of the hash when false.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub needs_repo: bool,
    /// Triggers (`on:`), e.g. `manual`, `cron:0 9 * * 1`, `jira.issue_labeled:agent`.
    /// Defaults to `[manual]`.
    pub on: Vec<String>,
    /// Failover policy; `None` uses the ecosystem default.
    pub runtime: Option<RuntimePolicy>,
    /// Run limits; all `None` when absent.
    pub budget: Budget,
    /// Stall settings (`watchdog:`); unset fields use `sun.yaml`'s.
    /// Left out of the hash when absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub watchdog: Option<WatchdogPolicy>,
    /// Where agent sessions (and their gates) run (`exec_env:`), in
    /// the `ExecEnvSpec` wire form; absent = bare. A task may override it.
    /// Left out of the hash when bare, so older workflows keep theirs.
    #[serde(skip_serializing_if = "ExecEnvSpec::is_bare")]
    pub exec_env: ExecEnvSpec,
    /// MCP servers every agent step gets (`mcp:`); a step's own
    /// `mcp:` entry for the same server wins. Left out of the hash when empty.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub mcp: Vec<McpGrant>,
    /// Steps in file order.
    pub steps: Vec<Step>,
    /// SHA-256 hex of the normalized workflow (this struct as JSON, without
    /// `hash`). Comments, key order, quoting, the YAML `version:` and implicit
    /// vs explicit `needs:` don't change it; any semantic change does.
    #[serde(skip)]
    pub hash: String,
}

impl Workflow {
    /// Whether a run needs a repo: it opens a PR (an `*.open_pr` action),
    /// gates on one (`pr_exists`, `ci_green`, `merge_order`) or says so
    /// (`needs_repo: true`).
    pub fn is_coding(&self) -> bool {
        self.needs_repo
            || self.steps.iter().any(|step| match &step.kind {
                StepKind::Action { target, .. } => target.ends_with(".open_pr"),
                StepKind::Gate { gates, .. } => gates
                    .iter()
                    .any(|g| matches!(g.name.as_str(), "pr_exists" | "ci_green" | "merge_order")),
                _ => false,
            })
    }

    /// Names of the mods this workflow needs, sorted: the plugin of each
    /// dotted trigger, action, extension step and extension gate, and the
    /// scheme of each notify target (`slack:#eng` → `slack`). The catalog lists
    /// these before a template installs.
    pub fn mods(&self) -> Vec<String> {
        let plugin = |name: &str| name.split_once('.').map(|(p, _)| p.to_owned());
        let mut mods = std::collections::BTreeSet::new();
        mods.extend(
            self.on
                .iter()
                .filter(|t| !t.starts_with("cron:"))
                .filter_map(|t| plugin(t)),
        );
        for step in &self.steps {
            match &step.kind {
                StepKind::Action { target: name, .. } | StepKind::Extension { name, .. } => {
                    mods.extend(plugin(name))
                }
                StepKind::Gate { gates, .. } => {
                    mods.extend(gates.iter().filter_map(|g| plugin(&g.name)))
                }
                StepKind::Notify { to, .. } => mods.extend(
                    to.iter()
                        .filter_map(|t| t.split_once(':').map(|(s, _)| s.to_owned())),
                ),
                StepKind::Agent { .. }
                | StepKind::Approval { .. }
                | StepKind::Wait { .. }
                | StepKind::Decide { .. } => {}
            }
        }
        mods.into_iter().collect()
    }
}

/// `runtime:` — the failover chain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RuntimePolicy {
    /// Runtimes in failover order: names (`sun_core::model::Runtime::name`)
    /// or `{tool, credential, host, model, effort}`; non-empty, each
    /// [`ChainEntry::name`](sun_core::ChainEntry::name) unique.
    pub chain: Vec<sun_core::ChainEntry>,
    /// What to do when the current runtime hits a limit; `None`: the
    /// ecosystem default (a task's own setting beats both).
    pub on_limit: Option<OnLimit>,
    /// Who must approve an agent step that failed over to another runtime
    /// before it counts as done; empty: no review.
    pub review_after_failover: Vec<String>,
    /// Go back to `chain[0]` mid-step, at the next tool result, once its
    /// cooldown is over, with a hand-off pack.
    pub switch_back: bool,
}

/// `runtime.on_limit`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnLimit {
    /// Hand off to the next runtime in the chain (default).
    Switch,
    /// Pause the run and notify.
    PauseAndNotify,
    /// Wait until the limit resets, then continue on the same runtime.
    WaitUntilReset,
}

/// `watchdog:` — how this workflow's agent steps are watched for stalls.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct WatchdogPolicy {
    /// No agent activity for this many seconds marks a step `stalled`
    /// (`stall_after:`, a duration like `15m`).
    pub stall_after_secs: Option<u64>,
    /// What happens then.
    pub on_stall: Option<StallPolicy>,
}

/// What the watchdog does with a stalled step. Each policy acts once
/// per step attempt; the next stall fails the step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StallPolicy {
    /// Steer the agent with a short message and keep it running.
    #[default]
    Nudge,
    /// Stop the session and start a new one with a hand-off note.
    Restart,
    /// Leave the step `stalled` and end the run `stalled` with the reason.
    Fail,
}

/// Send-backs a gate step gets when `on_fail.max` is left out.
pub const DEFAULT_SEND_BACKS: u32 = 2;

/// One step of the workflow.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Step {
    /// Unique step id.
    pub id: StepKey,
    /// Steps that must succeed first. Resolved: a step without `needs:` needs
    /// the previous step (none for the first); `needs: []` starts at once.
    pub needs: Vec<StepKey>,
    /// Retry policy for failed attempts (`retry:`); `None` = a
    /// failed attempt fails the step. Left out of the hash when absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry: Option<RetryPolicy>,
    /// What the step does.
    #[serde(flatten)]
    pub kind: StepKind,
}

/// `retry:` of a step: a failed attempt starts again until `max` attempts
/// ran, waiting `backoff_secs · 2^(n-1)` after the n-th.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct RetryPolicy {
    /// Most attempts in total, the first included (≥ 1).
    pub max: u32,
    /// Wait before the first retry (`backoff:`, default 0s).
    pub backoff_secs: u64,
}

impl RetryPolicy {
    /// The wait before attempt `failed + 1`, or `None` once `failed`
    /// attempts used up the policy. Doubles per attempt, at most an hour.
    pub fn delay_after(&self, failed: u32) -> Option<u64> {
        (failed < self.max).then(|| {
            let doubled = self
                .backoff_secs
                .saturating_mul(1 << failed.saturating_sub(1).min(16));
            doubled.min(3600)
        })
    }
}

/// What a step does, by its `type:`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StepKind {
    /// Run an agent session.
    Agent {
        /// Prompt: inline text, or a file under the repo's `.sun/` (e.g.
        /// `prompts/implement.md`, see [`crate::is_prompt_file`]) that the
        /// engine inlines at run start.
        prompt: String,
        /// Skill names installed into the session.
        skills: Vec<String>,
        /// MCP servers this step's sessions get on top of the workflow's
        /// (`mcp:`); left out of the hash when empty.
        #[serde(skip_serializing_if = "Vec::is_empty")]
        mcp: Vec<McpGrant>,
        /// Context sources, e.g. `repo:docs/`.
        context_sources: Vec<String>,
        /// Token budget for the pushed context pack.
        context_budget_tokens: Option<u32>,
    },
    /// Verify the previous work.
    Gate {
        /// Gates to run, in order; non-empty.
        gates: Vec<GateSpec>,
        /// Send-back policy on failure; `None` fails the step.
        on_fail: Option<OnFail>,
    },
    /// Block until an authorized human decides.
    Approval {
        /// Who may decide: `role:<name>` or `user:<name>`; non-empty.
        who: Vec<String>,
        /// Timeout policy; `None` waits forever.
        timeout: Option<ApprovalTimeout>,
        /// Only ask when this holds; otherwise the step succeeds at once.
        #[serde(skip_serializing_if = "Option::is_none")]
        when: Option<ApprovalWhen>,
    },
    /// Sleep.
    Wait {
        /// Seconds to wait (`for:`).
        for_secs: u64,
    },
    /// Send a message.
    Notify {
        /// Targets, e.g. `slack:#eng`; non-empty.
        to: Vec<String>,
        /// Message text.
        message: Option<String>,
    },
    /// Call a plugin action through the outbox (`use:`).
    Action {
        /// Dotted action name, e.g. `github.open_pr`.
        target: String,
        /// Action input (`with:`).
        with: Option<Value>,
    },
    /// Ask the decision engine a typed question. At or above
    /// `threshold` Sun acts on the answer; below it, or with no decision
    /// backend, a human in `who` decides as on an approval step.
    Decide {
        /// What the answer may be used for (never pass or done).
        decides: DecisionUse,
        /// The typed question.
        question: Question,
        /// Calibrated confidence (0..=1) at or above which Sun acts.
        threshold: f64,
        /// Who decides on escalation: `role:<name>` or `user:<name>`; non-empty.
        who: Vec<String>,
    },
    /// An extension step, typed by its dotted name (e.g. `browser.screenshot`).
    Extension {
        /// The `type:` value.
        name: String,
        /// Step input (`with:`).
        with: Option<Value>,
    },
}

/// One entry of `gates:` — a name, or `{name: params}`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GateSpec {
    /// A built-in gate name or a dotted extension gate.
    pub name: String,
    /// Gate parameters; their shape belongs to the gate.
    pub params: Option<Value>,
}

/// `on_fail:` of a gate step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OnFail {
    /// Step to send the work back to.
    pub send_back_to: StepKey,
    /// Maximum send-backs before the step fails.
    pub max: u32,
}

/// `timeout:` of an approval step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ApprovalTimeout {
    /// Seconds before the timeout fires (`after:`).
    pub after_secs: u64,
    /// What happens then (`then:`).
    pub then: TimeoutAction,
}

/// `when:` of an approval step: the condition under which a human must decide.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalWhen {
    /// A `data_touched` gate of this run detected data changes.
    DataTouched,
}

/// `timeout.then` of an approval step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimeoutAction {
    /// Fail the step.
    Fail,
    /// Remind the approvers and keep waiting.
    Remind,
    /// Reject automatically.
    AutoReject,
}

/// `decides:` of a decide step: the only things a decision may do.
/// There is deliberately no variant that passes a gate, marks work done or
/// approves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionUse {
    /// Pick a path, runtime or owner.
    Route,
    /// Classify incoming work (type, priority, repos).
    Triage,
    /// Raise a flag for human review.
    Flag,
    /// Ask for an extra check.
    AddCheck,
}

/// `question:` of a decide step, in the `/v1/systemone` wire shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Question {
    /// `type:` choice, score or noul.
    #[serde(rename = "type")]
    pub kind: QuestionKind,
    /// Guidance for the model.
    pub instructions: String,
    /// Labels → descriptions (choice), ordered levels (score) or the
    /// statement (noul).
    pub criteria: Value,
}

/// The typed question kinds of `/v1/systemone`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuestionKind {
    /// Pick one label.
    Choice,
    /// Pick an ordinal level.
    Score,
    /// Yes or no.
    Noul,
}
