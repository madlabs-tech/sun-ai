//! YAML → [`Workflow`]: span-aware deserialization (serde-saphyr `Spanned`)
//! followed by semantic validation that reports every error with line:col.

use std::collections::{BTreeMap, HashMap};

use serde::Deserialize;
use serde_json::Value;
use serde_saphyr::{Location, Spanned};
use sun_core::model::Budget;
use sun_core::{ChainEntry, ChainSpec, ExecEnvSpec, McpGrant, PermissionMode, StepKey};

use crate::model::{
    ApprovalTimeout, ApprovalWhen, CODE_GATES, CONTENT_GATES, DEFAULT_SEND_BACKS, DecisionUse,
    GateSpec, OnFail, OnLimit, Question, QuestionKind, RetryPolicy, RuntimePolicy, STEP_TYPES,
    StallPolicy, Step, StepKind, TimeoutAction, WatchdogPolicy, Workflow,
};

/// One problem in a workflow file, with its 1-based position (0:0 if unknown).
/// Displays as `line:column: message`; callers prefix the file name.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{line}:{column}: {message}")]
pub struct WorkflowError {
    /// 1-based line.
    pub line: u64,
    /// 1-based column.
    pub column: u64,
    /// What is wrong, and how to fix it when that isn't obvious.
    pub message: String,
}

/// Parses and validates a workflow file. Returns every semantic error found;
/// a YAML syntax or shape error stops at the first one.
pub fn parse(source: &str) -> Result<Workflow, Vec<WorkflowError>> {
    let raw = raw(source)?;
    let mut check = Check::default();
    let workflow = check.workflow(raw);
    let normalized = workflow.as_ref().map(serde_json::to_vec);
    match (workflow, normalized) {
        (Some(mut workflow), Some(Ok(json))) if check.errors.is_empty() => {
            workflow.hash = crate::sha256_hex(&json);
            Ok(workflow)
        }
        _ if !check.errors.is_empty() => Err(check.errors),
        _ => Err(vec![WorkflowError {
            line: 0,
            column: 0,
            message: "internal: workflow could not be normalized".to_owned(),
        }]),
    }
}

fn raw(source: &str) -> Result<RawWorkflow, Vec<WorkflowError>> {
    serde_saphyr::from_str(source).map_err(|e| {
        let (line, column) = e.location().map_or((0, 0), |l| (l.line(), l.column()));
        // The location is in our own fields; drop serde-saphyr's copy and its API advice.
        let message = e.without_snippet().to_string();
        let message = message
            .strip_suffix(&format!(" at line {line}, column {column}"))
            .unwrap_or(&message)
            .replace(", set DuplicateKeyPolicy in Options if acceptable", "");
        vec![WorkflowError {
            line,
            column,
            message,
        }]
    })
}

/// Whether an agent step's `prompt:` names a file instead of holding the
/// text: one word ending in `.md` or `.txt`, a relative path of
/// `[A-Za-z0-9._-]` parts without `..`. The engine reads it under the
/// repo's `.sun/` (e.g. `prompts/implement.md` → `.sun/prompts/implement.md`).
pub fn is_prompt_file(prompt: &str) -> bool {
    let safe = |part: &str| {
        !part.is_empty()
            && part != "."
            && part != ".."
            && part
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    };
    (prompt.ends_with(".md") || prompt.ends_with(".txt")) && prompt.split('/').all(safe)
}

/// `source` with the `prompt:` of each step in `texts` (by step id)
/// replaced by that text, written as a one-line double-quoted scalar so the
/// rest of the file keeps its lines. Fails when the result doesn't parse
/// back to exactly those prompts (e.g. a prompt inside a flow mapping).
pub fn inline_prompts(
    source: &str,
    texts: &BTreeMap<String, String>,
) -> Result<String, Vec<WorkflowError>> {
    let raw = raw(source)?;
    let mut lines: Vec<String> = source.split('\n').map(str::to_owned).collect();
    let mut failed = Vec::new();
    let error = |line, column| WorkflowError {
        line,
        column,
        message: "can't inline this prompt file: write `prompt: <path>` on its own line".into(),
    };
    for step in &raw.steps.value {
        let (Some(text), Some(prompt)) = (texts.get(&step.id.value), &step.prompt) else {
            continue;
        };
        let at = prompt.referenced;
        let replaced = usize::try_from(at.line())
            .ok()
            .and_then(|n| lines.get_mut(n.checked_sub(1)?))
            .and_then(|line| {
                let column = usize::try_from(at.column()).ok()?.checked_sub(1)?;
                let start = line.char_indices().nth(column).map(|(i, _)| i)?;
                let value = &prompt.value;
                let len = [format!("\"{value}\""), format!("'{value}'"), value.clone()]
                    .into_iter()
                    .find(|v| line[start..].starts_with(v.as_str()))?
                    .len();
                let quoted = serde_json::to_string(text).ok()?;
                line.replace_range(start..start + len, &quoted);
                Some(())
            });
        if replaced.is_none() {
            failed.push(error(at.line(), at.column()));
        }
    }
    let out = lines.join("\n");
    let parsed = parse(&out)?;
    let wrong = parsed
        .steps
        .iter()
        .any(|s| match (&s.kind, texts.get(s.id.as_str())) {
            (StepKind::Agent { prompt, .. }, Some(text)) => prompt != text,
            _ => false,
        });
    if wrong && failed.is_empty() {
        failed.push(error(0, 0));
    }
    if failed.is_empty() {
        Ok(out)
    } else {
        Err(failed)
    }
}

