//! Domain types, one per table. Field names match DB columns; `*_json`
//! columns are typed fields (or [`serde_json::Value`] where the shape belongs
//! to a later phase). Big content is a [`BlobHash`].
//!
//! Not here yet: `knowledge_source`/`doc`/`chunk` (row types live in `sun-store`).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    AgentId, ArtifactId, AuditId, BlobHash, ContextPackId, CorrectionId, EcosystemId, EvalRunId,
    EvalSetId, HandoffPackId, HostId, IdemKey, MemberId, MemoryItemId, NoteId, OutboxId, PinId,
    ProjectId, PullRequestId, RepoId, RunId, RunStatus, RuntimeId, SessionId, StepId, StepKey,
    StepStatus, TaskActivityId, TaskId, TaskStatus, Timestamp, VcsConnectionId, WorkflowId,
    WorkspaceId,
};

/// `repo` — one of a workspace's repositories (`{url, description}`): what
/// agents and workflow runs resolve by name. Unique per `(workspace, url)`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Repo {
    /// Id.
    pub id: RepoId,
    /// The workspace it belongs to.
    pub workspace_id: WorkspaceId,
    /// Clone URL.
    pub url: String,
    /// Default branch, e.g. `main`.
    pub default_branch: String,
    /// Gate configuration (test/lint commands, ...).
    pub gate_config: Value,
    /// What agents should know it for (listed in every run's context pack).
    #[serde(default)]
    pub description: String,
    /// The GitLab project hook Sun made for it, if any.
    #[serde(default)]
    pub hook_id: Option<i64>,
}

/// A workspace's git rules (`workspace.git_settings_json`); every switch is
/// on by default, so `{}` reads as all on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GitSettings {
    /// Mirror pull requests from GitHub and GitLab deliveries at all.
    pub github_enabled: bool,
    /// Show linked pull requests on the task page.
    pub pr_sidebar: bool,
    /// Link a PR to the task whose key is in its title, branch or body.
    pub auto_link: bool,
    /// Move a task once every linked PR has merged (workflow tasks to
    /// `done`, agent tasks to `needs_you`).
    pub move_on_merge: bool,
}

impl Default for GitSettings {
    fn default() -> Self {
        Self {
            github_enabled: true,
            pr_sidebar: true,
            auto_link: true,
            move_on_merge: true,
        }
    }
}

/// `PATCH /workspaces/{id} {git_settings}`: the switches to change; the
/// others keep their value. Only an object of known switches reads: an
/// unknown key, a list or a string is refused.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GitSettingsPatch {
    /// See [`GitSettings::github_enabled`].
    pub github_enabled: Option<bool>,
    /// See [`GitSettings::pr_sidebar`].
    pub pr_sidebar: Option<bool>,
    /// See [`GitSettings::auto_link`].
    pub auto_link: Option<bool>,
    /// See [`GitSettings::move_on_merge`].
    pub move_on_merge: Option<bool>,
}

impl GitSettingsPatch {
    /// `current` with the switches this patch names changed.
    pub fn apply(self, current: GitSettings) -> GitSettings {
        GitSettings {
            github_enabled: self.github_enabled.unwrap_or(current.github_enabled),
            pr_sidebar: self.pr_sidebar.unwrap_or(current.pr_sidebar),
            auto_link: self.auto_link.unwrap_or(current.auto_link),
            move_on_merge: self.move_on_merge.unwrap_or(current.move_on_merge),
        }
    }
}

impl<'de> Deserialize<'de> for GitSettingsPatch {
    /// An object only: serde would read a struct from a list too.
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Switches {
            github_enabled: Option<bool>,
            pr_sidebar: Option<bool>,
            auto_link: Option<bool>,
            move_on_merge: Option<bool>,
        }
        let value = Value::deserialize(deserializer)?;
        if !value.is_object() {
            return Err(serde::de::Error::custom(
                "git_settings must be an object of switches",
            ));
        }
        let switches: Switches = serde_json::from_value(value).map_err(serde::de::Error::custom)?;
        Ok(Self {
            github_enabled: switches.github_enabled,
            pr_sidebar: switches.pr_sidebar,
            auto_link: switches.auto_link,
            move_on_merge: switches.move_on_merge,
        })
    }
}

string_enum! {
    /// Where a mirrored [`PullRequest`] stands on its git host.
    pub enum PrState {
        /// Open and ready for review.
        Open => "open",
        /// Closed without merging.
        Closed => "closed",
        /// Merged.
        Merged => "merged",
        /// Open as a draft (GitLab: draft or work in progress).
        Draft => "draft",
    }
}

/// `pull_request` — a pull request (or GitLab merge request) mirrored from a
/// git host's deliveries; `provider` says which. Unique per
/// `(workspace, repo_owner, repo_name, number)`. The `checks_*`, `mergeable`
/// and `merge_state_status` columns are a snapshot fetched separately
/// ([`PrSnapshot`]) and valid for `snapshot_head_sha` only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequest {
    /// Id.
    pub id: PullRequestId,
    /// The workspace it was mirrored into.
    pub workspace_id: WorkspaceId,
    /// GitHub or GitLab.
    pub provider: VcsProvider,
    /// GitHub: the App installation that delivered it.
    pub installation_id: Option<i64>,
    /// Repository owner (GitLab: the namespace).
    pub repo_owner: String,
    /// Repository name.
    pub repo_name: String,
    /// PR number (GitLab: the MR `iid`).
    pub number: i64,
    /// Title.
    pub title: String,
    /// State.
    pub state: PrState,
    /// Link to it.
    pub html_url: String,
    /// Head branch.
    pub branch: Option<String>,
    /// Author's login.
    pub author_login: Option<String>,
    /// Author's avatar.
    pub author_avatar_url: Option<String>,
    /// When it merged.
    pub merged_at: Option<Timestamp>,
    /// When it closed.
    pub closed_at: Option<Timestamp>,
    /// When the host says it was created.
    pub pr_created_at: Timestamp,
    /// When the host says it last changed; an older delivery is stale.
    pub pr_updated_at: Timestamp,
    /// Head commit.
    pub head_sha: String,
    /// Lines added.
    pub additions: i64,
    /// Lines deleted.
    pub deletions: i64,
    /// Files changed.
    pub changed_files: i64,
    /// Snapshot: GitHub `mergeable` (`MERGEABLE`, `CONFLICTING`, `UNKNOWN`).
    pub mergeable: Option<String>,
    /// Snapshot: GitHub `mergeStateStatus` (`CLEAN`, `BLOCKED`, `BEHIND`, ...).
    pub merge_state_status: Option<String>,
    /// Snapshot: the checks' rollup (`SUCCESS`, `FAILURE`, `PENDING`, ...).
    pub checks_rollup_state: Option<String>,
    /// Snapshot: how many checks ran.
    pub checks_total: i64,
    /// Snapshot: how many passed.
    pub checks_passed: i64,
    /// Snapshot: how many failed.
    pub checks_failed: i64,
    /// Snapshot: how many are still running.
    pub checks_pending: i64,
    /// The head the snapshot was taken for (empty: none yet).
    pub snapshot_head_sha: String,
    /// When the snapshot was taken.
    pub snapshot_fetched_at: Option<Timestamp>,
}

impl PullRequest {
    /// `owner/name`.
    pub fn repo(&self) -> String {
        format!("{}/{}", self.repo_owner, self.repo_name)
    }

    /// True once a snapshot was taken for the current head.
    pub fn snapshot_available(&self) -> bool {
        !self.snapshot_head_sha.is_empty() && self.snapshot_head_sha == self.head_sha
    }
}

/// The checks and mergeability of a [`PullRequest`] at one head commit, as
/// fetched from the host; stored only while `head_sha` is still the PR's head.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrSnapshot {
    /// The head it was fetched for.
    pub head_sha: String,
    /// See [`PullRequest::mergeable`].
    pub mergeable: Option<String>,
    /// See [`PullRequest::merge_state_status`].
    pub merge_state_status: Option<String>,
    /// See [`PullRequest::checks_rollup_state`].
    pub checks_rollup_state: Option<String>,
    /// Checks run.
    pub checks_total: i64,
    /// Checks passed.
    pub checks_passed: i64,
    /// Checks failed.
    pub checks_failed: i64,
    /// Checks pending.
    pub checks_pending: i64,
}

/// `task_pull_request` — a link between a task and a mirrored PR.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskPullRequest {
    /// The task.
    pub task_id: TaskId,
    /// The pull request.
    pub pull_request_id: PullRequestId,
    /// Who linked it by hand; `None`: linked automatically (the task's key
    /// in the PR's title, branch or body).
    pub linked_by: Option<MemberId>,
    /// When.
    pub linked_at: Timestamp,
}

string_enum! {
    /// The git host of a [`VcsConnection`].
    pub enum VcsProvider {
        /// A GitHub App installation.
        Github => "github",
        /// A GitLab account (OAuth or an access token).
        Gitlab => "gitlab",
    }
}

/// `vcs_connection` — how a workspace reaches one git host. The token and
/// webhook secret are workspace-scoped [`Secret`]s; only their names are
/// here, and those never leave the kernel either (`serde(skip)`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VcsConnection {
    /// Id.
    pub id: VcsConnectionId,
    /// The workspace it belongs to.
    pub workspace_id: WorkspaceId,
    /// GitHub or GitLab.
    pub provider: VcsProvider,
    /// `https://github.com`, `https://gitlab.com` or a self-hosted GitLab.
    pub instance_url: String,
    /// The org or user it is connected to.
    pub account_login: String,
    /// GitHub: the App installation id.
    pub installation_id: Option<i64>,
    /// GitLab: name of the access token's secret.
    #[serde(skip)]
    pub token_secret: Option<String>,
    /// GitLab OAuth: name of the refresh token's secret.
    #[serde(skip)]
    pub refresh_secret: Option<String>,
    /// GitLab OAuth: when the access token expires.
    #[serde(skip)]
    pub token_expires_at: Option<Timestamp>,
    /// GitLab OAuth: the redirect URI the code was issued for (sent again
    /// with every refresh).
    #[serde(skip)]
    pub oauth_redirect_uri: Option<String>,
    /// GitLab: name of the project hooks' secret.
    #[serde(skip)]
    pub webhook_secret: Option<String>,
    /// The audit actor who connected it.
    pub connected_by: String,
    /// When.
    pub created_at: Timestamp,
}

/// `workspace` — a named workspace. It owns its workflows, projects and
/// tasks (`task.workspace_id`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Workspace {
    /// Id.
    pub id: WorkspaceId,
    /// Display name, unique ignoring case (see [`Workspace::clean_name`]).
    pub name: String,
    /// URL name, unique and never changed (see [`Workspace::clean_slug`]).
    pub slug: String,
    /// What the workspace is for (see [`Workspace::clean_description`]).
    pub description: String,
    /// Standing instructions for every run's agents: the context pack's
    /// "Workspace context" (see [`Workspace::clean_context`]).
    pub context: String,
    /// The sha256 of its logo (see [`Workspace::check_logo`]), which lives
    /// in its own row; `None`: no logo. Versions the logo's URL.
    #[serde(skip)]
    pub logo_hash: Option<String>,
    /// Task keys are `{issue_prefix}-{number}` (see [`Workspace::clean_prefix`]
    /// and [`TaskKey`]); unique in the ecosystem ignoring case.
    pub issue_prefix: String,
    /// The last task number given.
    pub issue_counter: u32,
    /// The workflow new tasks get by default; one of this workspace's.
    pub default_workflow_id: Option<WorkflowId>,
    /// Its git rules (PR mirroring, auto-link, move on merge).
    #[serde(default)]
    pub git_settings: GitSettings,
    /// The member who created it (`None`: Main, or from before workspaces
    /// had members, or a removed member).
    #[serde(default)]
    pub created_by: Option<MemberId>,
    /// When it was created.
    pub created_at: Timestamp,
}

