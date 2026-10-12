//! # sun-workflow — workflow files
//!
//! **Responsibility:** parse and validate workflow YAML with
//! line numbers in every error, assign versions, publish the JSON Schema.
//!
//! **Contract:**
//! - Input is untrusted: every field is validated; step ids become
//!   `sun_core::StepKey`, budgets become `sun_core::model::Budget`.
//! - Pure: no I/O beyond reading the bytes it is given.
//! - [`parse`] returns a [`Workflow`] or every [`WorkflowError`] it found, each
//!   with `line:column`. A YAML syntax/shape error stops at the first one.
//! - Version identity is [`Workflow::hash`] (SHA-256 of the normalized
//!   workflow): the store bumps `workflow_version.version` only when it changes.
//! - [`JSON_SCHEMA`] (`schema/workflow.schema.json`) describes the same format
//!   for editors; [`parse`] is the authority and also checks what JSON Schema
//!   can't (unique ids, `needs` refs, cycles, durations).
//!
//! **Patterns:** none beyond a two-layer parse (span-aware raw types →
//! validated model), so `serde-saphyr` types never leak to callers.
//!
//! **May depend on:** `sun-core`.

mod cron;
mod model;
mod parse;

pub use cron::Cron;
pub use model::*;
pub use parse::{WorkflowError, inline_prompts, is_prompt_file, parse, parse_duration};

/// The workflow every new install starts with:
/// implement → gates → open PR → human review.
pub const DEFAULT_WORKFLOW: &str = include_str!("../defaults/default.yaml");

/// A role template: a ready workflow file a new user installs from
/// the catalog. The files live in `templates/workflows/`; a test parses each.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Template {
    /// Stable id, also the file name (`templates/workflows/<id>.yaml`).
    pub id: &'static str,
    /// Who it is for: `engineering`, `marketing`, `product`, `leadership`,
    /// `forward-deployed`.
    pub role: &'static str,
    /// Display title.
    pub title: &'static str,
    /// One plain-language line on what it does.
    pub description: &'static str,
    /// The workflow file.
    pub source_yaml: &'static str,
}

impl Template {
    /// The mods (plugins and extensions) its steps, gates and triggers need.
    pub fn mods(&self) -> Vec<String> {
        parse(self.source_yaml)
            .map(|w| w.mods())
            .unwrap_or_default()
    }
}

/// The role templates that ship with Sun.
pub const TEMPLATES: &[Template] = &[
    Template {
        id: "bug-fix",
        role: "engineering",
        title: "Bug fix → verified PR",
        description: "Reproduce with a failing test, fix it, pass the gates, open a PR for review.",
        source_yaml: include_str!("../../../templates/workflows/bug-fix.yaml"),
    },
    Template {
        id: "feature-across-repos",
        role: "engineering",
        title: "Feature across repos",
        description: "Plan a feature that spans several repos, build it in each, open one PR per repo and merge them in order.",
        source_yaml: include_str!("../../../templates/workflows/feature-across-repos.yaml"),
    },
    Template {
        id: "ui-refactor",
        role: "engineering",
        title: "UI refactor with the design canvas",
        description: "Refactor a screen, pass the code gates, review the before and after side by side in the canvas, then open a PR.",
        source_yaml: include_str!("../../../templates/workflows/ui-refactor.yaml"),
    },
    Template {
        id: "marketing-campaign",
        role: "marketing",
        title: "Campaign: brief → drafts → approval",
        description: "Turn a request into a brief and channel posts, check them against the brand, then schedule them once approved.",
        source_yaml: include_str!("../../../templates/workflows/marketing-campaign.yaml"),
    },
    Template {
        id: "prd",
        role: "product",
        title: "Interview notes → PRD → tasks",
        description: "Sum up interview notes, draft a PRD with the required sections, then write its user stories as a Jira import file and report back to the Jira issue that started it.",
        source_yaml: include_str!("../../../templates/workflows/prd.yaml"),
    },
    Template {
        id: "client-deployment",
        role: "forward-deployed",
        title: "Client deployment",
        description: "Plan a client rollout, apply the configuration with tests, get sign-off, open a PR and tell the team.",
        source_yaml: include_str!("../../../templates/workflows/client-deployment.yaml"),
    },
    Template {
        id: "weekly-summary",
        role: "leadership",
        title: "Weekly summary: work, cost, blockers",
        description: "Every Monday, sum up what shipped, what it cost and what is blocked, get it approved, save it to Drive and tell the team.",
        source_yaml: include_str!("../../../templates/workflows/weekly-summary.yaml"),
    },
];

/// The published JSON Schema of the workflow format.
pub const JSON_SCHEMA: &str = include_str!("../../../schema/workflow.schema.json");

/// SHA-256 hex of [`JSON_SCHEMA`], for `workflow_version.schema_hash`.
pub fn schema_hash() -> String {
    sha256_hex(JSON_SCHEMA.as_bytes())
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