type S<T> = Spanned<T>;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawWorkflow {
    name: S<String>,
    #[allow(dead_code)] // set by Sun on save; accepted and ignored
    version: Option<u32>,
    description: Option<String>,
    needs_repo: Option<bool>,
    on: Option<Vec<S<String>>>,
    runtime: Option<RawRuntime>,
    budget: Option<RawBudget>,
    watchdog: Option<RawWatchdog>,
    exec_env: Option<S<ExecEnvSpec>>,
    mcp: Option<RawGrants>,
    steps: S<Vec<RawStep>>,
}

/// `mcp:` — MCP server grants, in the [`McpGrant`] shape.
type RawGrants = S<Vec<S<McpGrant>>>;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRuntime {
    chain: S<Vec<S<ChainEntry>>>,
    on_limit: Option<OnLimit>,
    review_after_failover: Option<Vec<S<String>>>,
    switch_back: Option<bool>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawBudget {
    wall: Option<S<String>>,
    turns: Option<S<u32>>,
    cost_usd: Option<S<f64>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawWatchdog {
    stall_after: Option<S<String>>,
    on_stall: Option<StallPolicy>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawStep {
    id: S<String>,
    #[serde(rename = "type")]
    kind: S<String>,
    needs: Option<S<Vec<S<String>>>>,
    prompt: Option<S<String>>,
    skills: Option<S<Vec<S<String>>>>,
    mcp: Option<RawGrants>,
    context: Option<S<RawContext>>,
    gates: Option<S<Vec<S<RawGate>>>>,
    on_fail: Option<S<RawOnFail>>,
    who: Option<S<Vec<S<String>>>>,
    timeout: Option<S<RawTimeout>>,
    when: Option<S<ApprovalWhen>>,
    #[serde(rename = "for")]
    wait_for: Option<S<String>>,
    to: Option<S<Vec<S<String>>>>,
    message: Option<S<String>>,
    #[serde(rename = "use")]
    target: Option<S<String>>,
    with: Option<S<Value>>,
    decides: Option<S<DecisionUse>>,
    question: Option<S<Question>>,
    threshold: Option<S<f64>>,
    retry: Option<S<RawRetry>>,
    permission_mode: Option<S<PermissionMode>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRetry {
    max: u32,
    backoff: Option<S<String>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawContext {
    #[serde(default)]
    sources: Vec<S<String>>,
    budget_tokens: Option<u32>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum RawGate {
    Name(String),
    WithParams(BTreeMap<String, Value>),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawOnFail {
    send_back_to: S<String>,
    max: Option<u32>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTimeout {
    after: S<String>,
    then: TimeoutAction,
}

/// Collects errors while building the validated [`Workflow`].
#[derive(Default)]
struct Check {
    errors: Vec<WorkflowError>,
}

impl Check {
    fn err(&mut self, at: Location, message: impl Into<String>) {
        self.errors.push(WorkflowError {
            line: at.line(),
            column: at.column(),
            message: message.into(),
        });
    }

    fn workflow(&mut self, raw: RawWorkflow) -> Option<Workflow> {
        if !is_slug(&raw.name.value) {
            self.err(
                raw.name.referenced,
                format!("invalid workflow name `{}`: {SLUG_RULE}", raw.name.value),
            );
        }
        let on = match raw.on {
            None => vec!["manual".to_owned()],
            Some(triggers) => triggers.into_iter().map(|t| self.trigger(t)).collect(),
        };
        let runtime = raw.runtime.map(|r| self.runtime(r));
        let budget = raw.budget.map(|b| self.budget(b)).unwrap_or_default();
        let watchdog = raw.watchdog.map(|w| WatchdogPolicy {
            stall_after_secs: w.stall_after.and_then(|d| self.duration(&d)),
            on_stall: w.on_stall,
        });
        let steps = self.steps(raw.steps);
        let exec_env = match raw.exec_env {
            Some(env) => {
                if let Err(e) = env.value.validate() {
                    self.err(env.referenced, format!("exec_env: {e}"));
                }
                env.value
            }
            None => ExecEnvSpec::Bare,
        };
        let mcp = self.grants(raw.mcp.as_ref());
        Some(Workflow {
            name: raw.name.value,
            description: raw.description.unwrap_or_default().trim().to_owned(),
            needs_repo: raw.needs_repo.unwrap_or_default(),
            on,
            runtime,
            budget,
            watchdog,
            exec_env,
            mcp,
            steps: steps?,
            hash: String::new(),
        })
    }

    fn trigger(&mut self, t: S<String>) -> String {
        let text = t.value.as_str();
        let valid = match text.split_once(':') {
            _ if text == "manual" || text == "webhook" => true,
            Some(("cron", expr)) => match crate::Cron::parse(expr) {
                Ok(_) => true,
                Err(e) => {
                    self.err(t.referenced, e);
                    return t.value;
                }
            },
            Some((event, filter)) => is_dotted(event) && !filter.is_empty(),
            None => is_dotted(text),
        };
        if !valid {
            self.err(
                t.referenced,
                format!(
                    "unknown trigger `{text}`: use manual, webhook, cron:<expr> or a plugin event like jira.issue_labeled:agent"
                ),
            );
        }
        t.value
    }

    fn runtime(&mut self, raw: RawRuntime) -> RuntimePolicy {
        if raw.chain.value.is_empty() {
            self.err(
                raw.chain.referenced,
                "`runtime.chain` needs at least one runtime",
            );
        }
        let mut chain: Vec<ChainEntry> = Vec::new();
        for entry in raw.chain.value {
            let at = entry.referenced;
            match &entry.value {
                ChainEntry::Name(name) => {
                    // `market:<adapter>`: that runtime rented on the marketplace;
                    // `<mod>.<adapter>`: one an extension serves (mod API 1.2).
                    let runtime = name.strip_prefix("market:").unwrap_or(name);
                    if !is_runtime(runtime) {
                        self.err(
                            at,
                            format!(
                                "invalid runtime name `{name}`: {SLUG_RULE}, or <extension>.<adapter>"
                            ),
                        );
                    }
                }
                ChainEntry::Spec(spec) => self.chain_spec(at, spec),
            }
            let name = entry.value.name();
            if chain.iter().any(|e| e.name() == name) {
                self.err(at, format!("runtime `{name}` appears twice in the chain"));
            }
            chain.push(entry.value);
        }
        let review_after_failover = raw
            .review_after_failover
            .unwrap_or_default()
            .iter()
            .filter_map(|w| self.principal(w))
            .collect();
        RuntimePolicy {
            chain,
            on_limit: raw.on_limit,
            review_after_failover,
            switch_back: raw.switch_back.unwrap_or(false),
        }
    }

    /// Checks the object form of a chain entry. `model` and `effort` end up
    /// as CLI arguments, so each must be one plain token. Whether the
    /// credential, host and effort exist is checked where the workspace is
    /// known (the engine, on save).
    fn chain_spec(&mut self, at: Location, spec: &ChainSpec) {
        if !is_runtime(&spec.tool) {
            self.err(
                at,
                format!(
                    "invalid `tool` `{}`: {SLUG_RULE}, or <extension>.<adapter>",
                    spec.tool
                ),
            );
        }
        if let Some(name) = &spec.credential
            && !is_runtime_name(name)
        {
            self.err(
                at,
                format!("invalid `credential` `{name}`: 1–64 of a-z, 0-9, - or _"),
            );
        }
        let fields = [
            ("host", &spec.host, 64),
            ("model", &spec.model, 128),
            ("effort", &spec.effort, 32),
        ];
        for (field, value, max) in fields {
            if let Some(value) = value
                && !is_cli_token(value, max)
            {
                self.err(
                    at,
                    format!(
                        "invalid `{field}` `{value}`: one word of at most {max} characters, not starting with `-`"
                    ),
                );
            }
        }
    }

    fn budget(&mut self, raw: RawBudget) -> Budget {
        if let Some(turns) = &raw.turns
            && turns.value == 0
        {
            self.err(turns.referenced, "`budget.turns` must be at least 1");
        }
        if let Some(cost) = &raw.cost_usd
            && !(cost.value.is_finite() && cost.value > 0.0)
        {
            self.err(
                cost.referenced,
                "`budget.cost_usd` must be a positive number",
            );
        }
        Budget {
            wall_secs: raw.wall.and_then(|w| self.duration(&w)),
            turns: raw.turns.map(|t| t.value),
            cost_usd: raw.cost_usd.map(|c| c.value),
        }
    }

    fn duration(&mut self, d: &S<String>) -> Option<u64> {
        let secs = parse_duration(&d.value);
        if secs.is_none() {
            self.err(
                d.referenced,
                format!("invalid duration `{}`: use a number and unit (s, m, h, d), e.g. 90s, 2h, 1h30m", d.value),
            );
        }
        secs
    }

    /// `retry: {max, backoff}`; `Some(None)` without one, `None` on error.
    fn retry(&mut self, raw: Option<&S<RawRetry>>) -> Option<Option<RetryPolicy>> {
        let Some(raw) = raw else { return Some(None) };
        if raw.value.max == 0 {
            self.err(raw.referenced, "`retry.max` counts attempts: use 1 or more");
            return None;
        }
        let backoff_secs = match &raw.value.backoff {
            Some(d) => self.duration(d)?,
            None => 0,
        };
        Some(Some(RetryPolicy {
            max: raw.value.max,
            backoff_secs,
        }))
    }

    fn steps(&mut self, raw: S<Vec<RawStep>>) -> Option<Vec<Step>> {
        if raw.value.is_empty() {
            self.err(raw.referenced, "a workflow needs at least one step");
        }
        // Pass 1: ids, so references can point forward.
        let mut index: HashMap<&str, usize> = HashMap::new();
        let mut keys = Vec::with_capacity(raw.value.len());
        for (i, step) in raw.value.iter().enumerate() {
            let id = &step.id;
            keys.push(StepKey::new(id.value.as_str()).ok());
            if keys[i].is_none() {
                self.err(
                    id.referenced,
                    format!("invalid step id `{}`: {SLUG_RULE}", id.value),
                );
            }
            if let Some(&first) = index.get(id.value.as_str()) {
                let at = raw.value[first].id.referenced;
                self.err(
                    id.referenced,
                    format!(
                        "duplicate step id `{}` (first at {}:{})",
                        id.value,
                        at.line(),
                        at.column()
                    ),
                );
            } else {
                index.insert(id.value.as_str(), i);
            }
        }

        // Pass 2: needs (as indices) and kinds.
        let mut deps: Vec<Vec<usize>> = Vec::with_capacity(raw.value.len());
        let mut kinds = Vec::with_capacity(raw.value.len());
        for (i, step) in raw.value.iter().enumerate() {
            deps.push(match &step.needs {
                None => i.checked_sub(1).into_iter().collect(),
                Some(needs) => needs
                    .value
                    .iter()
                    .filter_map(|n| {
                        let found = index.get(n.value.as_str()).copied();
                        if found.is_none() {
                            self.err(
                                n.referenced,
                                format!("`needs` names unknown step `{}`", n.value),
                            );
                        }
                        found
                    })
                    .collect(),
            });
            kinds.push(self.kind(step, i, &index));
        }
        self.cycles(&raw.value, &deps);
        self.device_after_pr(&raw.value, &deps);

        let mut steps = Vec::with_capacity(raw.value.len());
        for ((key, kind), needs) in keys.iter().zip(kinds).zip(&deps) {
            let needs = needs
                .iter()
                .map(|&j| keys[j].clone())
                .collect::<Option<_>>()?;
            steps.push(Step {
                id: key.clone()?,
                needs,
                retry: self.retry(raw.value[steps.len()].retry.as_ref())?,
                kind: kind?,
            });
        }
        Some(steps)
    }

    /// Kahn's algorithm; every step left over sits on or behind a cycle.
    fn cycles(&mut self, raw: &[RawStep], deps: &[Vec<usize>]) {
        let mut pending: Vec<usize> = deps.iter().map(Vec::len).collect();
        let mut ready: Vec<usize> = (0..deps.len()).filter(|&i| pending[i] == 0).collect();
        let mut done = vec![false; deps.len()];
        while let Some(i) = ready.pop() {
            done[i] = true;
            for (j, d) in deps.iter().enumerate() {
                for _ in d.iter().filter(|&&k| k == i) {
                    pending[j] -= 1;
                    if pending[j] == 0 {
                        ready.push(j);
                    }
                }
            }
        }
        let stuck: Vec<usize> = (0..deps.len()).filter(|&i| !done[i]).collect();
        if let Some(&first) = stuck.first() {
            let ids: Vec<&str> = stuck.iter().map(|&i| raw[i].id.value.as_str()).collect();
            let at = raw[first]
                .needs
                .as_ref()
                .map_or(raw[first].id.referenced, |n| n.referenced);
            self.err(
                at,
                format!(
                    "`needs` forms a cycle through steps {}; a step without `needs` runs after the previous step",
                    ids.join(", ")
                ),
            );
        }
    }

    /// A device verify attaches its screenshots to the task's PR, so
    /// an `<mod>.open_pr` action must run before it (directly or through
    /// other steps in `needs`); otherwise the attachment fails for good.
    fn device_after_pr(&mut self, raw: &[RawStep], deps: &[Vec<usize>]) {
        let opens_pr = |s: &RawStep| {
            s.kind.value == "action"
                && s.target
                    .as_ref()
                    .is_some_and(|t| t.value.ends_with(".open_pr"))
        };
        for (i, step) in raw.iter().enumerate() {
            if step.kind.value != DEVICE_VERIFY {
                continue;
            }
            // Walk the ancestors; `seen` also stops a cycle (reported above).
            let mut seen = vec![false; raw.len()];
            let mut todo = deps[i].clone();
            let mut found = false;
            while let Some(j) = todo.pop() {
                if std::mem::replace(&mut seen[j], true) {
                    continue;
                }
                found |= opens_pr(&raw[j]);
                todo.extend(&deps[j]);
            }
            if !found {
                self.err(
                    step.kind.referenced,
                    format!(
                        "`{DEVICE_VERIFY}` attaches screenshots to the PR: put a step like `github.open_pr` before `{}` (or in its `needs`)",
                        step.id.value
                    ),
                );
            }
        }
    }

    fn kind(&mut self, s: &RawStep, i: usize, index: &HashMap<&str, usize>) -> Option<StepKind> {
        let ty = s.kind.value.as_str();
        let allowed: &[&str] = match ty {
            "agent" => &["prompt", "skills", "mcp", "context", "permission_mode"],
            "gate" => &["gates", "on_fail"],
            "approval" => &["who", "timeout", "when"],
            "wait" => &["for"],
            "notify" => &["to", "message"],
            "action" => &["use", "with"],
            "decide" => &["decides", "question", "threshold", "who"],
            _ if is_dotted(ty) => &["with"],
            _ => {
                self.err(
                    s.kind.referenced,
                    format!(
                        "unknown step type `{ty}`: use {} or a dotted extension step like `browser.screenshot`",
                        STEP_TYPES.join(", ")
                    ),
                );
                return None;
            }
        };
        let present = [
            ("prompt", at(&s.prompt)),
            ("skills", at(&s.skills)),
            ("mcp", at(&s.mcp)),
            ("context", at(&s.context)),
            ("gates", at(&s.gates)),
            ("on_fail", at(&s.on_fail)),
            ("who", at(&s.who)),
            ("timeout", at(&s.timeout)),
            ("for", at(&s.wait_for)),
            ("to", at(&s.to)),
            ("message", at(&s.message)),
            ("use", at(&s.target)),
            ("with", at(&s.with)),
            ("decides", at(&s.decides)),
            ("question", at(&s.question)),
            ("threshold", at(&s.threshold)),
            ("permission_mode", at(&s.permission_mode)),
        ];
        for (field, location) in present {
            if let Some(location) = location
                && !allowed.contains(&field)
            {
                self.err(
                    location,
                    format!(
                        "`{field}` is not allowed on `{ty}` steps (allowed: {})",
                        allowed.join(", ")
                    ),
                );
            }
        }
        let id_at = s.id.referenced;
        let with = s.with.as_ref().map(|w| w.value.clone());

        match ty {
            "agent" => {
                // `prompt: ""` is allowed: the session gets its task and
                // context only (an agent with no instructions).
                let Some(prompt) = s.prompt.as_ref().map(|p| p.value.clone()) else {
                    self.err(id_at, "`agent` steps need a `prompt`");
                    return None;
                };
                let (context_sources, context_budget_tokens) = match &s.context {
                    None => (Vec::new(), None),
                    Some(c) => (
                        self.strings(&c.value.sources, "context source"),
                        c.value.budget_tokens,
                    ),
                };
                Some(StepKind::Agent {
                    prompt,
                    skills: s
                        .skills
                        .as_ref()
                        .map_or_else(Vec::new, |k| self.strings(&k.value, "skill")),
                    mcp: self.grants(s.mcp.as_ref()),
                    context_sources,
                    context_budget_tokens,
                    permission_mode: s.permission_mode.as_ref().map(|m| m.value),
                })
            }
            "gate" => {
                let Some(raw_gates) = s.gates.as_ref().filter(|g| !g.value.is_empty()) else {
                    self.err(id_at, "`gate` steps need a non-empty `gates` list");
                    return None;
                };
                let gates = raw_gates
                    .value
                    .iter()
                    .map(|g| self.gate(g))
                    .collect::<Option<Vec<_>>>();
                let on_fail = match &s.on_fail {
                    None => None,
                    Some(f) => Some(self.on_fail(&f.value, i, index)?),
                };
                Some(StepKind::Gate {
                    gates: gates?,
                    on_fail,
                })
            }
            "approval" => {
                let who = self.who(s, id_at, ty);
                let timeout = match &s.timeout {
                    None => None,
                    Some(t) => Some(ApprovalTimeout {
                        after_secs: self.duration(&t.value.after)?,
                        then: t.value.then,
                    }),
                };
                Some(StepKind::Approval {
                    who: who?,
                    timeout,
                    when: s.when.as_ref().map(|w| w.value),
                })
            }
            "wait" => match &s.wait_for {
                None => {
                    self.err(id_at, "`wait` steps need `for`, e.g. 30m");
                    None
                }
                Some(d) => Some(StepKind::Wait {
                    for_secs: self.duration(d)?,
                }),
            },
            "notify" => {
                let Some(to) = s.to.as_ref().filter(|t| !t.value.is_empty()) else {
                    self.err(id_at, "`notify` steps need a non-empty `to` list");
                    return None;
                };
                Some(StepKind::Notify {
                    to: self.strings(&to.value, "notify target"),
                    message: s.message.as_ref().map(|m| m.value.clone()),
                })
            }
            "action" => {
                let target = self.non_empty(
                    s.target.as_ref(),
                    id_at,
                    "`action` steps need `use`, e.g. github.open_pr",
                )?;
                if !is_dotted(&target) {
                    self.err(
                        at(&s.target).unwrap_or(id_at),
                        format!(
                            "invalid action `{target}`: use plugin.action, e.g. github.open_pr"
                        ),
                    );
                }
                Some(StepKind::Action { target, with })
            }
            "decide" => {
                let who = self.who(s, id_at, ty);
                let Some(decides) = &s.decides else {
                    self.err(
                        id_at,
                        "`decide` steps need `decides`: route, triage, flag or add_check",
                    );
                    return None;
                };
                let question = self.question(s.question.as_ref(), id_at);
                let threshold = match &s.threshold {
                    Some(t) if (0.0..=1.0).contains(&t.value) => Some(t.value),
                    Some(t) => {
                        self.err(t.referenced, "`threshold` must be between 0 and 1");
                        None
                    }
                    None => {
                        self.err(id_at, "`decide` steps need a `threshold`, e.g. 0.8");
                        None
                    }
                };
                Some(StepKind::Decide {
                    decides: decides.value,
                    question: question?,
                    threshold: threshold?,
                    who: who?,
                })
            }
            _ => Some(StepKind::Extension {
                name: ty.to_owned(),
                with,
            }),
        }
    }

    fn gate(&mut self, g: &S<RawGate>) -> Option<GateSpec> {
        let (name, params) = match &g.value {
            RawGate::Name(name) => (name.clone(), None),
            RawGate::WithParams(map) if map.len() == 1 => {
                let (name, params) = map.iter().next()?;
                (name.clone(), Some(params.clone()))
            }
            RawGate::WithParams(_) => {
                self.err(
                    g.referenced,
                    "a gate is a name or `{name: params}` with exactly one key",
                );
                return None;
            }
        };
        let known = CODE_GATES.contains(&name.as_str())
            || CONTENT_GATES.contains(&name.as_str())
            || is_dotted(&name);
        if !known {
            self.err(
                g.referenced,
                format!(
                    "unknown gate `{name}`: use {}, {} or a dotted extension gate",
                    CODE_GATES.join(", "),
                    CONTENT_GATES.join(", ")
                ),
            );
            return None;
        }
        Some(GateSpec { name, params })
    }

    fn on_fail(&mut self, f: &RawOnFail, i: usize, index: &HashMap<&str, usize>) -> Option<OnFail> {
        let target = &f.send_back_to;
        match index.get(target.value.as_str()) {
            Some(&j) if j != i => Some(OnFail {
                send_back_to: StepKey::new(target.value.as_str()).ok()?,
                max: f.max.unwrap_or(DEFAULT_SEND_BACKS),
            }),
            Some(_) => {
                self.err(target.referenced, "`send_back_to` must name another step");
                None
            }
            None => {
                self.err(
                    target.referenced,
                    format!("`send_back_to` names unknown step `{}`", target.value),
                );
                None
            }
        }
    }

    /// `who:` of an approval or decide step: non-empty, each a principal.
    fn who(&mut self, s: &RawStep, id_at: Location, ty: &str) -> Option<Vec<String>> {
        match s.who.as_ref().filter(|w| !w.value.is_empty()) {
            None => {
                self.err(id_at, format!("`{ty}` steps need `who`, e.g. [role:admin]"));
                None
            }
            Some(who) => who
                .value
                .iter()
                .map(|w| self.principal(w))
                .collect::<Option<Vec<_>>>(),
        }
    }

    /// `question:` of a decide step: instructions, and criteria shaped for its type.
    fn question(&mut self, q: Option<&S<Question>>, id_at: Location) -> Option<Question> {
        let Some(q) = q else {
            self.err(
                id_at,
                "`decide` steps need a `question` {type, instructions, criteria}",
            );
            return None;
        };
        let criteria_ok = match q.value.kind {
            QuestionKind::Choice => q.value.criteria.as_object().is_some_and(|c| !c.is_empty()),
            QuestionKind::Score => q.value.criteria.as_array().is_some_and(|c| !c.is_empty()),
            QuestionKind::Noul => !q.value.criteria.is_null(),
        };
        if q.value.instructions.trim().is_empty() {
            self.err(q.referenced, "`question.instructions` must not be empty");
        } else if !criteria_ok {
            self.err(
                q.referenced,
                "`question.criteria`: a map of labels for choice, a list of levels for score, the statement for noul",
            );
        } else {
            return Some(q.value.clone());
        }
        None
    }

    fn principal(&mut self, w: &S<String>) -> Option<String> {
        match w.value.split_once(':') {
            Some(("role" | "user", name)) if !name.is_empty() => Some(w.value.clone()),
            _ => {
                self.err(
                    w.referenced,
                    format!(
                        "invalid approver `{}`: use role:<name> or user:<name>",
                        w.value
                    ),
                );
                None
            }
        }
    }

    fn non_empty(
        &mut self,
        value: Option<&S<String>>,
        fallback: Location,
        missing: &str,
    ) -> Option<String> {
        match value {
            Some(v) if !v.value.trim().is_empty() => Some(v.value.clone()),
            Some(v) => {
                self.err(v.referenced, missing);
                None
            }
            None => {
                self.err(fallback, missing);
                None
            }
        }
    }

    /// `mcp:` grants: registered-server names (checked when the session
    /// starts), each once, never `sun` (Sun's own server).
    fn grants(&mut self, raw: Option<&RawGrants>) -> Vec<McpGrant> {
        let mut grants: Vec<McpGrant> = Vec::new();
        for grant in raw.map_or(&[][..], |g| &g.value) {
            let name = &grant.value.server;
            let message = if !is_slug(name) {
                format!("invalid mcp server name `{name}`: {SLUG_RULE}")
            } else if name == "sun" {
                "`sun` is Sun's own MCP server; every agent session already has it".to_owned()
            } else if grants.iter().any(|g| g.server == *name) {
                format!("mcp server `{name}` is granted twice")
            } else if let Some(tools) = &grant.value.tools
                && tools.iter().any(|t| t.trim().is_empty())
            {
                format!("mcp server `{name}`: empty tool name")
            } else {
                grants.push(grant.value.clone());
                continue;
            };
            self.err(grant.referenced, message);
        }
        grants
    }

    fn strings(&mut self, items: &[S<String>], what: &str) -> Vec<String> {
        for item in items.iter().filter(|s| s.value.trim().is_empty()) {
            self.err(item.referenced, format!("empty {what}"));
        }
        items.iter().map(|s| s.value.clone()).collect()
    }
}

/// The device verify extension step, which needs a PR before it.
const DEVICE_VERIFY: &str = "device.verify_on_device";
const SLUG_RULE: &str = "use a-z, 0-9, _ or -, starting with a letter";

fn at<T>(field: &Option<S<T>>) -> Option<Location> {
    field.as_ref().map(|s| s.referenced)
}

/// `[a-z][a-z0-9_-]*`, at most 64 chars (the [`StepKey`] rule).
fn is_slug(text: &str) -> bool {
    StepKey::new(text).is_ok()
}

/// A runtime or tool name: a slug, or `<extension>.<adapter>`.
fn is_runtime(text: &str) -> bool {
    is_slug(text) || (is_dotted(text) && text.matches('.').count() == 1)
}

/// A key-backed runtime's name (`Runtime::validate`): 1–64 of `[a-z0-9_-]`.
fn is_runtime_name(text: &str) -> bool {
    (1..=64).contains(&text.len())
        && text
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

/// A value passed to a CLI as one argument: 1–`max` characters of
/// `[A-Za-z0-9._:/@\[\]-]`, not starting with `-` (never read as an option).
/// Covers `gpt-5-codex`, `opus[1m]`, `openrouter/qwen/qwen3-coder`,
/// `qwen2.5-coder:7b`.
fn is_cli_token(text: &str, max: usize) -> bool {
    !text.is_empty()
        && text.len() <= max
        && !text.starts_with('-')
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._:/@[]-".contains(c))
}

/// Two or more slugs joined by `.`, e.g. `github.open_pr`.
fn is_dotted(text: &str) -> bool {
    text.contains('.') && text.split('.').all(is_slug)
}

/// `90s`, `30m`, `2h`, `1d`, `1h30m` → seconds. `None` if malformed or zero.
pub fn parse_duration(text: &str) -> Option<u64> {
    let mut total: u64 = 0;
    let mut number: Option<u64> = None;
    for c in text.trim().chars() {
        if let Some(digit) = c.to_digit(10) {
            number = Some(
                number
                    .unwrap_or(0)
                    .checked_mul(10)?
                    .checked_add(u64::from(digit))?,
            );
        } else {
            let unit = match c {
                's' => 1,
                'm' => 60,
                'h' => 3_600,
                'd' => 86_400,
                _ => return None,
            };
            total = total.checked_add(number.take()?.checked_mul(unit)?)?;
        }
    }
    (number.is_none() && total > 0).then_some(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn description_and_needs_repo_say_when_a_run_needs_a_repo() {
        let plain = parse("name: prd\nsteps:\n  - {id: a, type: agent, prompt: x}\n").unwrap();
        assert_eq!((plain.description.as_str(), plain.needs_repo), ("", false));
        assert!(!plain.is_coding());
        let described = parse(
            "name: prd\ndescription: '  Writes the PRD for VPS Infra  '\nneeds_repo: true\n\
             steps:\n  - {id: a, type: agent, prompt: x}\n",
        )
        .unwrap();
        assert_eq!(described.description, "Writes the PRD for VPS Infra");
        assert!(described.is_coding(), "needs_repo");
        assert_ne!(plain.hash, described.hash);
        let pr =
            parse("name: ship\nsteps:\n  - {id: pr, type: action, use: github.open_pr}\n").unwrap();
        assert!(pr.is_coding(), "open_pr");
        let gate =
            parse("name: ship\nsteps:\n  - {id: g, type: gate, gates: [ci_green]}\n").unwrap();
        assert!(gate.is_coding(), "ci gate");
        let other = parse("name: ship\nsteps:\n  - {id: g, type: gate, gates: [test]}\n").unwrap();
        assert!(!other.is_coding(), "a test gate runs in a folder too");
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("90s"), Some(90));
        assert_eq!(parse_duration("2h"), Some(7_200));
        assert_eq!(parse_duration("1h30m"), Some(5_400));
        assert_eq!(parse_duration("1d"), Some(86_400));
        for bad in [
            "",
            "5",
            "h",
            "0s",
            "2x",
            "1.5h",
            "-1h",
            "99999999999999999999d",
        ] {
            assert_eq!(parse_duration(bad), None, "{bad}");
        }
    }

    #[test]
    fn prompt_files_are_inlined_on_their_line() {
        for file in ["prompts/implement.md", "fix.txt", "a/b-c_d.md"] {
            assert!(is_prompt_file(file), "{file}");
        }
        for text in [
            "fix it",
            "../x.md",
            "/etc/x.md",
            "a//b.md",
            "x.rs",
            "$(id).md",
        ] {
            assert!(!is_prompt_file(text), "{text}");
        }
        let source = "name: w\nsteps:\n  - id: a\n    type: agent\n    prompt: prompts/a.md # file\n  - id: b\n    type: agent\n    prompt: 'b.md'\n";
        let texts = BTreeMap::from([
            ("a".to_owned(), "Line one\n\"quoted\" \\ é".to_owned()),
            ("b".to_owned(), "B".to_owned()),
        ]);
        let out = inline_prompts(source, &texts).unwrap();
        assert_eq!(out.lines().count(), source.lines().count());
        let prompts: Vec<String> = parse(&out)
            .unwrap()
            .steps
            .into_iter()
            .map(|s| match s.kind {
                StepKind::Agent { prompt, .. } => prompt,
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(prompts, [texts["a"].clone(), texts["b"].clone()]);
        let flow = "name: w\nsteps:\n  - { id: a, type: agent, prompt: a.md }\n";
        let texts = BTreeMap::from([("a".to_owned(), "A".to_owned())]);
        assert!(inline_prompts(flow, &texts).is_ok_and(|o| o.contains("\"A\"")));
    }

    #[test]
    fn dotted_names() {
        assert!(is_dotted("github.open_pr"));
        assert!(!is_dotted("github"));
        assert!(!is_dotted("github."));
        assert!(!is_dotted("GitHub.open"));
    }
}