impl Workspace {
    /// Longest workspace name, in characters.
    pub const MAX_NAME: usize = 64;
    /// Longest slug, in characters.
    pub const MAX_SLUG: usize = 48;
    /// Longest description, in characters.
    pub const MAX_DESCRIPTION: usize = 1000;
    /// Longest context, in characters.
    pub const MAX_CONTEXT: usize = 8000;
    /// Longest issue prefix, in characters.
    pub const MAX_PREFIX: usize = 10;
    /// Largest logo, in bytes (1 MB).
    pub const MAX_LOGO: usize = 1024 * 1024;

    /// The trimmed name, or `InvalidInput` unless it has 1–64 characters.
    pub fn clean_name(name: &str) -> crate::Result<String> {
        clean_name(name, Self::MAX_NAME, "workspace")
    }

    /// The slug a workspace named `name` starts from: ASCII letters and
    /// digits lowercased, every other run one `-`, at most 48 characters;
    /// `workspace` when nothing is left. The store makes it unique
    /// ([`Workspace::slug_with_suffix`]). The profile migration backfills
    /// with the same rule.
    pub fn slug_from_name(name: &str) -> String {
        match slugify(name, Self::MAX_SLUG) {
            slug if slug.is_empty() => "workspace".into(),
            slug => slug,
        }
    }

    /// `base-n`, cut so it stays within 48 characters (`n` ≥ 2 de-duplicates).
    pub fn slug_with_suffix(base: &str, n: u32) -> String {
        let suffix = format!("-{n}");
        let keep = Self::MAX_SLUG.saturating_sub(suffix.len()).min(base.len());
        format!("{}{suffix}", base[..keep].trim_end_matches('-'))
    }

    /// The slug as given, or `InvalidInput` unless it is 1–48 characters of
    /// `a-z 0-9` in `-`-separated words (`^[a-z0-9]+(-[a-z0-9]+)*$`).
    pub fn clean_slug(slug: &str) -> crate::Result<String> {
        let slug = slug.trim();
        let valid = (1..=Self::MAX_SLUG).contains(&slug.len())
            && slug.split('-').all(|word| {
                !word.is_empty() && word.bytes().all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9'))
            });
        if valid {
            Ok(slug.to_owned())
        } else {
            Err(crate::Error::InvalidInput(format!(
                "A slug is 1 to {} lowercase letters or digits, in words joined by single `-`.",
                Self::MAX_SLUG
            )))
        }
    }

    /// The default issue prefix: the slug's first four letters or digits,
    /// uppercased (`main` → `MAIN`).
    pub fn prefix_from_slug(slug: &str) -> String {
        slug.chars()
            .filter(char::is_ascii_alphanumeric)
            .take(4)
            .collect::<String>()
            .to_ascii_uppercase()
    }

    /// `base` with the digits of `n` appended, cut so it stays within 10
    /// characters (`n` ≥ 2 de-duplicates: `MAIN` → `MAIN2`). The task
    /// migration de-duplicates stored prefixes with the same rule.
    pub fn prefix_with_suffix(base: &str, n: u32) -> String {
        let suffix = n.to_string();
        let keep = Self::MAX_PREFIX
            .saturating_sub(suffix.len())
            .min(base.len());
        format!("{}{suffix}", &base[..keep])
    }

    /// The prefix trimmed and uppercased, or `InvalidInput` unless it is
    /// 1–10 of `A-Z 0-9`.
    pub fn clean_prefix(prefix: &str) -> crate::Result<String> {
        let prefix = prefix.trim().to_ascii_uppercase();
        let valid = (1..=Self::MAX_PREFIX).contains(&prefix.len())
            && prefix.bytes().all(|b| b.is_ascii_alphanumeric());
        if valid {
            Ok(prefix)
        } else {
            Err(crate::Error::InvalidInput(format!(
                "An issue prefix is 1 to {} letters or digits.",
                Self::MAX_PREFIX
            )))
        }
    }

    /// The trimmed description, or `InvalidInput` past 1000 characters.
    pub fn clean_description(text: &str) -> crate::Result<String> {
        clean_text(text, Self::MAX_DESCRIPTION, "description")
    }

    /// The trimmed context, or `InvalidInput` past 8000 characters.
    pub fn clean_context(text: &str) -> crate::Result<String> {
        clean_text(text, Self::MAX_CONTEXT, "agent context")
    }

    /// The logo's media type when `bytes` are a PNG, JPEG or WebP image of at
    /// most 1 MB whose magic bytes match `content_type`; `InvalidInput` otherwise.
    pub fn check_logo(bytes: &[u8], content_type: &str) -> crate::Result<&'static str> {
        if bytes.len() > Self::MAX_LOGO {
            return Err(crate::Error::InvalidInput(
                "The logo is larger than 1 MB.".into(),
            ));
        }
        let declared = content_type.split(';').next().unwrap_or_default().trim();
        match Self::logo_type(bytes) {
            Some(kind) if kind.eq_ignore_ascii_case(declared) => Ok(kind),
            _ => Err(crate::Error::InvalidInput(
                "The logo must be a PNG, JPEG or WebP image.".into(),
            )),
        }
    }

    /// The media type of a PNG, JPEG or WebP image, read from its magic bytes.
    pub fn logo_type(bytes: &[u8]) -> Option<&'static str> {
        if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
            Some("image/png")
        } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
            Some("image/jpeg")
        } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
            Some("image/webp")
        } else {
            None
        }
    }
}

/// `project` — a named group of one workspace's tasks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Project {
    /// Id.
    pub id: ProjectId,
    /// The workspace it belongs to.
    pub workspace_id: WorkspaceId,
    /// Name, unique in its workspace ignoring case (see [`Project::clean_name`]).
    pub name: String,
    /// What it is about (see [`Project::clean_description`]).
    pub description: String,
    /// When it was created.
    pub created_at: Timestamp,
}

impl Project {
    /// Longest project name, in characters.
    pub const MAX_NAME: usize = 64;
    /// Longest description, in characters.
    pub const MAX_DESCRIPTION: usize = 1000;

    /// The trimmed name, or `InvalidInput` unless it has 1–64 characters.
    pub fn clean_name(name: &str) -> crate::Result<String> {
        clean_name(name, Self::MAX_NAME, "project")
    }

    /// The trimmed description, or `InvalidInput` past 1000 characters.
    pub fn clean_description(text: &str) -> crate::Result<String> {
        clean_text(text, Self::MAX_DESCRIPTION, "description")
    }
}

/// `text` as a slug: ASCII letters and digits lowercased, every other run
/// one `-`, cut to `max` characters, no `-` at either end; may be empty.
fn slugify(text: &str, max: usize) -> String {
    let mut slug = String::new();
    for c in text.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
    }
    slug.truncate(max);
    slug.trim_end_matches('-').to_owned()
}

/// The trimmed name, or `InvalidInput` unless it has 1 to `max` characters.
fn clean_name(name: &str, max: usize, what: &str) -> crate::Result<String> {
    let name = name.trim();
    if (1..=max).contains(&name.chars().count()) {
        Ok(name.to_owned())
    } else {
        Err(crate::Error::InvalidInput(format!(
            "Give the {what} a name of 1 to {max} characters."
        )))
    }
}

/// `text` trimmed, or `InvalidInput` when it has more than `max` characters.
fn clean_text(text: &str, max: usize, what: &str) -> crate::Result<String> {
    let text = text.trim();
    if text.chars().count() <= max {
        Ok(text.to_owned())
    } else {
        Err(crate::Error::InvalidInput(format!(
            "Keep the {what} to {max} characters."
        )))
    }
}

/// `workflow` — the stable identity; content lives in [`WorkflowVersion`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Workflow {
    /// Id.
    pub id: WorkflowId,
    /// The workspace it belongs to; its tasks are on this workspace too.
    pub workspace_id: WorkspaceId,
    /// Name from the YAML `name:` field.
    pub name: String,
}

/// `workflow_version` — saving a workflow creates a new version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowVersion {
    /// The workflow.
    pub workflow_id: WorkflowId,
    /// Version number, starting at 1.
    pub version: u32,
    /// The YAML source as saved.
    pub source_yaml: String,
    /// Hash of the JSON Schema the source was validated against.
    pub schema_hash: String,
}

/// `task`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Task {
    /// Id.
    pub id: TaskId,
    /// The workspace it is on.
    pub workspace_id: WorkspaceId,
    /// Its number on the workspace, given by the store on insert (see [`TaskKey`]).
    pub number: u32,
    /// Title.
    pub title: String,
    /// Description.
    pub body: String,
    /// Workflow to run; `None`: a plain task that never runs.
    pub workflow_id: Option<WorkflowId>,
    /// The queue admits higher [`Priority::rank`] first.
    pub priority: Priority,
    /// Status (its Tasks column).
    pub status: TaskStatus,
    /// A person turned off "move when PRs merge" for this task alone.
    #[serde(default)]
    pub pr_auto_complete_disabled: bool,
    /// The agent assigned to it; it then runs the agent's hidden system
    /// workflow on its workspace (`workflow_id`).
    pub assignee_agent_id: Option<AgentId>,
    /// The project it belongs to, on the same workspace.
    pub project_id: Option<ProjectId>,
    /// Manual order inside its column, ascending (new tasks: their number).
    pub position: f64,
    /// Who created it; `None`: the engine (a trigger, an eval).
    pub created_by: Option<MemberId>,
    /// When it was created.
    pub created_at: Timestamp,
    /// When a field or its status last changed.
    pub updated_at: Timestamp,
}

impl Task {
    /// Longest title, in characters.
    pub const MAX_TITLE: usize = 200;

    /// A `todo` task on `workspace` titled `title`, with no workflow, no
    /// priority and everything else empty; the store gives its number.
    /// Created now: the time in its new ULID.
    pub fn new(workspace_id: WorkspaceId, title: impl Into<String>) -> Self {
        let id = TaskId::generate();
        let now = i64::try_from(id.0.timestamp_ms())
            .ok()
            .and_then(Timestamp::from_timestamp_millis)
            .unwrap_or_default();
        Self {
            id,
            workspace_id,
            number: 0,
            title: title.into(),
            body: String::new(),
            workflow_id: None,
            priority: Priority::None,
            status: TaskStatus::Todo,
            pr_auto_complete_disabled: false,
            assignee_agent_id: None,
            project_id: None,
            position: 0.0,
            created_by: None,
            created_at: now,
            updated_at: now,
        }
    }

