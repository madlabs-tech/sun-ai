//! # sun-engine — run orchestration
//!
//! **Responsibility:** the single writer that owns every run and step
//! transition: queue, flow control, the step **state machine**
//! (`sun_core::StateMachine`), gates, approvals, watchdog and runtime failover
//! (**Strategy** / **Chain of Responsibility**).
//!
//! **Contract:**
//! - Wakes on events (hosts, mods, timers, API), never polls an LLM.
//! - Talks to hosts and mods only through `sun_core::HostLink` / `sun_core::ModLink`;
//!   `sun-cli` injects the real implementations.
//! - Only the engine moves a step to `succeeded`; an agent's "done" only moves
//!   `running → verifying`.
//!
//! **May depend on:** `sun-core`, `sun-store`, `sun-workflow`, `sun-context`.
//!
//! ## Layout
//! - [`Engine`] (`executor`): the durable executor. One task, one store
//!   transaction per state change, resume on boot.
//! - `state`: pure decisions (which steps are ready, how a run ends, what an
//!   agent event does to its step).
//! - `outbox`: the **Transactional outbox** worker that delivers side effects
//!   through `ModLink` after commit.
//! - `watchdog` (child of `executor`): stall, loop, budget and explicit-end
//!   rules over the agent event stream.
//! - [`Queue`] (`queue`): flow control. Priority then FIFO, caps per
//!   runtime/host/ecosystem, no new work on a cooling runtime.
//! - `gates`: the gate **Strategy** (code gates on the host, content gates in
//!   the engine) and send-back. `approval`: who may decide, and timeouts.
//! - `decision` (child of `executor`): the `decide` step and its
//!   `/v1/systemone` client (**Adapter**). A [`Decision`] can't become a
//!   [`GateResult`] or an approval.
//! - `device`: the `device.verify_on_device` step on a host simulator
//!   or emulator; screenshots go to the PR through the git host plugin.
//! - `corrections` (child of `executor`): captures gate fails, rejects,
//!   reverts, contradicting steers and overridden decisions as `correction`
//!   rows labelled by cause.
//! - `push`: pushes the task branch before a git host's `open_pr`.
//! - `vcs`: workspace git connections (GitHub App, GitLab OAuth or token),
//!   their repos, and the per-call credentials of pushes and git host mods.
//! - `attention`: notifier messages with a deep link when a step needs a
//!   person (approval, watchdog).
//! - `data_touched` / `quality` (extensions): data-touched
//!   detection over the task diff, and how merged work held up afterwards.
//! - `eval` (extension): replays past tasks in a sandbox store and
//!   engine against a candidate memory / workflow; side effects are recorded.
//! - `triggers`: mod triggers start tasks per workflow `on:`,
//!   alerts and review corrections; run hooks feed `observe` mods the log.
//! - `query`: the read **Facade** the API uses (it may not reach the store).
//! - `conversation`: an agent's final message as its comment, and comment-woken
//!   runs that pick the conversation up.

mod access;
mod agents;
mod approval;
mod attention;
mod base;
mod budget;
mod conversation;
mod data_touched;
mod device;
mod dry_run;
mod eval;
mod executor;
mod gates;
mod knowledge;
mod market;
mod mcp;
mod memory;
mod multi_repo;
mod outbox;
mod pins;
mod projects;
mod pull_requests;
mod push;
mod quality;
mod query;
mod queue;
mod queued;
mod repo_resolve;
mod retention;
mod roles;
mod screenshot;
mod secrets;
mod skill_pr;
mod snapshot;
mod sso;
mod state;
mod triggers;
mod two_step;
mod users;
mod vcs;
mod workspaces;

pub use access::credits_model_key;
pub use agents::{AGENT_STEP, AgentFields};
pub use approval::Principal;
pub use attention::Attention;
pub use budget::Spend;
pub use conversation::Chat;
pub use data_touched::Touch;
pub use dry_run::{SimStep, Simulation};
pub use eval::{
    Candidate, EvalReport, EvalSpec, MAX_TASKS as MAX_EVAL_TASKS, Outcome, Pick, REPLAY, Side,
    TaskResult,
};
pub use executor::{Actor, NewTask, TaskChange, WorkflowPick};
pub use executor::{
    Answer, Decision, DecisionClient, DecisionReplay, Design, Engine, EngineConfig, PLUGIN_METHOD,
    RUNTIMES_MODEL, Resolution, ScreenPath, SessionMcp, StallPolicy, Tally, WORKTREE_RETENTION,
    WatchdogConfig,
};
pub use gates::GateResult;
pub use knowledge::{
    ContextPreview, EMBEDDING_API_KEY, EmbeddingProvider, EmbeddingSettings, SuggestedSource,
    SyncReport, ecosystem_scope,
};
pub use market::{
    MARKET_FAILOVER, MARKET_MOD, MARKET_PREFIX, MarketAllow, MarketPolicy, RELEASE_LEASE, TaskLease,
};
pub use mcp::{MAX_TASK_MCP_GRANTS, mcp_secret_values};
pub use memory::{
    FirstPass, MemoryMetrics, MemoryView, NamedRate, PassRate, PrView, RepeatRate, WeekRate,
};
pub use multi_repo::RepoPr;
pub use pins::{Author, PinChange, PinCommit, Screen};
pub use projects::ProjectChange;
pub use pull_requests::{
    AutoComplete, AutoCompleteState, LinkSource, PrEvent, PullRequestView, TaskPullRequests,
};
pub use quality::{DEFAULT_WINDOW_DAYS, MERGED_TRIGGER, Merge, QualityMetrics};
pub use query::{DryRun, DryStep, ModNeed, Plan, PlannedStep, check_workflow};
pub use queue::{Caps, HostCap, Queue, QueueConfig, Slot};
pub use roles::Access;
pub use secrets::{audit_secret_read, delete_secret, set_secret};
pub use skill_pr::SkillPr;
pub use sun_context::McpSessions;
pub use sun_store::{DocChunk, DocSummary, SourceSummary, TaskContext, Usage, WorkspaceItem};
pub use sun_workflow::OnLimit;
pub use sun_workflow::{
    TEMPLATES as WORKFLOW_TEMPLATES, Template as WorkflowTemplate, WorkflowError,
};
pub use triggers::{ALERT_TRIGGER, Hooks, KERNEL_TRIGGERS, METRIC, REVIEW_TRIGGER, hook, hook_of};
pub use users::{FirstSignIn, SMTP_PASSWORD};
pub use vcs::{
    AddedRepo, GITHUB_APP_API_URL, GITHUB_APP_ID, GITHUB_APP_PRIVATE_KEY, GITHUB_APP_SLUG,
    GITHUB_APP_WEBHOOK_SECRET, GITLAB_OAUTH_CLIENT_ID, GITLAB_OAUTH_CLIENT_SECRET,
    GITLAB_OAUTH_URL, MAX_REPO_DESCRIPTION, ManualHook, RemoteRepo, Rotation, VcsApps, check_url,
};
pub use workspaces::{OnWorkspace, ProfileChange, Reach};
