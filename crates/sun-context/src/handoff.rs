//! Hand-off packs: built from the ledger in milliseconds, with no
//! LLM call, so a dying or rate-limited agent is never needed to hand off.

use std::fmt::Write as _;

use serde::{Deserialize, Serialize};
use sun_core::model::{Note, NoteKind};
use sun_core::redact::redact;
use sun_core::{StepBrief, StepKey, SunEvent, Timestamp, TodoStatus};

/// `transcript_tail` keeps about the last 3k tokens (≈ 4 bytes/token).
const TRANSCRIPT_TAIL_BYTES: usize = 12_000;
/// How many recent errors `last_errors` keeps.
const LAST_ERRORS: usize = 5;

/// The conversation of `events` as plain text (agent and user turns, tool
/// calls), unredacted: the pack keeps its tail, the engine stores it whole
/// behind `transcript_ref`.
pub fn transcript(events: &[SunEvent]) -> String {
    let mut transcript = String::new();
    for event in events {
        // Writing to a String cannot fail.
        let _ = match event {
            SunEvent::Text { role, text } => writeln!(transcript, "{role}: {text}\n"),
            SunEvent::ToolCall { name, summary, .. } => {
                writeln!(transcript, "tool {name}: {summary}\n")
            }
            _ => Ok(()),
        };
    }
    transcript
}

/// The runtime-neutral hand-off pack, a fixed JSON shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HandoffPack {
    /// Task title.
    pub task: String,
    /// The step being continued.
    pub step: StepKey,
    /// Attempt number of the new session's step.
    pub attempt: u32,
    /// Who handed off and why.
    pub from: HandoffFrom,
    /// The plan: the agent's latest todo list plus open `todo` notes.
    pub plan: Vec<PlanItem>,
    /// Decisions recorded with `sun.note(kind=decision)`.
    pub decisions: Vec<String>,
    /// The latest `.sun/progress.md`.
    pub progress_md: String,
    /// Git state read by the engine; `None` for tasks with no repo.
    pub git: Option<GitState>,
    /// The most recent errors, oldest first.
    pub last_errors: Vec<String>,
    /// Latest gate results.
    pub gates: Vec<GateResult>,
    /// The last ~3k tokens of the conversation, verbatim.
    pub transcript_tail: String,
    /// Pointer to the full transcript, fetched with `sun.get`.
    pub transcript_ref: Option<String>,
}

/// `from` of a [`HandoffPack`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HandoffFrom {
    /// Runtime name, e.g. `claude-code`.
    pub runtime: String,
    /// Why the session ended.
    pub reason: HandoffReason,
    /// When the limit resets, for `limit_hit`.
    pub reset_at: Option<Timestamp>,
}

/// Why work moves to a new session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HandoffReason {
    /// The runtime hit a rate limit or quota.
    LimitHit,
    /// A human switched runtime.
    ManualSwitch,
    /// The context window filled up.
    ContextRotation,
    /// The watchdog restarted a stalled session.
    WatchdogRestart,
    /// The host running the session was lost.
    HostLost,
    /// A failed attempt of the step is tried again.
    Retry,
    /// The preferred runtime's cooldown ended (`runtime.switch_back`).
    SwitchBack,
}

impl HandoffReason {
    /// The snake_case wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::LimitHit => "limit_hit",
            Self::ManualSwitch => "manual_switch",
            Self::ContextRotation => "context_rotation",
            Self::WatchdogRestart => "watchdog_restart",
            Self::HostLost => "host_lost",
            Self::Retry => "retry",
            Self::SwitchBack => "switch_back",
        }
    }
}

/// One plan entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanItem {
    /// What to do.
    pub todo: String,
    /// Progress.
    pub status: TodoStatus,
}

/// Git state of the task branch, read by the engine after its checkpoint commit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitState {
    /// Task branch.
    pub branch: String,
    /// Commits on the branch, `<short sha> <subject>`, oldest first.
    pub commits: Vec<String>,
    /// `git diff --shortstat` against the base, e.g. `3 files +120 -8`.
    pub diff_stat: String,
    /// There was uncommitted work (now in the checkpoint commit).
    pub uncommitted: bool,
}

/// One gate result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GateResult {
    /// Gate name, e.g. `test`.
    pub name: String,
    /// Whether it passed.
    pub pass: bool,
}

/// What [`build_handoff`] reads. Everything is already in the ledger or read
/// by the engine; nothing comes from the agent being replaced.
#[derive(Debug, Clone, Copy)]
pub struct HandoffInput<'a> {
    /// The step brief (task title and step).
    pub brief: &'a StepBrief,
    /// Attempt number of the new session's step.
    pub attempt: u32,
    /// Who handed off and why.
    pub from: &'a HandoffFrom,
    /// The ending session's events, in order.
    pub events: &'a [SunEvent],
    /// Notes of the run.
    pub notes: &'a [Note],
    /// The latest `.sun/progress.md`.
    pub progress_md: &'a str,
    /// Git state, `None` without a repo.
    pub git: Option<&'a GitState>,
    /// Latest gate results.
    pub gates: &'a [GateResult],
    /// Pointer to the full transcript.
    pub transcript_ref: Option<&'a str>,
    /// Secret values to redact besides the built-in patterns.
    pub known_secrets: &'a [&'a str],
}