    /// The trimmed title, or `InvalidInput` unless it has 1–200 characters.
    pub fn clean_title(title: &str) -> crate::Result<String> {
        let title = title.trim();
        if (1..=Self::MAX_TITLE).contains(&title.chars().count()) {
            Ok(title.to_owned())
        } else {
            Err(crate::Error::InvalidInput(format!(
                "Give the task a title of 1 to {} characters.",
                Self::MAX_TITLE
            )))
        }
    }

    /// True when it runs a workflow or an agent (the engine moves it after its run).
    pub fn runs_something(&self) -> bool {
        self.workflow_id.is_some() || self.assignee_agent_id.is_some()
    }

    /// Who works on it: its agent, else its workflow; `None` for a plain task.
    pub fn assignee(&self) -> Option<Assignee> {
        match (self.assignee_agent_id, self.workflow_id) {
            (Some(id), _) => Some(Assignee::Agent { id }),
            (None, Some(id)) => Some(Assignee::Workflow { id }),
            (None, None) => None,
        }
    }
}

/// Who works on a task: wire shape `{kind: "agent" | "workflow", id}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Assignee {
    /// An agent: the task runs its system workflow on the task's workspace.
    Agent {
        /// The agent.
        id: AgentId,
    },
    /// A workflow on the task's workspace.
    Workflow {
        /// The workflow.
        id: WorkflowId,
    },
}

/// `agent` — one workspace's agent. A task there assigned to it runs its
/// hidden system workflow on that workspace: one `agent` step with its
/// runtime chain (names of the workspace's runtimes), instructions and skills.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Agent {
    /// Id.
    pub id: AgentId,
    /// The workspace it belongs to.
    pub workspace_id: WorkspaceId,
    /// Name, unique among the workspace's active agents ignoring case
    /// (1–64 characters).
    pub name: String,
    /// What it is for (at most 1000 characters).
    pub description: String,
    /// Its prompt (at most 20 000 characters).
    pub instructions: String,
    /// Runtime names in failover order (a workflow's `runtime.chain`); non-empty.
    pub runtime_chain: Vec<String>,
    /// Skill names (catalog ids) installed into its sessions.
    pub skill_ids: Vec<String>,
    /// When it was created.
    pub created_at: Timestamp,
    /// When it was archived (`DELETE`); archived agents take no new tasks.
    pub archived_at: Option<Timestamp>,
}

impl Agent {
    /// Longest name, in characters.
    pub const MAX_NAME: usize = 64;
    /// Longest description, in characters.
    pub const MAX_DESCRIPTION: usize = 1000;
    /// Longest instructions, in characters.
    pub const MAX_INSTRUCTIONS: usize = 20_000;
    /// Most runtimes in a chain, and most skills.
    pub const MAX_LIST: usize = 32;

    /// The trimmed name, or `InvalidInput` unless it has 1–64 characters.
    pub fn clean_name(name: &str) -> crate::Result<String> {
        clean_name(name, Self::MAX_NAME, "agent")
    }

    /// The trimmed description, or `InvalidInput` past 1000 characters.
    pub fn clean_description(text: &str) -> crate::Result<String> {
        clean_text(text, Self::MAX_DESCRIPTION, "description")
    }

    /// The trimmed instructions, or `InvalidInput` past 20 000 characters.
    pub fn clean_instructions(text: &str) -> crate::Result<String> {
        clean_text(text, Self::MAX_INSTRUCTIONS, "instructions")
    }

    /// The runtime chain, or `InvalidInput` when it is empty or too long
    /// (names are checked by the workflow parser).
    pub fn clean_chain(chain: Vec<String>) -> crate::Result<Vec<String>> {
        if chain.is_empty() || chain.len() > Self::MAX_LIST {
            return Err(crate::Error::InvalidInput(format!(
                "Pick 1 to {} runtimes for the agent.",
                Self::MAX_LIST
            )));
        }
        Ok(chain)
    }

    /// The skills, or `InvalidInput` when there are too many.
    pub fn clean_skills(skills: Vec<String>) -> crate::Result<Vec<String>> {
        if skills.len() > Self::MAX_LIST {
            return Err(crate::Error::InvalidInput(format!(
                "Pick at most {} skills for the agent.",
                Self::MAX_LIST
            )));
        }
        Ok(skills)
    }
}

string_enum! {
    /// How urgent a task is. The queue admits by [`Priority::rank`], then FIFO.
    pub enum Priority {
        /// Drop everything.
        Urgent => "urgent",
        /// Soon.
        High => "high",
        /// Normal importance, said out loud.
        Medium => "medium",
        /// When there is time.
        Low => "low",
        /// Not set.
        None => "none",
    }
}

impl Priority {
    /// Queue order, higher first: urgent, high, medium, low, then no priority.
    pub fn rank(self) -> u8 {
        match self {
            Priority::Urgent => 4,
            Priority::High => 3,
            Priority::Medium => 2,
            Priority::Low => 1,
            Priority::None => 0,
        }
    }

    /// The old integer priority (and `POST /tasks/{id}/bump`): above 0 is
    /// high, below 0 low, 0 none. The task migration maps stored ones the same way.
    pub fn from_int(priority: i32) -> Self {
        match priority {
            p if p > 0 => Priority::High,
            p if p < 0 => Priority::Low,
            _ => Priority::None,
        }
    }

    /// A triage level, 0 (no opinion) to 3 (urgent).
    pub fn from_level(level: i32) -> Self {
        match level {
            l if l >= 3 => Priority::Urgent,
            2 => Priority::High,
            1 => Priority::Medium,
            _ => Priority::None,
        }
    }
}

/// A task's key, `{issue_prefix}-{number}` (e.g. `ENG-12`): the one place
/// keys are written and read. Keys are computed, never stored, so changing a
/// workspace's prefix renames every key. Prefixes are unique in the
/// ecosystem, so a key alone finds its task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskKey {
    /// The workspace's issue prefix, uppercase.
    pub prefix: String,
    /// The task's number.
    pub number: u32,
}

impl std::fmt::Display for TaskKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}-{}", self.prefix, self.number)
    }
}

impl std::str::FromStr for TaskKey {
    type Err = crate::Error;

    /// `eng-12` or `ENG-12`; the prefix comes back uppercase.
    fn from_str(text: &str) -> crate::Result<Self> {
        let bad = || crate::Error::InvalidInput(format!("`{text}` is not a task key"));
        let (prefix, number) = text.trim().rsplit_once('-').ok_or_else(bad)?;
        let prefix = Workspace::clean_prefix(prefix).map_err(|_| bad())?;
        let digits = !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit());
        let number: u32 = number
            .parse()
            .ok()
            .filter(|n| digits && *n > 0)
            .ok_or_else(bad)?;
        Ok(Self { prefix, number })
    }
}

impl TaskKey {
    /// Every task branch starts with this.
    pub const BRANCH_PREFIX: &str = "sun/";
    /// Longest title slug in a branch name, in characters.
    pub const MAX_BRANCH_SLUG: usize = 40;

    /// The task's git branch, `sun/{KEY}-{slug}` (`sun/SUN-12-fix-login`):
    /// the title lowercased, every run of other characters one `-`, cut to
    /// 40 characters. Just `sun/{KEY}` when nothing of the title is left.
    pub fn branch(&self, title: &str) -> String {
        match slugify(title, Self::MAX_BRANCH_SLUG) {
            slug if slug.is_empty() => format!("{}{self}", Self::BRANCH_PREFIX),
            slug => format!("{}{self}-{slug}", Self::BRANCH_PREFIX),
        }
    }

    /// The key a [`TaskKey::branch`] names; `None` for any other branch
    /// (`sun/skill-tdd`, `sun/01J…`, `main`).
    pub fn from_branch(branch: &str) -> Option<Self> {
        let rest = branch.strip_prefix(Self::BRANCH_PREFIX)?;
        let (prefix, rest) = rest.split_once('-')?;
        let number = rest.split('-').next()?;
        format!("{prefix}-{number}").parse().ok()
    }
}

string_enum! {
    /// What a [`TaskActivity`] entry records.
    pub enum ActivityKind {
        /// A person's comment `{text}`.
        Comment => "comment",
        /// A status move `{from, to}`.
        Status => "status",
        /// `{from, to}` priorities.
        Priority => "priority",
        /// `{from, to}` assignees.
        Assignee => "assignee",
        /// `{from, to}` project ids.
        Project => "project",
        /// `{from, to}` titles.
        Title => "title",
        /// The description changed (`{}`; the text is the task's).
        Description => "description",
        /// The task was created `{status}`.
        Created => "created",
        /// A run of the task `{run_id}` was queued.
        Run => "run",
        /// A pull request was linked, unlinked or merged:
        /// `{action: linked|unlinked|merged, number, url, repo, source}`.
        PullRequest => "pull_request",
    }
}

/// `task_activity` — the append-only log of a task's comments and changes,
/// tasks without a run included. `seq` counts per task from 1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskActivity {
    /// Id.
    pub id: TaskActivityId,
    /// The task.
    pub task_id: TaskId,
    /// Position in the task's log, from 1, no gaps.
    pub seq: i64,
    /// What happened.
    pub kind: ActivityKind,
    /// Who did it; `None`: the engine.
    pub actor_member_id: Option<MemberId>,
    /// The details, shaped by `kind`.
    pub body: Value,
    /// When.
    pub created_at: Timestamp,
}

impl TaskActivity {
    /// Longest comment, in characters.
    pub const MAX_COMMENT: usize = 10_000;

    /// The trimmed comment, or `InvalidInput` unless it has 1–10 000 characters.
    pub fn clean_comment(text: &str) -> crate::Result<String> {
        let text = text.trim();
        if (1..=Self::MAX_COMMENT).contains(&text.chars().count()) {
            Ok(text.to_owned())
        } else {
            Err(crate::Error::InvalidInput(format!(
                "Write a comment of 1 to {} characters.",
                Self::MAX_COMMENT
            )))
        }
    }
}

/// Run budget (`run.budget_json`). `None` means unlimited.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Budget {
    /// Wall-clock limit in seconds.
    pub wall_secs: Option<u64>,
    /// Agent turn limit.
    pub turns: Option<u32>,
    /// Cost limit in USD.
    pub cost_usd: Option<f64>,
}

/// `run` — one execution of a task, pinned to a workflow version.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Run {
    /// Id.
    pub id: RunId,
    /// The task.
    pub task_id: TaskId,
    /// The workflow.
    pub workflow_id: WorkflowId,
    /// The pinned workflow version.
    pub workflow_version: u32,
    /// Status.
    pub status: RunStatus,
    /// Why the run ended; set with a terminal status.
    pub end_reason: Option<String>,
    /// Limits.
    pub budget: Budget,
    /// The workspace repos the run works in, resolved from the task and
    /// workflow text when it was queued; empty for a plain folder.
    #[serde(default)]
    pub repo_ids: Vec<RepoId>,
}

