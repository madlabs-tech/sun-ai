//! The context pack compiler. [`compile`] is a pure function: no
//! I/O, no clock, no randomness, so the same ledger gives the same pack.

use std::fmt::Write as _;

use serde::Serialize;
use sun_core::model::{MemoryItem, MemoryScope, MemoryStatus};
use sun_core::{Envelope, StepBrief, StepKey};

use crate::handoff::HandoffPack;
use crate::knowledge::{FlagKind, KnowledgeFlag, unique_flags};

/// Bytes per token for the fast estimate.
// ponytail: one ratio for every runtime; correct it per runtime from real
// `Usage` events once the engine records them.
pub const BYTES_PER_TOKEN: usize = 4;

/// Share of the runtime's window a pack may use: `B = min(step budget, 10%)`.
const WINDOW_SHARE_DIVISOR: u32 = 10;
/// Lines of the hand-off summary pushed into the envelope.
const ENVELOPE_HANDOFF_LINES: usize = 12;

/// Where the files live, relative to the session dir.
pub const PACK_PATH: &str = ".sun/context/pack.md";
/// The full hand-off pack.
pub const HANDOFF_PATH: &str = ".sun/context/handoff.md";
/// Knowledge snippets, one file each.
pub const SOURCES_DIR: &str = ".sun/context/sources";
/// The agent's progress notes.
pub const PROGRESS_PATH: &str = ".sun/progress.md";

/// Fast token estimate: ≈ [`BYTES_PER_TOKEN`] bytes per token, rounded up.
pub fn estimate_tokens(text: &str) -> u32 {
    u32::try_from(text.len().div_ceil(BYTES_PER_TOKEN)).unwrap_or(u32::MAX)
}

/// Ledger facts a pack draws on: the hand-off (P1), earlier outputs (P2) and
/// the repo's design system (P3).
#[derive(Debug, Clone, Default)]
pub struct LedgerView {
    /// Set when the step continues another session's work.
    pub handoff: Option<HandoffPack>,
    /// Outputs of the steps this step depends on.
    pub step_outputs: Vec<StepOutput>,
    /// The repo's `DESIGN.md` and token files, for UI work.
    pub design: Vec<DesignFile>,
    /// The workspace's standing instructions (its profile `context`); empty: none.
    pub workspace_context: String,
    /// `(url, description)` of the task workspace's repos.
    pub repos: Vec<(String, String)>,
}

/// A design-system file read from the task's repo.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DesignFile {
    /// Path relative to the repo root, e.g. `DESIGN.md`.
    pub path: String,
    /// Its text (possibly cut short; the whole file is in the worktree).
    pub text: String,
}

/// An earlier step's output (P2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepOutput {
    /// The step.
    pub step_key: StepKey,
    /// Its summary.
    pub summary: String,
    /// Pointer to the full output, e.g. `artifact:<id>`.
    pub pointer: String,
}

/// A knowledge search hit (P4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnowledgeHit {
    /// Citation: doc title plus version or date.
    pub citation: String,
    /// The snippet.
    pub text: String,
    /// Pointer to the whole doc, for `sun.get`.
    pub pointer: String,
    /// Stale or conflict flags this doc is part of.
    pub flags: Vec<KnowledgeFlag>,
}

/// A compiled context pack: what one agent session is given.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ContextPack {
    /// The step.
    pub step_key: StepKey,
    /// Budget `B` in tokens.
    pub budget_tokens: u32,
    /// Estimated tokens of the rendered `pack.md`.
    pub token_estimate: u32,
    /// Every candidate item in priority order, included or trimmed to a pointer.
    pub items: Vec<PackItem>,
}

/// One candidate item of a pack.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PackItem {
    /// 0 (task) to 4 (knowledge); pointers to trimmed items are P5.
    pub priority: u8,
    /// Heading.
    pub label: String,
    /// Where the whole item can be read or fetched.
    pub pointer: String,
    /// Estimated tokens of `body`.
    pub tokens: u32,
    /// In the pack, or trimmed to its pointer.
    pub included: bool,
    /// Content. Not part of the stored item list (the pack blob has it).
    #[serde(skip)]
    pub body: String,
}