/// Builds a hand-off pack from the ledger. Pure and LLM-free; every text field
/// goes through the shared secret filter.
pub fn build_handoff(input: HandoffInput<'_>) -> HandoffPack {
    let clean = |text: &str| redact(text, input.known_secrets);
    let live = |kind: NoteKind| {
        input
            .notes
            .iter()
            .filter(move |note| note.kind == kind && note.superseded_by.is_none())
    };

    let mut plan: Vec<PlanItem> = input
        .events
        .iter()
        .rev()
        .find_map(|event| match event {
            SunEvent::Plan { todos } => Some(todos),
            _ => None,
        })
        .into_iter()
        .flatten()
        .map(|todo| PlanItem {
            todo: clean(&todo.text),
            status: todo.status,
        })
        .collect();
    for note in live(NoteKind::Todo) {
        let todo = clean(&note.body);
        if !plan.iter().any(|item| item.todo == todo) {
            plan.push(PlanItem {
                todo,
                status: TodoStatus::Pending,
            });
        }
    }

    let errors: Vec<String> = input
        .events
        .iter()
        .filter_map(|event| match event {
            SunEvent::Error { message } => Some(clean(message)),
            SunEvent::ToolResult { ok: false, summary } => Some(clean(summary)),
            _ => None,
        })
        .collect();
    let last_errors = errors[errors.len().saturating_sub(LAST_ERRORS)..].to_vec();

    let transcript = transcript(input.events);

    HandoffPack {
        task: clean(&input.brief.title),
        step: input.brief.step_key.clone(),
        attempt: input.attempt,
        from: input.from.clone(),
        plan,
        decisions: live(NoteKind::Decision).map(|n| clean(&n.body)).collect(),
        progress_md: clean(input.progress_md),
        git: input.git.cloned(),
        last_errors,
        gates: input.gates.to_vec(),
        transcript_tail: clean(tail(&transcript, TRANSCRIPT_TAIL_BYTES)),
        transcript_ref: input.transcript_ref.map(str::to_owned),
    }
}

/// The last `max_bytes` of `text`, cut on a char boundary.
pub(crate) fn tail(text: &str, max_bytes: usize) -> &str {
    let mut start = text.len().saturating_sub(max_bytes);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
}

impl HandoffPack {
    /// The short form for a context pack's P1 item: todos, decisions, diff
    /// stat, last errors and gates.
    pub fn summary_markdown(&self) -> String {
        let mut out = format!(
            "Continuing `{}` (attempt {}) after **{}** stopped: {}.\n",
            self.step,
            self.attempt,
            self.from.runtime,
            self.from.reason.as_str(),
        );
        if !self.plan.is_empty() {
            out.push_str("\nPlan:\n");
            for item in &self.plan {
                let mark = if item.status == TodoStatus::Done {
                    'x'
                } else {
                    ' '
                };
                let _ = writeln!(out, "- [{mark}] {} ({})", item.todo, item.status);
            }
        }
        list(&mut out, "Decisions", &self.decisions);
        if let Some(git) = &self.git {
            let _ = writeln!(
                out,
                "\nGit: branch `{}`, {} commit(s), {}{}",
                git.branch,
                git.commits.len(),
                git.diff_stat,
                if git.uncommitted {
                    " (uncommitted work saved in a checkpoint commit)"
                } else {
                    ""
                },
            );
        }
        list(&mut out, "Last errors", &self.last_errors);
        if !self.gates.is_empty() {
            out.push_str("\nGates:\n");
            for gate in &self.gates {
                let verdict = if gate.pass { "pass" } else { "FAIL" };
                let _ = writeln!(out, "- {}: {verdict}", gate.name);
            }
        }
        out
    }

    /// The full pack as Markdown, for `.sun/context/handoff.md`.
    pub fn render_markdown(&self) -> String {
        let mut out = format!("# Hand-off: {}\n\n{}", self.task, self.summary_markdown());
        if let Some(git) = &self.git {
            list(&mut out, "Commits", &git.commits);
        }
        if !self.progress_md.is_empty() {
            let _ = write!(
                out,
                "\n## Progress notes\n\n{}\n",
                self.progress_md.trim_end()
            );
        }
        if !self.transcript_tail.is_empty() {
            let _ = write!(
                out,
                "\n## Transcript tail\n\n{}\n",
                self.transcript_tail.trim_end()
            );
        }
        if let Some(reference) = &self.transcript_ref {
            let _ = write!(
                out,
                "\nFull transcript: `{reference}` (fetch with `sun` `get`).\n"
            );
        }
        out
    }
}

fn list(out: &mut String, title: &str, items: &[String]) {
    if items.is_empty() {
        return;
    }
    let _ = writeln!(out, "\n{title}:");
    for item in items {
        let _ = writeln!(out, "- {item}");
    }
}