/// `step` — one row per attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Step {
    /// Id of this attempt.
    pub id: StepId,
    /// The run.
    pub run_id: RunId,
    /// Step id from the workflow.
    pub step_key: StepKey,
    /// Attempt number, starting at 1.
    pub attempt: u32,
    /// Status.
    pub status: StepStatus,
    /// Runtime used, for agent steps.
    pub runtime_id: Option<RuntimeId>,
    /// Session used, for agent steps.
    pub session_id: Option<SessionId>,
    /// When it started running.
    pub started_at: Option<Timestamp>,
    /// When it reached a terminal status.
    pub ended_at: Option<Timestamp>,
}

/// `event` — the ledger. Append-only, ordered by `seq`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventRecord {
    /// Gapless, ascending sequence number assigned by the store.
    pub seq: i64,
    /// The run.
    pub run_id: RunId,
    /// The step, for step-level events.
    pub step_key: Option<StepKey>,
    /// Event kind, e.g. `tool_call` or `step_status`.
    pub kind: String,
    /// Event payload (inline).
    pub payload: Value,
    /// Payload too big to inline.
    pub blob: Option<BlobHash>,
    /// When it happened.
    pub at: Timestamp,
}

string_enum! {
    /// Delivery state of an [`OutboxItem`].
    pub enum OutboxStatus {
        /// Not delivered yet.
        Pending => "pending",
        /// Delivered; `result` holds the reply.
        Delivered => "delivered",
        /// Delivery timed out; waits for a human or a plugin `reconcile`.
        Unknown => "unknown",
        /// Delivery failed definitively.
        Failed => "failed",
    }
}

/// `outbox` — a side effect, delivered after commit (**Transactional outbox**).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutboxItem {
    /// Id.
    pub id: OutboxId,
    /// The run; `None` for a side effect outside any run (a failed
    /// trigger's notification).
    pub run_id: Option<RunId>,
    /// Unique idempotency key.
    pub idem_key: IdemKey,
    /// Target action, e.g. `github.open_pr`.
    pub target: String,
    /// Action input.
    pub payload: Value,
    /// Delivery state.
    pub status: OutboxStatus,
    /// Action result, once delivered.
    pub result: Option<Value>,
}

string_enum! {
    /// Who pays for a runtime (`runtime.source`).
    pub enum RuntimeSource {
        /// The user's own CLI subscription login.
        ByoRuntime => "byo_runtime",
        /// The user's own API key.
        ByoKey => "byo_key",
        /// Sun credits via the gateway.
        Credits => "credits",
    }
}

/// `runtime` — a configured runtime of one workspace, which `runtime.chain`
/// entries of that workspace's workflows and agents name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Runtime {
    /// Id.
    pub id: RuntimeId,
    /// The workspace whose chains it serves.
    pub workspace_id: WorkspaceId,
    /// Name used in workflow chains, unique in its workspace, e.g.
    /// `opencode-openrouter`.
    pub name: String,
    /// Adapter id (`RuntimeAdapter::id`), e.g. `opencode`.
    pub adapter: String,
    /// Who pays.
    pub source: RuntimeSource,
    /// The endpoint, model and key secret when `source` is `byo_key`
    /// (`runtime.config_json`).
    #[serde(default)]
    pub byo_key: Option<crate::ByoKey>,
    /// Who added it; `None` for one a chain or `sun.yaml` named first, or
    /// one from before runtimes were per workspace.
    #[serde(default)]
    pub created_by: Option<crate::MemberId>,
}

impl Runtime {
    /// Checks a runtime before it is saved: a slug name, and for `byo_key`
    /// a valid [`crate::ByoKey`] (and only then) whose kind the adapter runs
    /// ([`crate::ByoKeyKind::runs_on`]).
    /// Like [`crate::ByoKey::validate`], the message starts with the field
    /// (`name: `, `adapter: `, `source: `, ...).
    pub fn validate(&self) -> crate::Result<()> {
        let invalid = |msg: &str| Err(crate::Error::InvalidInput(msg.into()));
        let slug = (1..=64).contains(&self.name.len())
            && self
                .name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
        if !slug {
            return invalid("name: 1–64 lowercase letters, digits, - or _, e.g. ollama-qwen");
        }
        match (self.source, &self.byo_key) {
            (RuntimeSource::ByoKey, Some(byo)) if !byo.kind.runs_on(&self.adapter) => invalid(
                "adapter: opencode takes any key; claude-code takes an anthropic key, \
                     codex an openai_compatible one",
            ),
            (RuntimeSource::ByoKey, Some(byo)) => byo.validate(),
            (RuntimeSource::ByoKey, None) => invalid("endpoint: no endpoint"),
            (RuntimeSource::Credits, _)
                if !matches!(self.adapter.as_str(), "claude-code" | "codex" | "opencode") =>
            {
                invalid("adapter: Sun credits run on claude-code, codex or opencode")
            }
            (_, Some(_)) => invalid("source: only a byo_key runtime takes an endpoint"),
            (_, None) => Ok(()),
        }
    }
}

string_enum! {
    /// Availability of a runtime (`runtime_state.status`).
    pub enum RuntimeStatus {
        /// Can take work.
        Ready => "ready",
        /// Hit a limit; usable again after `cooldown_until`.
        CoolingDown => "cooling_down",
        /// Not usable (not installed, logged out, disabled).
        Unavailable => "unavailable",
    }
}

/// `runtime_state`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeState {
    /// The runtime.
    pub runtime_id: RuntimeId,
    /// Availability.
    pub status: RuntimeStatus,
    /// End of the current cooldown.
    pub cooldown_until: Option<Timestamp>,
}

/// `host` — a machine running the host daemon.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Host {
    /// Id.
    pub id: HostId,
    /// Display name.
    pub name: String,
    /// Reported capabilities (installed CLIs, simulators, KVM, ...).
    pub capabilities: Value,
    /// Who minted the enrolment token it joined with (`None`: before hosts
    /// were per workspace, the kernel's own host, or a leased seller host).
    /// They may attach it to their other workspaces.
    #[serde(default)]
    pub enrolled_by: Option<MemberId>,
}

/// `session` — an agent session on a host, with its resource use.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Session {
    /// Id.
    pub id: SessionId,
    /// The host.
    pub host_id: HostId,
    /// The session's worktree on the host.
    pub worktree: PathBuf,
    /// OS process id.
    pub pid: Option<u32>,
    /// Resident memory in bytes.
    pub rss_bytes: Option<u64>,
    /// CPU use in percent of one core.
    pub cpu_percent: Option<f32>,
}

/// `handoff_pack` — context moved from one session to the next. The pack is a blob.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandoffPack {
    /// Id.
    pub id: HandoffPackId,
    /// The run.
    pub run_id: RunId,
    /// Session handing off.
    pub from_session: SessionId,
    /// Session receiving, once started.
    pub to_session: Option<SessionId>,
    /// The pack.
    pub pack_hash: BlobHash,
}

/// `context_pack` — exactly what an agent step was given, reproducible.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextPack {
    /// Id.
    pub id: ContextPackId,
    /// The step attempt.
    pub step_id: StepId,
    /// The pack.
    pub pack_hash: BlobHash,
    /// Estimated tokens.
    pub token_estimate: u32,
    /// What went in (shape owned by `sun-context`).
    pub items: Value,
}

string_enum! {
    /// Kind of [`Note`].
    pub enum NoteKind {
        /// A decision taken.
        Decision => "decision",
        /// Work left to do.
        Todo => "todo",
        /// Something learned.
        Learning => "learning",
        /// An open question.
        Question => "question",
        /// A progress update.
        Progress => "progress",
    }
}

/// `note` — written by agents via `sun.note`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Note {
    /// Id.
    pub id: NoteId,
    /// The run.
    pub run_id: RunId,
    /// The step.
    pub step_key: StepKey,
    /// Kind.
    pub kind: NoteKind,
    /// Text.
    pub body: String,
    /// A later note that replaces this one.
    pub superseded_by: Option<NoteId>,
}

string_enum! {
    /// Scope of a [`MemoryItem`] within its workspace.
    pub enum MemoryScope {
        /// The whole workspace (the key predates workspaces).
        Ecosystem => "ecosystem",
        /// One repo.
        Repo => "repo",
        /// One workflow.
        Workflow => "workflow",
    }
}

string_enum! {
    /// Review state of a [`MemoryItem`].
    pub enum MemoryStatus {
        /// Suggested, awaiting review.
        Proposed => "proposed",
        /// Approved and in use.
        Active => "active",
        /// No longer used.
        Retired => "retired",
    }
}

impl MemoryStatus {
    /// The review moves a human may make: approve or reject a proposal
    /// (`proposed → active | retired`), retire an active item. Nothing
    /// becomes active without one.
    pub fn can_move(self, to: MemoryStatus) -> bool {
        use MemoryStatus::*;
        matches!((self, to), (Proposed, Active | Retired) | (Active, Retired))
    }
}

/// `memory_item` — long-lived memory (L3); changes go through review.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryItem {
    /// Id.
    pub id: MemoryItemId,
    /// The workspace it was learned in; only that workspace's runs get it.
    pub workspace_id: WorkspaceId,
    /// Scope.
    pub scope: MemoryScope,
    /// The repo id (`repo` scope) or workflow id (`workflow` scope); `None`
    /// for the whole workspace.
    pub scope_id: Option<String>,
    /// The instruction, one line, as it goes into packs and AGENTS.md.
    pub body: String,
    /// The repo file a PR adds the line to: `AGENTS.md`,
    /// `CLAUDE.md`, a skill's `SKILL.md` or a workflow prompt under `.sun/`.
    /// Set when proposed and settled when the PR is pushed; `None`: the
    /// repo's agent instructions file.
    #[serde(default)]
    pub target: Option<String>,
    /// Why it was proposed, e.g. `test_failure corrected 3 times in fix`.
    pub reason: String,
    /// Run that proposed it.
    pub source_run: Option<RunId>,
    /// Review state.
    pub status: MemoryStatus,
    /// Who proposed it: `sun` (a repeated correction) or the person who
    /// starred the source.
    pub author: String,
    /// Reviewer.
    pub reviewed_by: Option<String>,
    /// When it was proposed.
    pub created_at: Timestamp,
}

string_enum! {
    /// What a [`Correction`] was captured from.
    pub enum CorrectionKind {
        /// A gate failed the agent's work.
        GateFail => "gate_fail",
        /// A human rejected an approval (or an escalated decision).
        Reject => "reject",
        /// A commit on the task branch was reverted.
        Revert => "revert",
        /// A human steered a running agent against what it was doing.
        Steer => "steer",
        /// A human answered an escalated `decide` step differently.
        DecisionOverride => "decision_override",
        /// A reviewer commented on Sun's PR without approving it.
        ReviewComment => "review_comment",
    }
}

string_enum! {
    /// Root cause of a [`Correction`], the label recurring mistakes group by.
    pub enum CorrectionCause {
        /// Tests or CI failed.
        TestFailure => "test_failure",
        /// Lint, type or diagnostics errors.
        Lint => "lint",
        /// Something asked for is missing.
        MissingRequirement => "missing_requirement",
        /// Changed more (or other things) than asked.
        WrongScope => "wrong_scope",
        /// Naming, formatting, wording.
        Style => "style",
        /// None of the above.
        Other => "other",
    }
}