/// A file to write in the session dir: the host protocol's `worktree.write` file.
pub use sun_core::host_protocol::SessionFile;

/// The label of the workspace's standing instructions in a pack.
pub const WORKSPACE_CONTEXT: &str = "Workspace context";

/// Compiles the context pack for one agent step.
///
/// Budget `B = min(step budget, 10% of context_window)`. Items are taken in
/// priority order (P0 task and the workspace context, P1 hand-off, P2 earlier outputs, P3 the repo's
/// design system then active memory, P4 knowledge); one that doesn't fit becomes a one-line pointer (P5). P0 is
/// always included, and so are the workspace's repos ("Repositories agents
/// can use") and the knowledge hits' flags, as P0 items after the task.
pub fn compile(
    step: &StepBrief,
    ledger: &LedgerView,
    memory: &[MemoryItem],
    knowledge: &[KnowledgeHit],
    context_window: u32,
) -> ContextPack {
    let window_share = context_window / WINDOW_SHARE_DIVISOR;
    let budget = step
        .context_budget_tokens
        .map_or(window_share, |b| b.min(window_share));

    let mut items = vec![item(
        0,
        format!("Task: {}", step.title),
        "sun.task",
        brief_markdown(step),
    )];
    if !ledger.workspace_context.trim().is_empty() {
        items.push(item(
            0,
            WORKSPACE_CONTEXT.into(),
            "sun.task",
            ledger.workspace_context.trim().to_owned(),
        ));
    }
    if !ledger.repos.is_empty() {
        items.push(item(
            0,
            "Repositories agents can use".into(),
            "sun.task",
            repos_markdown(&ledger.repos),
        ));
    }
    let flags = unique_flags(knowledge);
    if !flags.is_empty() {
        items.push(item(
            0,
            "Knowledge warnings".into(),
            "sun.task",
            warnings_markdown(&flags),
        ));
    }
    if let Some(handoff) = &ledger.handoff {
        items.push(item(
            1,
            "Hand-off".into(),
            HANDOFF_PATH,
            handoff.summary_markdown(),
        ));
        if !handoff.transcript_tail.is_empty() {
            items.push(item(
                1,
                "Transcript tail".into(),
                HANDOFF_PATH,
                handoff.transcript_tail.clone(),
            ));
        }
    }
    for output in &ledger.step_outputs {
        items.push(item(
            2,
            format!("Output of step `{}`", output.step_key),
            &output.pointer,
            output.summary.clone(),
        ));
    }
    // A trimmed design file's pointer is the file itself, in the worktree.
    for file in &ledger.design {
        items.push(item(
            3,
            format!("Design system: {}", file.path),
            &file.path,
            file.text.clone(),
        ));
    }
    let mut active: Vec<&MemoryItem> = memory
        .iter()
        .filter(|m| m.status == MemoryStatus::Active)
        .collect();
    // Most specific scope first (workflow > repo > ecosystem).
    active.sort_by_key(|m| match m.scope {
        MemoryScope::Workflow => 0,
        MemoryScope::Repo => 1,
        MemoryScope::Ecosystem => 2,
    });
    for memory in active {
        items.push(item(
            3,
            format!("Memory ({})", memory.scope),
            &format!("memory:{}", memory.id),
            memory.body.clone(),
        ));
    }
    for (index, hit) in knowledge.iter().enumerate() {
        items.push(item(
            4,
            format!("Source: {}", hit.citation),
            &source_path(index),
            format!("{}\n\n(full doc: `{}`)", hit.text, hit.pointer),
        ));
    }

    let mut used = 0u32;
    for item in &mut items {
        if item.priority == 0 || used.saturating_add(item.tokens) <= budget {
            item.included = true;
            used = used.saturating_add(item.tokens);
        } else {
            used = used.saturating_add(estimate_tokens(&pointer_line(item)));
        }
    }

    let mut pack = ContextPack {
        step_key: step.step_key.clone(),
        budget_tokens: budget,
        token_estimate: 0,
        items,
    };
    pack.token_estimate = estimate_tokens(&pack.render_markdown());
    pack
}