/// `correction` — a human correction of agent work, linked to the
/// run, its workflow and the runtime whose work was corrected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Correction {
    /// Id.
    pub id: CorrectionId,
    /// The task.
    pub task_id: TaskId,
    /// The run.
    pub run_id: RunId,
    /// The run's workflow.
    pub workflow_id: WorkflowId,
    /// The step corrected (`None`: the run as a whole, e.g. a revert).
    pub step_key: Option<StepKey>,
    /// Name of the runtime of the run's latest agent attempt, if any ran.
    pub runtime: Option<String>,
    /// What it was captured from.
    pub kind: CorrectionKind,
    /// Root-cause label.
    pub cause_label: CorrectionCause,
    /// Who set the label: `rules`, or a classifier (model) name.
    pub classified_by: String,
    /// What was wrong (redacted).
    pub body: String,
    /// When it was captured.
    pub created_at: Timestamp,
}

/// `artifact` — a versioned output of a task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Artifact {
    /// Id.
    pub id: ArtifactId,
    /// The task.
    pub task_id: TaskId,
    /// File name in the task's worktree, e.g. `brief.md`.
    pub name: String,
    /// Version, starting at 1 (numbered per task).
    pub version: u32,
    /// Content.
    pub blob: BlobHash,
    /// MIME type.
    pub mime: String,
}

string_enum! {
    /// Where a design-canvas [`Pin`] stands.
    pub enum PinStatus {
        /// Pinned; nothing asked yet.
        Open => "open",
        /// Has a request batched for "send all" (the pin queue).
        Queued => "queued",
        /// Sent to the agent; waits for its change to land.
        InProgress => "in_progress",
        /// The change landed (or the user closed it).
        Done => "done",
    }
}

/// The element a pin points at, as the preview bridge reported it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PinTarget {
    /// A CSS selector unique in the page when picked.
    pub selector: String,
    /// Lower-case tag name.
    pub tag: String,
    /// The element's own text (trimmed, may be empty).
    pub text: String,
    /// Breakpoint (page CSS width) it was picked at.
    pub width: u32,
    /// Bounding box in page CSS pixels at pick time.
    pub rect: PinRect,
    /// Computed styles the canvas shows and checks (`property → value`).
    pub styles: std::collections::BTreeMap<String, String>,
    /// Source location from dev metadata or `data-*` attributes, when known.
    pub source: Option<SourceLocation>,
    /// The page's path and query under the preview base, e.g. `/pricing?x=1`
    /// (empty = `/`); the screenshot crops load it.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub path: String,
}

/// A bounding box in CSS pixels.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PinRect {
    /// Left.
    pub x: f64,
    /// Top.
    pub y: f64,
    /// Width.
    pub width: f64,
    /// Height.
    pub height: f64,
}

/// `file:line:column` in the task's repo.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceLocation {
    /// Path as the framework reports it.
    pub file: String,
    /// 1-based line.
    pub line: u32,
    /// 1-based column, 0 when unknown.
    pub column: u32,
}

/// One value the user changed on the canvas: a suggested diff.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PinEdit {
    /// `text` or a CSS property.
    pub property: String,
    /// Value before.
    pub from: String,
    /// Value the user set.
    pub to: String,
}

/// `pin` — a design-canvas pin on a task.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Pin {
    /// Id.
    pub id: PinId,
    /// The task.
    pub task_id: TaskId,
    /// Status.
    pub status: PinStatus,
    /// What the user wants changed (for "Ask AI").
    pub comment: String,
    /// The element.
    pub target: PinTarget,
    /// Values the user changed themselves.
    pub edits: Vec<PinEdit>,
    /// PNG crop of the element when it was pinned (the "before"), once the
    /// host took it.
    #[serde(default)]
    pub before_crop: Option<crate::BlobHash>,
    /// PNG crop of the same box once the pin is done.
    #[serde(default)]
    pub after_crop: Option<crate::BlobHash>,
    /// Created.
    pub created_at: Timestamp,
    /// Last change.
    pub updated_at: Timestamp,
}

/// `task_quality` — how a task's merged PR held up afterwards.
/// Created when the git host reports the merge; each quality check refreshes
/// the signals.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskQuality {
    /// The task.
    pub task_id: TaskId,
    /// The git host mod that reported the merge, e.g. `github`.
    pub mod_name: String,
    /// `owner/name` on the git host.
    pub repo: String,
    /// PR / MR number.
    pub pr_number: Option<i64>,
    /// Branch the PR merged into.
    pub base: String,
    /// The merge (or squash) commit on `base`.
    pub merge_sha: String,
    /// When it merged.
    pub merged_at: Timestamp,
    /// Days after the merge in which fix commits count against it.
    pub window_days: u32,
    /// CI of the merge commit on `base`: `success`, `failure`, `pending` or
    /// `none`; `None` before the first check.
    pub ci_state: Option<String>,
    /// The commit that reverted the merge, if any.
    pub reverted_by: Option<String>,
    /// Fix commits on `base` touching the PR's files within the window.
    pub fix_commits: Vec<FixCommit>,
    /// Bug issues linked to the merged PR: the one that started the task,
    /// those its text references, and regressions reported after the merge.
    #[serde(default)]
    pub bug_issues: Vec<BugIssue>,
    /// Last check.
    pub checked_at: Option<Timestamp>,
}

impl TaskQuality {
    /// `Some(true)` once checked with green CI, no revert and no fix
    /// commits; `Some(false)` on any of those; `None` while unchecked or CI
    /// is still pending.
    pub fn held_up(&self) -> Option<bool> {
        if self.ci_state.as_deref() == Some("failure")
            || self.reverted_by.is_some()
            || !self.fix_commits.is_empty()
            || self.regressions().next().is_some()
        {
            return Some(false);
        }
        match self.ci_state.as_deref() {
            Some("success" | "none") => Some(true),
            _ => None,
        }
    }

    /// Bug issues reported against the merged code after the merge.
    pub fn regressions(&self) -> impl Iterator<Item = &BugIssue> {
        self.bug_issues
            .iter()
            .filter(|b| b.relation == BugRelation::Regression)
    }
}

string_enum! {
    /// How a bug issue relates to a merged task ([`BugIssue::relation`]).
    pub enum BugRelation {
        /// The issue whose trigger started the task.
        Origin => "origin",
        /// An issue the task's title or description links.
        Referenced => "referenced",
        /// A bug issue opened after the merge that names the merged code.
        Regression => "regression",
    }
}

/// A bug issue on a tracker (GitHub, GitLab, Gitea, Jira), linked to a
/// task's merged PR in [`TaskQuality::bug_issues`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BugIssue {
    /// Origin, referenced or regression.
    pub relation: BugRelation,
    /// `owner/repo#12`, or a tracker key like `APP-4`.
    pub key: String,
    /// Issue title, when known.
    pub title: Option<String>,
    /// Link to the issue, when known.
    pub url: Option<String>,
    /// When it was opened (regressions).
    pub opened_at: Option<Timestamp>,
}

/// A follow-up fix commit in [`TaskQuality::fix_commits`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FixCommit {
    /// Commit sha.
    pub sha: String,
    /// First line of the message.
    pub subject: String,
    /// Commit time.
    pub at: Timestamp,
}

string_enum! {
    /// Kind of mod.
    pub enum ModKind {
        /// Integration with an outside service (GitHub, Slack, ...).
        Plugin => "plugin",
        /// Extends the host (runtimes, simulators, VMs, ...).
        Extension => "extension",
    }
}

/// `mod_install` — an installed mod and what it may do.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModInstall {
    /// Unique mod name.
    pub name: String,
    /// Kind.
    pub kind: ModKind,
    /// Installed version.
    pub version: String,
    /// The manifest (shape owned by `sun-mod`).
    pub manifest: Value,
    /// Permissions the user granted.
    pub granted_permissions: Vec<String>,
    /// Hex SHA-256 of the installed package (`sun_mod::package_digest`).
    #[serde(default)]
    pub package_digest: Option<String>,
    /// Hex ed25519 public key that signed the package; `None` = unsigned
    /// private mod.
    #[serde(default)]
    pub signed_by: Option<String>,
}

impl ModInstall {
    /// Whether this (extension) declares plugin `name` in `permissions.plugins`.
    pub fn needs_plugin(&self, name: &str) -> bool {
        let needs = format!("plugin:{name}");
        self.granted_permissions.contains(&needs)
    }
}

/// Who a [`Secret`] belongs to. JSON: `{"kind": "ecosystem", "id": "<ulid>"}`
/// , `{"kind": "mod", "id": "<mod name>"}` or `{"kind": "workspace", "id": "<ulid>"}`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum SecretScope {
    /// Shared by the ecosystem's runs and mods.
    Ecosystem(EcosystemId),
    /// Only for one mod, by [`ModInstall::name`].
    Mod(String),
    /// One workspace's git connection tokens and webhook secrets
    /// ([`VcsConnection`]); managed by the kernel, not set by hand.
    Workspace(WorkspaceId),
}

impl SecretScope {
    /// Rebuilds a scope from its `(kind, id)` columns.
    pub fn from_parts(kind: &str, id: &str) -> crate::Result<Self> {
        match kind {
            "ecosystem" => Ok(Self::Ecosystem(id.parse()?)),
            "mod" if !id.is_empty() => Ok(Self::Mod(id.to_owned())),
            "workspace" => Ok(Self::Workspace(id.parse()?)),
            _ => Err(crate::Error::InvalidInput(format!(
                "secret scope `{kind}:{id}`"
            ))),
        }
    }

    /// `ecosystem`, `mod` or `workspace`.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Ecosystem(_) => "ecosystem",
            Self::Mod(_) => "mod",
            Self::Workspace(_) => "workspace",
        }
    }

    /// The ecosystem id, mod name or workspace id.
    pub fn id(&self) -> String {
        match self {
            Self::Ecosystem(id) => id.to_string(),
            Self::Mod(name) => name.clone(),
            Self::Workspace(id) => id.to_string(),
        }
    }
}

/// `secret` — metadata only. The value is encrypted at rest and is never part
/// of this type, so it cannot leak through `Debug`, JSON or a list endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Secret {
    /// Id.
    pub id: crate::SecretId,
    /// Owner.
    pub scope: SecretScope,
    /// Name, unique within the scope, e.g. `SLACK_BOT_TOKEN`.
    pub name: String,
    /// When the value was last set.
    pub updated_at: Timestamp,
}

impl Secret {
    /// Ecosystem settings kept with the secrets (an app id, a slug, a URL)
    /// that are not secret: their values are never redacted from text or
    /// searched for in diffs (a diff mentioning `https://gitlab.com` must
    /// still push), and may be shorter than [`crate::redact::MIN_SECRET_LEN`].
    pub const PLAIN: &[&str] = &[
        "GITHUB_APP_ID",
        "GITHUB_APP_SLUG",
        "GITHUB_APP_API_URL",
        "GITLAB_OAUTH_URL",
        "GITLAB_OAUTH_CLIENT_ID",
    ];

    /// True for a [`Self::PLAIN`] name.
    pub fn is_plain(name: &str) -> bool {
        Self::PLAIN.contains(&name)
    }

    /// The deployment's own secrets, which stay in the ecosystem scope: the
    /// mail password, the git host apps, the embedding key.
    pub const DEPLOYMENT: &[&str] = &[
        "SMTP_PASSWORD",
        "EMBEDDING_API_KEY",
        "GITHUB_APP_ID",
        "GITHUB_APP_SLUG",
        "GITHUB_APP_PRIVATE_KEY",
        "GITHUB_APP_WEBHOOK_SECRET",
        "GITHUB_APP_API_URL",
        "GITLAB_OAUTH_URL",
        "GITLAB_OAUTH_CLIENT_ID",
        "GITLAB_OAUTH_CLIENT_SECRET",
    ];

    /// Prefix of the workspace secrets the kernel keeps for a git
    /// connection (`VCS_<connection id>_TOKEN`, `_REFRESH`, `_WEBHOOK`):
    /// never listed, set or deleted by hand.
    pub const CONNECTION_PREFIX: &str = "VCS_";

    /// True for a [`Self::CONNECTION_PREFIX`] name.
    pub fn is_connection(name: &str) -> bool {
        name.starts_with(Self::CONNECTION_PREFIX)
    }

    /// True for a deployment secret: a [`Self::DEPLOYMENT`] name or an SSO
    /// provider's client secret (`SSO_<ID>_SECRET`). Every other secret a
    /// person sets belongs to a workspace.
    pub fn is_deployment(name: &str) -> bool {
        Self::DEPLOYMENT.contains(&name)
            || (name.starts_with("SSO_") && name.ends_with("_SECRET") && name.len() > 11)
    }

    /// Checks a secret name: 1–64 chars of `A-Z a-z 0-9 _`, not starting with
    /// a digit, so it is usable as an env var name.
    pub fn validate_name(name: &str) -> crate::Result<()> {
        let valid = (1..=64).contains(&name.len())
            && !name.starts_with(|c: char| c.is_ascii_digit())
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        if valid {
            Ok(())
        } else {
            Err(crate::Error::InvalidInput(format!("secret name `{name}`")))
        }
    }
}

string_enum! {
    /// One capability a [`Role`] grants in a workspace. A workspace member
    /// has their role's permissions there; a deployment owner has every one.
    pub enum Permission {
        /// Create, steer, pause, retry and cancel tasks.
        Run => "run",
        /// Decide approval steps (still subject to the step's `who:`).
        Approve => "approve",
        /// Install plugins (mods of kind `plugin`) and MCP servers.
        ManagePlugins => "manage_plugins",
        /// Install extensions (mods of kind `extension`).
        ManageExtensions => "manage_extensions",
        /// List, set and delete secrets.
        ManageSecrets => "manage_secrets",
        /// Add and change runtimes and their permission modes.
        ManageRuntimes => "manage_runtimes",
        /// Ecosystem settings: SMTP, sign-in, the audit log, hosts, the
        /// license, embeddings, the account, the marketplace, and importing
        /// repos or knowledge into another workspace.
        ManageSettings => "manage_settings",
        /// Invite members by an emailed code.
        InviteMembers => "invite_members",
        /// Add members with a password (no mail).
        CreateMembers => "create_members",
        /// List members, and the chat links.
        ViewMembers => "view_members",
        /// Change a member's role and password, send a reset code, and
        /// link chat accounts.
        EditMembers => "edit_members",
        /// Deactivate and reactivate members.
        DeactivateMembers => "deactivate_members",
        /// Rename workspaces and change their settings.
        EditWorkspace => "edit_workspace",
        /// Delete workspaces.
        DeleteWorkspace => "delete_workspace",
        /// Create workflows (and install templates).
        CreateWorkflow => "create_workflow",
        /// Save new versions of existing workflows.
        EditWorkflow => "edit_workflow",
    }
}

string_enum! {
    /// A member's role in one workspace (`workspace_member.role`). Approval
    /// `who: role:<key>` matches the key. Each role grants a fixed set of
    /// permissions ([`Role::permissions`]); the creator of a workspace is its
    /// owner, and a workspace always keeps at least one owner.
    pub enum Role {
        /// Every permission in the workspace; manages its members and roles.
        Owner => "owner",
        /// Runs work and manages the workspace's members, secrets,
        /// runtimes, plugins and settings.
        Admin => "admin",
        /// Runs tasks, approves and edits workflows.
        Member => "member",
        /// Reads only.
        Viewer => "viewer",
    }
}

/// What [`Role::Member`] may do (the seeded Member matrix).
const MEMBER_PERMISSIONS: &[Permission] = &[
    Permission::Run,
    Permission::Approve,
    Permission::CreateWorkflow,
    Permission::EditWorkflow,
];

/// What [`Role::Admin`] may do (the seeded Admin matrix, without the
/// permissions roles replaced), in [`Permission::ALL`] order.
const ADMIN_PERMISSIONS: &[Permission] = &[
    Permission::Run,
    Permission::Approve,
    Permission::ManagePlugins,
    Permission::ManageExtensions,
    Permission::ManageSecrets,
    Permission::ManageRuntimes,
    Permission::ManageSettings,
    Permission::InviteMembers,
    Permission::CreateMembers,
    Permission::ViewMembers,
    Permission::EditMembers,
    Permission::DeactivateMembers,
    Permission::EditWorkspace,
    Permission::DeleteWorkspace,
    Permission::CreateWorkflow,
    Permission::EditWorkflow,
];

impl Role {
    /// The permissions the role grants in its workspace, in
    /// [`Permission::ALL`] order; an owner has every one.
    pub const fn permissions(self) -> &'static [Permission] {
        match self {
            Role::Owner => Permission::ALL,
            Role::Admin => ADMIN_PERMISSIONS,
            Role::Member => MEMBER_PERMISSIONS,
            Role::Viewer => &[],
        }
    }

    /// True when this role is `other` or above it (owner, then admin,
    /// member, viewer): who may grant `other` (an admin grants admin,
    /// member or viewer; only an owner grants owner).
    pub fn covers(self, other: Role) -> bool {
        let rank = |role: Role| match role {
            Role::Owner => 3,
            Role::Admin => 2,
            Role::Member => 1,
            Role::Viewer => 0,
        };
        rank(self) >= rank(other)
    }
}

/// What someone with `roles` may do: every permission for a deployment
/// owner, else the union of the roles' permissions, in [`Permission::ALL`] order.
pub fn permissions_of(is_owner: bool, roles: &[Role]) -> Vec<Permission> {
    Permission::ALL
        .iter()
        .copied()
        .filter(|p| is_owner || roles.iter().any(|r| r.permissions().contains(p)))
        .collect()
}

/// The role names approval `who: role:<name>` matches for someone with
/// `roles`: each role's key and the keys of the roles below it (an admin
/// also passes `role:member` and `role:viewer`, as when admins were in
/// every lower group), every key for a deployment owner; in ladder order
/// (viewer, member, admin, owner), once each.
pub fn roles_of(is_owner: bool, roles: &[Role]) -> Vec<String> {
    [Role::Viewer, Role::Member, Role::Admin, Role::Owner]
        .into_iter()
        .filter(|key| is_owner || roles.iter().any(|r| r.covers(*key)))
        .map(|key| key.as_str().to_owned())
        .collect()
}

/// `workspace_member` — a member's role in one workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceMember {
    /// The workspace.
    pub workspace_id: WorkspaceId,
    /// The member.
    pub member_id: MemberId,
    /// Their role there.
    pub role: Role,
    /// When they joined it.
    pub created_at: Timestamp,
}

/// `workspace_invitation` — an emailed invitation into one workspace with a
/// role. The link token is stored as `sha256(token)` only (never here).
/// Pending until accepted, declined or past `expires_at`; at most one
/// pending per workspace and email.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceInvitation {
    /// Id.
    pub id: crate::InvitationId,
    /// The workspace.
    pub workspace_id: WorkspaceId,
    /// Who is invited, lowercase.
    pub email: String,
    /// The role they get on accepting.
    pub role: Role,
    /// Who invited them (`None`: the token holder, or a removed member).
    pub inviter_id: Option<MemberId>,
    /// When it was sent.
    pub created_at: Timestamp,
    /// When it stops working.
    pub expires_at: Timestamp,
    /// When they accepted it.
    pub accepted_at: Option<Timestamp>,
    /// When they declined it.
    pub declined_at: Option<Timestamp>,
}

impl WorkspaceInvitation {
    /// Neither accepted nor declined, and not expired at `now`.
    pub fn is_pending(&self, now: Timestamp) -> bool {
        self.accepted_at.is_none() && self.declined_at.is_none() && self.expires_at > now
    }
}

/// Who may sign up and create workspaces on this deployment
/// (`SUN_ALLOW_SIGNUP`, `SUN_ALLOWED_EMAIL_DOMAINS`,
/// `SUN_DISABLE_WORKSPACE_CREATION`). A pending invitation admits an email
/// whatever this says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignupPolicy {
    /// Anyone may sign up (default on).
    pub allow_signup: bool,
    /// When not empty, only emails in these domains (lowercase) may sign up.
    pub allowed_email_domains: Vec<String>,
    /// Only deployment owners may create workspaces.
    pub disable_workspace_creation: bool,
}

impl Default for SignupPolicy {
    fn default() -> Self {
        Self {
            allow_signup: true,
            allowed_email_domains: Vec::new(),
            disable_workspace_creation: false,
        }
    }
}

impl SignupPolicy {
    /// Reads the policy through `var` (e.g. `|n| std::env::var(n).ok()`):
    /// signup is on unless `SUN_ALLOW_SIGNUP` is `false`/`0`/`no`/`off`;
    /// `SUN_ALLOWED_EMAIL_DOMAINS` is a comma-separated list;
    /// `SUN_DISABLE_WORKSPACE_CREATION` is on for `true`/`1`/`yes`/`on`.
    pub fn from_env(var: impl Fn(&str) -> Option<String>) -> Self {
        let flag = |name: &str| var(name).map(|v| v.trim().to_ascii_lowercase());
        let off = flag("SUN_ALLOW_SIGNUP")
            .is_some_and(|v| matches!(v.as_str(), "false" | "0" | "no" | "off"));
        let on = flag("SUN_DISABLE_WORKSPACE_CREATION")
            .is_some_and(|v| matches!(v.as_str(), "true" | "1" | "yes" | "on"));
        let allowed_email_domains = var("SUN_ALLOWED_EMAIL_DOMAINS")
            .unwrap_or_default()
            .split(',')
            .map(|d| d.trim().trim_start_matches('@').to_ascii_lowercase())
            .filter(|d| !d.is_empty())
            .collect();
        Self {
            allow_signup: !off,
            allowed_email_domains,
            disable_workspace_creation: on,
        }
    }

    /// True when `email` (normalized) may sign up without an invitation.
    pub fn admits(&self, email: &str) -> bool {
        let domain = email.rsplit_once('@').map_or("", |(_, d)| d);
        self.allow_signup
            && (self.allowed_email_domains.is_empty()
                || self.allowed_email_domains.iter().any(|d| d == domain))
    }
}