fn item(priority: u8, label: String, pointer: &str, body: String) -> PackItem {
    PackItem {
        priority,
        label,
        pointer: pointer.to_owned(),
        tokens: estimate_tokens(&body),
        included: false,
        body,
    }
}

fn source_path(index: usize) -> String {
    format!("{SOURCES_DIR}/{:02}.md", index + 1)
}

fn pointer_line(item: &PackItem) -> String {
    format!(
        "- {} (~{} tokens): `{}`\n",
        item.label, item.tokens, item.pointer
    )
}

fn repos_markdown(repos: &[(String, String)]) -> String {
    let mut out = String::new();
    for (url, description) in repos {
        match description.trim() {
            "" => _ = writeln!(out, "- {url}"),
            text => _ = writeln!(out, "- {url}: {}", text.replace('\n', " ")),
        }
    }
    out
}

fn warnings_markdown(flags: &[KnowledgeFlag]) -> String {
    let mut out = String::from(
        "Some sources below are stale or disagree. Don't pick one silently: say which \
         one you relied on and why, or ask with the `sun` MCP tool `ask`.\n\n",
    );
    for flag in flags {
        let kind = match flag.kind {
            FlagKind::Stale => "Stale",
            FlagKind::Conflict => "Conflict",
        };
        let _ = writeln!(out, "- **{kind}:** {}", flag.message);
        for (citation, pointer) in flag.citations.iter().zip(&flag.pointers) {
            let _ = writeln!(out, "  - {citation} (`{pointer}`)");
        }
    }
    out
}

fn brief_markdown(step: &StepBrief) -> String {
    brief_sections(
        step,
        &[
            ("Constraints", &step.constraints),
            ("Gates the step must pass", &step.gates),
            ("Acceptance criteria", &step.acceptance),
        ],
    )
}

/// The envelope's `task`: the brief without its constraints and gates,
/// which the template renders in their own sections.
fn task_markdown(step: &StepBrief) -> String {
    brief_sections(step, &[("Acceptance criteria", &step.acceptance)])
}

fn brief_sections(step: &StepBrief, sections: &[(&str, &Vec<String>)]) -> String {
    let mut out = format!("**{}**\n\n{}\n\n", step.title, step.body.trim_end());
    let _ = writeln!(out, "Step `{}`: {}", step.step_key, step.goal);
    for (title, lines) in sections {
        if !lines.is_empty() {
            let _ = writeln!(out, "\n{title}:");
            for line in *lines {
                let _ = writeln!(out, "- {line}");
            }
        }
    }
    out
}

impl ContextPack {
    /// Runs every item's label, pointer and body through
    /// [`sun_core::redact::redact`] with `known_secrets` (from
    /// `sun_store::SecretRepo::known_secrets`). Call it right after
    /// [`compile`]: memory, knowledge and the task text are not redacted on
    /// the way in, and every output (`pack.md`, envelope, files, stored blob)
    /// is rendered from the items.
    pub fn redact(&mut self, known_secrets: &[&str]) {
        for item in &mut self.items {
            for text in [&mut item.label, &mut item.pointer, &mut item.body] {
                *text = sun_core::redact::redact(text, known_secrets);
            }
        }
    }

    /// Timeline line, e.g. `12.1k tokens · 3 docs · 2 trimmed (pointers)`.
    pub fn summary(&self) -> String {
        let docs = self
            .items
            .iter()
            .filter(|i| i.priority == 4 && i.included)
            .count();
        let trimmed = self.items.iter().filter(|i| !i.included).count();
        let tokens = if self.token_estimate >= 1000 {
            format!("{:.1}k", f64::from(self.token_estimate) / 1000.0)
        } else {
            self.token_estimate.to_string()
        };
        format!("{tokens} tokens · {docs} docs · {trimmed} trimmed (pointers)")
    }

    /// `.sun/context/pack.md`: included items, then a pointer for every
    /// trimmed one. This is also the stored, hashed form of the pack.
    pub fn render_markdown(&self) -> String {
        let mut out = format!("# Context for step `{}`\n", self.step_key);
        for item in self.items.iter().filter(|i| i.included) {
            let _ = write!(out, "\n## {}\n\n{}\n", item.label, item.body.trim_end());
        }
        let trimmed: Vec<&PackItem> = self.items.iter().filter(|i| !i.included).collect();
        if !trimmed.is_empty() {
            out.push_str(
                "\n## Pointers\n\nNot included to save room. Read the file or fetch the id \
                 with the `sun` MCP tool `get` if you need it.\n\n",
            );
            for item in trimmed {
                out.push_str(&pointer_line(item));
            }
        }
        out
    }