/// `member` — a person's identity on the deployment: someone who signed up
/// or was invited, linked to an OIDC identity on their first provider
/// sign-in, or the local owner (the token holder, issuer [`OWNER_ISSUER`]).
/// What they may do comes from their [`Role`] in each workspace
/// ([`WorkspaceMember`]), or everything for a deployment owner. The password
/// hash never leaves the store.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Member {
    /// Id.
    pub id: MemberId,
    /// OIDC issuer URL.
    pub issuer: String,
    /// OIDC `sub`, unique per issuer.
    pub subject: String,
    /// Name matched by approval `who: user:<name>` (`preferred_username`,
    /// else email, else `sub`).
    pub username: String,
    /// Email, lowercase. `None` only for a member from before invites that has
    /// not given one yet: they are asked on their next sign-in.
    pub email: Option<String>,
    /// A deployment owner (super-admin) has every permission in every
    /// workspace, always (the token holder is one).
    #[serde(default)]
    pub is_owner: bool,
    /// When they proved they own their email (a code, or an SSO sign-in);
    /// `None`: signed up or invited, not verified yet.
    #[serde(default)]
    pub email_verified_at: Option<Timestamp>,
    /// They set a password (an invite still pending has none).
    #[serde(default)]
    pub has_password: bool,
    /// Signed in with an invite code: nothing but setting a password is
    /// allowed until they do.
    #[serde(default)]
    pub must_set_password: bool,
    /// When an admin deactivated them: they can't sign in until reactivated.
    #[serde(default)]
    pub deactivated_at: Option<Timestamp>,
    /// They set up a second sign-in step.
    #[serde(default)]
    pub has_two_step: bool,
}

/// `member.issuer` of members an admin invited, before they link an OIDC
/// identity; their `subject` is their email.
pub const LOCAL_ISSUER: &str = "local";
/// `member.issuer` of the local owner (the token holder); the `subject` is
/// the kernel principal's user name.
pub const OWNER_ISSUER: &str = "token";
/// The first-run owner's placeholder email: they sign in with it (and the
/// kernel token as the password), then must replace it.
pub const FIRST_ADMIN_EMAIL: &str = "admin@sun.local";

/// Checks and lowercases an email address: one `@` with text on both sides,
/// a dot in the domain, at most 254 bytes, no spaces or control characters.
pub fn normalize_email(email: &str) -> Result<String, crate::Error> {
    let email = email.trim().to_ascii_lowercase();
    let ok = email.len() <= 254
        && !email.chars().any(|c| c.is_whitespace() || c.is_control())
        && email.split_once('@').is_some_and(|(local, domain)| {
            !local.is_empty()
                && !domain.contains('@')
                && domain.contains('.')
                && !domain.starts_with('.')
                && !domain.ends_with('.')
        });
    if ok {
        Ok(email)
    } else {
        Err(crate::Error::InvalidInput(
            "That is not an email address.".into(),
        ))
    }
}

string_enum! {
    /// How the SMTP connection is secured.
    pub enum SmtpTls {
        /// Plain connect, then `STARTTLS` (usually port 587).
        Starttls => "starttls",
        /// TLS from the first byte (usually port 465).
        Tls => "tls",
    }
}

/// Outgoing mail settings, the `smtp` kernel setting. The password is
/// the ecosystem secret `SMTP_PASSWORD`, never part of this.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SmtpSettings {
    /// Server host name.
    pub host: String,
    /// Server port.
    pub port: u16,
    /// Login name; empty = no authentication.
    pub username: String,
    /// The `From:` address.
    pub from: String,
    /// How the connection is secured.
    pub tls: SmtpTls,
}

impl SmtpSettings {
    /// Host is a plain host name, port is not 0, `from` is an email, and no
    /// field holds control characters (header injection).
    pub fn validate(&self) -> Result<(), crate::Error> {
        let bad = |m: &str| Err(crate::Error::InvalidInput(m.into()));
        let host_ok = (1..=253).contains(&self.host.len())
            && self
                .host
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-.".contains(c));
        if !host_ok {
            return bad("The SMTP host must be a host name such as smtp.example.com.");
        }
        if self.port == 0 {
            return bad("The SMTP port must be 1 to 65535.");
        }
        if self.username.len() > 256 || self.username.chars().any(char::is_control) {
            return bad("The SMTP username is not valid.");
        }
        normalize_email(&self.from)
            .map_err(|_| crate::Error::InvalidInput("The From address is not an email.".into()))?;
        Ok(())
    }
}

string_enum! {
    /// A second sign-in step a member set up.
    pub enum FactorKind {
        /// An authenticator app: RFC 6238 codes from a sealed secret.
        Totp => "totp",
        /// A 6-digit code mailed to the member's email.
        Email => "email",
        /// A WebAuthn passkey.
        Passkey => "passkey",
    }
}

/// One of a member's second steps (`member_factor`); its secret never
/// leaves the store but for checking a code.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Factor {
    /// Id.
    pub id: crate::FactorId,
    /// What it is.
    pub kind: FactorKind,
    /// The member's name for it, e.g. "Phone".
    pub label: String,
    /// When it was added.
    pub created_at: Timestamp,
}

/// The ecosystem's 2-step rules, the `security` kernel setting.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecurityPolicy {
    /// Every member signing in with a password needs a second step; those
    /// without one must add one before anything else.
    #[serde(default)]
    pub require_two_step: bool,
    /// Changing your own password also needs your second step.
    #[serde(default)]
    pub two_step_for_password_change: bool,
}

/// One plain-text mail.
#[derive(Debug, Clone, PartialEq)]
pub struct Mail {
    /// Recipient address.
    pub to: String,
    /// Subject line.
    pub subject: String,
    /// Plain-text body.
    pub body: String,
}

string_enum! {
    /// What a security-relevant action was.
    pub enum AuditAction {
        /// Someone signed in (local token or OIDC).
        Login => "login",
        /// A member's workspace role or deployment owner flag changed.
        RoleChange => "role_change",
        /// Before roles: a group was created, renamed, deleted, or its
        /// permissions or members changed (kept to read old audit rows).
        GroupChange => "group_change",
        /// A member was invited, re-invited or removed, or set their email or password.
        MemberChange => "member_change",
        /// A person read a secret value.
        SecretRead => "secret_read",
        /// A secret was set, replaced or deleted (never its value).
        SecretChange => "secret_change",
        /// An ecosystem setting changed (e.g. the embedding provider).
        SettingsChange => "settings_change",
        /// A mod was installed.
        ModInstall => "mod_install",
        /// An approval step was decided.
        Approval => "approval",
        /// A runtime's permission mode changed.
        PolicyChange => "policy_change",
        /// A trigger (mod event, cron, alert) failed to start or route work.
        TriggerFailed => "trigger_failed",
        /// A workspace was created, renamed or deleted.
        WorkspaceChange => "workspace_change",
        /// An agent was created, edited or archived.
        AgentChange => "agent_change",
    }
}

string_enum! {
    /// State of an [`EvalRun`].
    pub enum EvalStatus {
        /// Replaying.
        Running => "running",
        /// Finished; the report is set.
        Done => "done",
        /// Stopped; `error` says why.
        Failed => "failed",
    }
}

/// `eval_set` — past tasks saved to replay against later changes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalSet {
    /// Id.
    pub id: EvalSetId,
    /// Name.
    pub name: String,
    /// The tasks, in order.
    pub task_ids: Vec<TaskId>,
    /// When.
    pub created_at: Timestamp,
}

/// `eval_run` — one replay of past tasks against a candidate instruction,
/// memory or workflow version, and its report. `spec` and
/// `report` are the engine's JSON shapes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalRun {
    /// Id.
    pub id: EvalRunId,
    /// Name.
    pub name: String,
    /// What to replay, against what.
    pub spec: Value,
    /// State.
    pub status: EvalStatus,
    /// The side-by-side result, once done.
    pub report: Option<Value>,
    /// Why it failed.
    pub error: Option<String>,
    /// Started.
    pub created_at: Timestamp,
    /// Finished.
    pub finished_at: Option<Timestamp>,
}

/// `audit` — one security-relevant action. Append-only.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuditEntry {
    /// Id (time-ordered).
    pub id: AuditId,
    /// Who did it, e.g. `user:alice` or `token:local`.
    pub actor: String,
    /// What.
    pub action: AuditAction,
    /// On what, e.g. a member name, a mod name or `<run>/<step>`.
    pub target: String,
    /// Short detail, e.g. `viewer -> admin` or `approved`. Never a secret value.
    pub detail: String,
    /// When.
    pub at: Timestamp,
}

#[cfg(test)]
mod member_tests {
    use super::*;

    #[test]
    fn workspace_names_are_trimmed_and_1_to_64_chars() {
        assert_eq!(Workspace::clean_name("  Main ").unwrap(), "Main");
        assert!(Workspace::clean_name("   ").is_err());
        assert!(Workspace::clean_name(&"é".repeat(64)).is_ok());
        assert!(Workspace::clean_name(&"a".repeat(65)).is_err());
    }

    #[test]
    fn workspace_slugs_and_prefixes_follow_their_rules() {
        // Same cases as the profile migration's backfill.
        assert_eq!(Workspace::slug_from_name("Main"), "main");
        assert_eq!(
            Workspace::slug_from_name("  --Hello__World--  "),
            "hello-world"
        );
        assert_eq!(Workspace::slug_from_name("Été 🚀"), "t");
        assert_eq!(Workspace::slug_from_name("$$$"), "workspace");
        let long = format!("{}-3", "x".repeat(60));
        assert_eq!(Workspace::slug_from_name(&long), "x".repeat(48));
        assert_eq!(Workspace::slug_with_suffix("my-app", 2), "my-app-2");
        let cut = Workspace::slug_with_suffix(&format!("{}-y", "x".repeat(45)), 10);
        assert_eq!(cut, format!("{}-10", "x".repeat(45)));
        for ok in ["main", "a", "my-app-2", &"a".repeat(48)] {
            assert_eq!(Workspace::clean_slug(ok).unwrap(), ok);
        }
        for bad in ["", "-a", "a-", "a--b", "A", "a_b", "é", &"a".repeat(49)] {
            assert!(Workspace::clean_slug(bad).is_err(), "{bad}");
        }
        assert_eq!(Workspace::prefix_from_slug("main"), "MAIN");
        assert_eq!(Workspace::prefix_from_slug("a-2"), "A2");
        assert_eq!(Workspace::clean_prefix(" eng2 ").unwrap(), "ENG2");
        for bad in ["", "A-B", "ÉT", &"A".repeat(11)] {
            assert!(Workspace::clean_prefix(bad).is_err(), "{bad}");
        }
        assert!(Workspace::clean_context(&"é".repeat(8000)).is_ok());
        assert!(Workspace::clean_context(&"a".repeat(8001)).is_err());
        assert_eq!(Workspace::prefix_with_suffix("MAIN", 2), "MAIN2");
        assert_eq!(
            Workspace::prefix_with_suffix("ABCDEFGHIJ", 12),
            "ABCDEFGH12"
        );
    }

    #[test]
    fn task_keys_print_and_parse_ignoring_case() {
        let key = TaskKey {
            prefix: "ENG".into(),
            number: 12,
        };
        assert_eq!(key.to_string(), "ENG-12");
        assert_eq!("eng-12".parse::<TaskKey>().unwrap(), key);
        assert_eq!(" ENG-12 ".parse::<TaskKey>().unwrap(), key);
        for bad in [
            "ENG",
            "ENG-",
            "-12",
            "ENG-0",
            "ENG-+1",
            "EN G-1",
            "ENG-1x",
            "01K0000000",
        ] {
            assert!(bad.parse::<TaskKey>().is_err(), "{bad}");
        }
        assert!(Priority::Urgent.rank() > Priority::None.rank());
        assert!(
            Priority::Low.rank() > Priority::None.rank(),
            "no priority is last"
        );
        assert_eq!(
            [5, 0, -2].map(Priority::from_int),
            [Priority::High, Priority::None, Priority::Low]
        );
        assert_eq!(
            [0, 1, 2, 3, 9].map(Priority::from_level),
            [
                Priority::None,
                Priority::Medium,
                Priority::High,
                Priority::Urgent,
                Priority::Urgent
            ]
        );
        assert!(TaskActivity::clean_comment("  ").is_err());
        assert!(TaskActivity::clean_comment(&"é".repeat(10_000)).is_ok());
        assert!(TaskActivity::clean_comment(&"a".repeat(10_001)).is_err());
    }

    #[test]
    fn task_branches_round_trip_their_key() {
        let key = TaskKey {
            prefix: "SUN".into(),
            number: 12,
        };
        let branch = key.branch("Fix login!  (again)");
        assert_eq!(branch, "sun/SUN-12-fix-login-again");
        assert_eq!(TaskKey::from_branch(&branch), Some(key.clone()));
        assert_eq!(key.branch("🚀"), "sun/SUN-12");
        assert_eq!(TaskKey::from_branch("sun/SUN-12"), Some(key.clone()));
        assert_eq!(TaskKey::from_branch("sun/sun-12-x"), Some(key.clone()));
        let long = key.branch(&"word ".repeat(20));
        assert_eq!(
            long.len(),
            "sun/SUN-12-".len() + TaskKey::MAX_BRANCH_SLUG - 1
        );
        assert!(!long.ends_with('-'));
        for other in [
            "sun/skill-tdd",
            "sun/01J9X8K2M3N4P5Q6R7S8T9V0WX",
            "sun/SUN-0-x",
            "sun/SUN-",
            "main",
            "SUN-12",
            "feature/sun/SUN-12",
        ] {
            assert_eq!(TaskKey::from_branch(other), None, "{other}");
        }
        let settings: GitSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(settings, GitSettings::default());
        assert!(settings.move_on_merge);
        let off: GitSettings = serde_json::from_str(r#"{"move_on_merge":false}"#).unwrap();
        assert!(!off.move_on_merge && off.auto_link);
    }

    #[test]
    fn workspace_logos_are_checked_by_magic_bytes() {
        let png = b"\x89PNG\r\n\x1a\n rest";
        assert_eq!(
            Workspace::check_logo(png, "image/png").unwrap(),
            "image/png"
        );
        assert!(Workspace::check_logo(png, "image/jpeg").is_err());
        assert!(Workspace::check_logo(b"<svg/>", "image/svg+xml").is_err());
        assert!(Workspace::check_logo(b"GIF89a", "image/png").is_err());
        let webp = b"RIFF\0\0\0\0WEBPVP8 ";
        assert_eq!(
            Workspace::check_logo(webp, "image/webp").unwrap(),
            "image/webp"
        );
        let jpeg = [0xFF, 0xD8, 0xFF, 0xE0];
        assert_eq!(Workspace::logo_type(&jpeg), Some("image/jpeg"));
        let big = [&png[..], &vec![0; Workspace::MAX_LOGO][..]].concat();
        assert!(Workspace::check_logo(&big, "image/png").is_err());
    }

    #[test]
    fn roles_grant_the_seeded_matrices_and_owners_have_all() {
        assert!(Role::Viewer.permissions().is_empty());
        assert_eq!(
            Role::Member.permissions(),
            [
                Permission::Run,
                Permission::Approve,
                Permission::CreateWorkflow,
                Permission::EditWorkflow
            ]
        );
        assert_eq!(Role::Owner.permissions(), Permission::ALL);
        // Admin: every permission left once groups went (an owner differs
        // in what the engine lets them do: owners, the last-owner guard).
        let admin = Role::Admin.permissions();
        assert_eq!(admin, Permission::ALL);
        assert_eq!(permissions_of(false, &[Role::Admin]), admin);
        assert_eq!(Role::Admin.as_str(), "admin");
        assert_eq!("viewer".parse::<Role>().unwrap(), Role::Viewer);
        // Every role's permissions sit inside the next one up.
        let ladder = [Role::Viewer, Role::Member, Role::Admin, Role::Owner];
        for pair in ladder.windows(2) {
            assert!(pair[1].covers(pair[0]) && !pair[0].covers(pair[1]));
        }
        assert!(Role::Admin.covers(Role::Admin) && !Role::Admin.covers(Role::Owner));
        // A deployment owner has every permission with no role at all.
        assert!(permissions_of(false, &[]).is_empty());
        assert_eq!(permissions_of(true, &[]), Permission::ALL);
        assert_eq!(
            permissions_of(false, &[Role::Viewer, Role::Member]),
            Role::Member.permissions()
        );
        assert_eq!(
            roles_of(false, &[Role::Member, Role::Viewer]),
            ["viewer", "member"]
        );
        assert_eq!(
            roles_of(false, &[Role::Admin]),
            ["viewer", "member", "admin"]
        );
        assert_eq!(roles_of(true, &[]), ["viewer", "member", "admin", "owner"]);
        assert!(roles_of(false, &[]).is_empty());
    }

    #[test]
    fn signup_policy_reads_env_and_checks_domains() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(k, _)| *k == name)
                    .map(|(_, v)| (*v).to_owned())
            }
        };
        let open = SignupPolicy::from_env(env(&[]));
        assert_eq!(open, SignupPolicy::default());
        assert!(open.admits("a@x.io"));
        let closed = SignupPolicy::from_env(env(&[
            ("SUN_ALLOW_SIGNUP", " False "),
            ("SUN_DISABLE_WORKSPACE_CREATION", "1"),
        ]));
        assert!(!closed.admits("a@x.io") && closed.disable_workspace_creation);
        let domains = SignupPolicy::from_env(env(&[(
            "SUN_ALLOWED_EMAIL_DOMAINS",
            " @Acme.io, ,beta.dev",
        )]));
        assert_eq!(domains.allowed_email_domains, ["acme.io", "beta.dev"]);
        assert!(domains.admits("a@acme.io") && domains.admits("b@beta.dev"));
        assert!(!domains.admits("a@evil.io") && !domains.admits("a@sub.acme.io"));
    }
}

#[cfg(test)]
mod runtime_tests {
    use super::*;

    #[test]
    fn secret_deployment_names_stay_in_the_ecosystem() {
        for name in [
            "SMTP_PASSWORD",
            "SSO_GOOGLE_SECRET",
            "GITHUB_APP_ID",
            "EMBEDDING_API_KEY",
        ] {
            assert!(Secret::is_deployment(name), "{name}");
        }
        for name in [
            "OR_KEY",
            "SSO__SECRET",
            "SSO_SECRET",
            "JIRA_TOKEN",
            "SMTP_PASSWORD2",
        ] {
            assert!(!Secret::is_deployment(name), "{name}");
        }
    }

    #[test]
    fn byo_key_runtimes_name_the_field_to_fix() {
        let byo = |endpoint: &str, model: &str, key: Option<&str>| crate::ByoKey {
            kind: crate::ByoKeyKind::OpenaiCompatible,
            endpoint: endpoint.into(),
            model: model.into(),
            key_secret: key.map(Into::into),
        };
        let runtime = |name: &str, adapter: &str, source, b| Runtime {
            id: RuntimeId::generate(),
            workspace_id: WorkspaceId::MAIN,
            name: name.into(),
            adapter: adapter.into(),
            source,
            byo_key: b,
            created_by: None,
        };
        let ollama = byo("http://localhost:11434/v1", "qwen2.5-coder:7b", None);
        let ok = runtime("ollama", "opencode", RuntimeSource::ByoKey, Some(ollama));
        assert_eq!(ok.validate(), Ok(()));
        let plain = runtime(
            "claude-code",
            "claude-code",
            RuntimeSource::ByoRuntime,
            None,
        );
        assert_eq!(plain.validate(), Ok(()));
        let field = |r: Runtime| match r.validate() {
            Err(crate::Error::InvalidInput(m)) => m.split(':').next().unwrap_or("").to_owned(),
            other => panic!("{other:?}"),
        };
        let with = |b| runtime("x", "opencode", RuntimeSource::ByoKey, Some(b));
        assert_eq!(field(with(byo(" ", "m", None))), "endpoint");
        assert_eq!(
            field(with(byo("http://u:sk-secret@h/v1", "m", None))),
            "endpoint"
        );
        assert_eq!(field(with(byo("ftp://h", "m", None))), "endpoint");
        assert_eq!(field(with(byo("https://h/v1", "a b", None))), "model");
        assert_eq!(
            field(with(byo("https://h/v1", "m", Some("1X")))),
            "key_secret"
        );
        let bad_name = runtime("Big Name", "opencode", RuntimeSource::ByoRuntime, None);
        assert_eq!(field(bad_name), "name");
        // Claude Code takes an anthropic key (endpoint optional), Codex an
        // OpenAI-compatible one; nothing else.
        let codex = runtime(
            "x",
            "codex",
            RuntimeSource::ByoKey,
            Some(byo("http://h", "m", None)),
        );
        assert_eq!(codex.validate(), Ok(()));
        let claude = |b| runtime("x", "claude-code", RuntimeSource::ByoKey, Some(b));
        assert_eq!(field(claude(byo("http://h", "m", None))), "adapter");
        let anthropic = crate::ByoKey {
            kind: crate::ByoKeyKind::Anthropic,
            ..byo("", "claude-sonnet-4-5", Some("ANTHROPIC_KEY"))
        };
        assert_eq!(claude(anthropic.clone()).validate(), Ok(()));
        let codex = runtime("x", "codex", RuntimeSource::ByoKey, Some(anthropic));
        assert_eq!(field(codex), "adapter");
        let unknown = runtime(
            "x",
            "aider",
            RuntimeSource::ByoKey,
            Some(byo("http://h", "m", None)),
        );
        assert_eq!(field(unknown), "adapter");
        assert_eq!(
            field(runtime("x", "opencode", RuntimeSource::ByoKey, None)),
            "endpoint"
        );
        let stray = runtime(
            "x",
            "opencode",
            RuntimeSource::Credits,
            Some(byo("http://h", "m", None)),
        );
        assert_eq!(field(stray), "source");
        let Err(crate::Error::InvalidInput(m)) =
            with(byo("http://u:sk-secret@h/v1", "m", None)).validate()
        else {
            panic!()
        };
        assert!(!m.contains("sk-secret"), "{m}");
    }
}