    /// The text pushed into the first prompt: P0, the first lines of the
    /// hand-off, and where to read the rest.
    pub fn envelope(&self) -> String {
        let mut out = String::new();
        for (index, item) in self.items.iter().filter(|i| i.priority == 0).enumerate() {
            if index > 0 {
                let _ = write!(out, "\n\n**{}**\n\n", item.label);
            }
            out.push_str(item.body.trim_end());
        }
        if let Some(handoff) = self.handoff_head() {
            out.push_str("\n\n");
            out.push_str(&handoff);
        }
        let _ = write!(
            out,
            "\n\nRead `{PACK_PATH}` for the full context; fetch any pointer in it by reading \
             the file or with the `sun` MCP tool `get`. Keep `{PROGRESS_PATH}` current \
             (done, next, open questions) and commit it with your work.\n"
        );
        out
    }

    /// The same facts as [`Self::envelope`], split for the per-CLI templates.
    /// `step` is the brief the pack was compiled from; its text is
    /// redacted with `known_secrets` like the pack's items.
    pub fn envelope_parts(&self, step: &StepBrief, known_secrets: &[&str]) -> Envelope {
        let redact = |text: &str| sun_core::redact::redact(text, known_secrets);
        let mut task = redact(&task_markdown(step));
        // Other P0 items (workspace context, knowledge warnings) ride with the task.
        for item in self.items.iter().filter(|i| i.priority == 0).skip(1) {
            let _ = writeln!(task, "\n**{}**\n\n{}", item.label, item.body.trim_end());
        }
        Envelope {
            task,
            constraints: step.constraints.iter().map(|c| redact(c)).collect(),
            gates: step.gates.iter().map(|g| redact(g)).collect(),
            context: vec![
                format!(
                    "`{PACK_PATH}`: the full context; fetch any pointer in it by reading \
                     the file or with the `sun` MCP tool `get`."
                ),
                format!(
                    "`{PROGRESS_PATH}`: keep it current (done, next, open questions) and \
                     commit it with your work."
                ),
            ],
            handoff: self.handoff_head(),
        }
    }

    /// The first lines of the hand-off, if the pack has one.
    fn handoff_head(&self) -> Option<String> {
        let item = self
            .items
            .iter()
            .find(|i| i.priority == 1 && i.label == "Hand-off")?;
        let mut out = String::new();
        for line in item.body.lines().take(ENVELOPE_HANDOFF_LINES) {
            out.push_str(line);
            out.push('\n');
        }
        Some(out)
    }

    /// The `.sun/` files for this pack. Pure: write them with
    /// [`crate::write_files`] on the session's host.
    pub fn files(&self, handoff: Option<&HandoffPack>) -> Vec<SessionFile> {
        let file = |path: &str, contents: String, overwrite| SessionFile {
            path: path.to_owned(),
            contents,
            overwrite,
        };
        let mut files = vec![
            // Context files are derived; keep them out of git.
            file(".sun/context/.gitignore", "*\n".into(), true),
            file(PACK_PATH, self.render_markdown(), true),
        ];
        if let Some(handoff) = handoff {
            files.push(file(HANDOFF_PATH, handoff.render_markdown(), true));
        }
        for item in self.items.iter().filter(|i| i.priority == 4) {
            let source = format!("# {}\n\n{}\n", item.label, item.body.trim_end());
            files.push(file(&item.pointer, source, true));
        }
        let progress = handoff
            .map(|h| h.progress_md.clone())
            .filter(|p| !p.trim().is_empty())
            .unwrap_or_else(|| {
                "# Progress\n\nKeep this current: what is done, what is next, open \
                 questions. Commit it with your work.\n"
                    .into()
            });
        files.push(file(PROGRESS_PATH, progress, false));
        files
    }
}
