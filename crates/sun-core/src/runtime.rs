//! [`RuntimeAdapter`]: the only thing the engine knows about agents.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::pin::Pin;

use async_trait::async_trait;
use futures_core::Stream;
use serde::{Deserialize, Serialize};

use crate::{HostId, Result, RunId, SessionId, StepKey, SunEvent};

/// A boxed, `Send` stream (same shape as `futures::stream::BoxStream`).
pub type BoxStream<'a, T> = Pin<Box<dyn Stream<Item = T> + Send + 'a>>;

/// **Adapter** over one agent CLI (Claude Code, Codex, OpenCode, ...).
///
/// Contract for every impl (contract tests run against all of them):
/// - [`id`](Self::id) is stable and unique, e.g. `"claude-code"`.
/// - [`events`](Self::events) yields every event of the session exactly once,
///   in order, normalized to [`SunEvent`], and ends after [`SunEvent::Exited`].
///   Limits are reported as [`SunEvent::LimitHit`], never as [`SunEvent::Error`].
/// - [`DoneClaimed`](SunEvent::DoneClaimed) is only a claim; the adapter never
///   decides that work is finished.
/// - [`stop`](Self::stop) is idempotent: stopping a stopped session is `Ok`.
/// - Text leaving the adapter is already redacted ([`crate::redact`]).
#[async_trait]
pub trait RuntimeAdapter: Send + Sync {
    /// Stable adapter id, e.g. `"claude-code"`.
    fn id(&self) -> &str;

    /// Is the CLI installed on this host, which version, is it logged in?
    async fn readiness(&self, host: &HostCtx) -> Readiness;

    /// The effort levels the installed CLI itself offers (read from the CLI,
    /// never a Sun list), probed with [`readiness`](Self::readiness). Uses
    /// no quota. Default: none, so the agent form greys the picker out.
    async fn effort(&self, _host: &HostCtx) -> EffortCatalog {
        EffortCatalog::default()
    }

    /// The [`PermissionMode`]s the installed CLI can enforce, probed with
    /// [`readiness`](Self::readiness); the agent form and save-time checks
    /// offer only these. Default: [`PermissionMode::LEGACY`].
    async fn permission_modes(&self, _host: &HostCtx) -> Vec<PermissionMode> {
        PermissionMode::legacy()
    }

    /// Starts a session in `spec.worktree` with the rendered envelope.
    async fn start(&self, spec: SessionSpec) -> Result<SessionHandle>;

    /// Steers the running agent with a message now.
    async fn send(&self, session: &SessionHandle, msg: Steer) -> Result<()>;

    /// Stops the session.
    async fn stop(&self, session: &SessionHandle, how: StopMode) -> Result<()>;

    /// The session's normalized event stream.
    fn events(&self, session: &SessionHandle) -> BoxStream<'static, SunEvent>;

    /// Exports the transcript, used to build hand-off packs.
    async fn export(&self, session: &SessionHandle) -> Result<Transcript>;

    /// Context window size of `model` in tokens, if known.
    fn context_window(&self, model: &str) -> Option<u32>;
}

/// The host a readiness check runs on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostCtx {
    /// The host.
    pub host_id: HostId,
}

/// Result of [`RuntimeAdapter::readiness`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Readiness {
    /// The CLI binary was found.
    pub installed: bool,
    /// The CLI's reported version.
    pub version: Option<String>,
    /// The CLI has usable credentials (subscription login or key).
    pub logged_in: bool,
    /// Human-readable reason when not ready, with a fix if known.
    pub detail: Option<String>,
}

/// The effort (reasoning) levels a CLI offers, in its own vocabulary:
/// Claude Code's `--effort`, Codex's `model_reasoning_effort`. Same shape as
/// Multica's per-model thinking catalog (`RuntimeModel.thinking`), so the
/// agent form renders the picker from it alone.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffortCatalog {
    /// Levels for any model, lowest first; empty when the CLI has no effort
    /// setting (or the probe could not read it).
    #[serde(default)]
    pub effort_options: Vec<EffortLevel>,
    /// Per-model levels, when the CLI scopes effort by model (Codex). A
    /// model not listed here uses `effort_options`.
    #[serde(default)]
    pub models: Vec<ModelEffort>,
}

/// One effort level.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffortLevel {
    /// The CLI's own token, passed to it unchanged, e.g. `xhigh`.
    pub value: String,
    /// What the CLI's own UI calls it, e.g. `Extra high`.
    pub label: String,
    /// The CLI's description of the level, when it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// The effort levels of one model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelEffort {
    /// The model id the CLI takes, e.g. `gpt-5-codex`.
    pub id: String,
    /// Its display name.
    pub label: String,
    /// Levels this model accepts, lowest first; empty: no effort setting.
    #[serde(default)]
    pub effort_options: Vec<EffortLevel>,
    /// The level the CLI uses when none is set, if it says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_effort: Option<String>,
}

/// Everything needed to start one agent session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSpec {
    /// Sun's id for the new session.
    pub session_id: SessionId,
    /// The run the session works for.
    pub run_id: RunId,
    /// The workflow step it executes.
    pub step_key: StepKey,
    /// Working directory (the task's git worktree).
    pub worktree: PathBuf,
    /// The first prompt as plain text; sent as is when `envelope_parts` is
    /// absent (an older kernel, or the whole pack riding inline).
    pub envelope: String,
    /// The envelope's parts; the adapter renders them into its
    /// CLI's editable template instead of sending `envelope`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub envelope_parts: Option<Envelope>,
    /// Model override; `None` uses the CLI default.
    pub model: Option<String>,
    /// Effort level, one of the tokens the host reported for this tool and
    /// model ([`EffortCatalog`]); the adapter maps it to its CLI's flag, and
    /// one whose CLI has no such setting ignores it. `None`: the CLI default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// MCP config file to pass to the CLI, if any.
    pub mcp_config: Option<PathBuf>,
    /// Extra environment, e.g. model endpoint and key for BYO-key or credits.
    /// Values are secrets: never log them.
    pub env: BTreeMap<String, String>,
    /// Where the CLI runs. Absent in JSON means [`ExecEnvSpec::Bare`].
    #[serde(default)]
    pub exec_env: ExecEnvSpec,
    /// The CLI's own id of an earlier session to continue:
    /// `claude --resume`, `codex exec resume`, ACP `session/load`. `envelope`
    /// is then the next user message of that conversation. When the CLI
    /// can't resume it, the session fails before any agent output.
    #[serde(default)]
    pub resume: Option<String>,
    /// The permission mode set for this runtime in the UI (one of
    /// [`PERMISSION_MODES`]); `None` keeps the host's own default.
    #[serde(default)]
    pub permission_mode: Option<String>,
    /// The `byo_key` source of the session's runtime; the key value is in
    /// `env[BYO_KEY_ENV]`. OpenCode takes any [`ByoKeyKind`], Claude Code
    /// `anthropic` and Codex `openai_compatible` ([`ByoKeyKind::runs_on`]).
    #[serde(default)]
    pub byo_key: Option<ByoKey>,
    /// The skills of the store the session gets: those the run's workspace
    /// installed. `None` (an older kernel): every skill in the store.
    #[serde(default)]
    pub skills: Option<Vec<String>>,
}

/// What every agent is told, whatever the CLI: one
/// structure, rendered per CLI from an editable template (`sun-runtime`
/// `envelope`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Envelope {
    /// The task itself.
    pub task: String,
    /// Rules the agent must follow.
    pub constraints: Vec<String>,
    /// Checks that decide whether the work counts (tests, lint, review).
    pub gates: Vec<String>,
    /// Pointers to context files or docs (pull-based).
    pub context: Vec<String>,
    /// The hand-off pack from a previous session, if any.
    pub handoff: Option<String>,
}

/// Sun's permission policy vocabulary, which every adapter maps to its CLI
/// (each adapter's module doc has its table). Strictness, most restrictive
/// first: `plan` < `default` < `accept_edits` < `auto` < `bypass_permissions`.
/// Only `bypass_permissions` needs the operator's `allow_bypass` and an
/// isolated exec env; `auto` is not a bypass.
pub const PERMISSION_MODES: [&str; 5] = [
    "default",
    "accept_edits",
    "plan",
    "auto",
    "bypass_permissions",
];

/// One of [`PERMISSION_MODES`], parsed. Every mode a session runs in passes
/// through this type: a spelling outside the list (`bypassPermissions`,
/// `BYPASS_PERMISSIONS`, ` plan`) is an error, never passed on to a CLI that
/// might read it as something else.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionMode {
    /// The CLI asks before anything that needs a permission.
    Default,
    /// File edits without asking.
    AcceptEdits,
    /// Read-only planning.
    Plan,
    /// The CLI approves what it judges safe itself and refuses the rest
    /// (Claude Code's `auto`, Codex's automatic reviewer).
    Auto,
    /// Everything without asking: only in a container or VM.
    BypassPermissions,
}

impl PermissionMode {
    /// Every mode, in [`PERMISSION_MODES`] order.
    pub const ALL: [Self; 5] = [
        Self::Default,
        Self::AcceptEdits,
        Self::Plan,
        Self::Auto,
        Self::BypassPermissions,
    ];

    /// The modes every adapter took before `auto`: what a host that reports
    /// no `permission_modes` (2.1 and older) supports.
    pub const LEGACY: [Self; 4] = [
        Self::Default,
        Self::AcceptEdits,
        Self::Plan,
        Self::BypassPermissions,
    ];

    /// [`Self::LEGACY`] as a `Vec`, the serde default of a capability report.
    pub fn legacy() -> Vec<Self> {
        Self::LEGACY.to_vec()
    }

    /// Sun's name of the mode, as in [`PERMISSION_MODES`].
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::AcceptEdits => "accept_edits",
            Self::Plan => "plan",
            Self::Auto => "auto",
            Self::BypassPermissions => "bypass_permissions",
        }
    }

    /// Parses an optional mode (a `SessionSpec`'s):
    /// `None` stays `None`, anything else must be an exact name.
    pub fn parse_opt(mode: Option<&str>) -> std::result::Result<Option<Self>, String> {
        mode.map(str::parse).transpose()
    }
}

impl std::str::FromStr for PermissionMode {
    type Err = String;

    /// Exact names only: no case folding, trimming or CLI spellings.
    fn from_str(s: &str) -> std::result::Result<Self, String> {
        Self::ALL
            .into_iter()
            .find(|mode| mode.as_str() == s)
            .ok_or_else(|| {
                format!(
                    "`{s}` is not a permission mode; use {}",
                    PERMISSION_MODES.join(", ")
                )
            })
    }
}

impl std::fmt::Display for PermissionMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why a session in permission `mode` may not run in `env`, if it may not:
/// `bypass_permissions` runs only in a `container` or `vm`, never bare or in
/// an extension env. The one owner of the rule, checked at placement (the
/// engine) and again by the host before it starts the CLI.
pub fn bypass_refusal(mode: Option<PermissionMode>, env: &ExecEnvSpec) -> Option<String> {
    let isolated = matches!(env, ExecEnvSpec::Container(_) | ExecEnvSpec::Vm(_));
    (mode == Some(PermissionMode::BypassPermissions) && !isolated).then(|| {
        format!(
            "bypass_permissions needs a container or vm exec env, and this session's is `{}`",
            env.kind()
        )
    })
}

/// [`bypass_refusal`] for a mode not parsed yet (a session's): a
/// spelling outside [`PERMISSION_MODES`] is refused too, whatever the env.
pub fn mode_refusal(mode: Option<&str>, env: &ExecEnvSpec) -> Option<String> {
    match PermissionMode::parse_opt(mode) {
        Ok(mode) => bypass_refusal(mode, env),
        Err(why) => Some(why),
    }
}

/// Why `tool`, which reports it can enforce `offered`, may not run a
/// session in `mode`, if it may not (BLO-168): a mode the CLI has no exact
/// equivalent for is refused, never run as another. The one owner of the
/// rule, checked at placement, at dispatch and by the host before it starts
/// the CLI.
pub fn unoffered_mode(
    tool: &str,
    mode: PermissionMode,
    offered: &[PermissionMode],
) -> Option<String> {
    (!offered.contains(&mode)).then(|| {
        let offered: Vec<&str> = offered.iter().map(|m| m.as_str()).collect();
        format!(
            "{tool} {}; it offers {}",
            unoffered(mode),
            offered.join(", ")
        )
    })
}

/// Whether a host's refusal `why` is [`unoffered_mode`]'s for `mode`
/// (BLO-178): the mode itself was refused, not the session for another
/// reason.
pub fn refuses_mode(why: &str, mode: PermissionMode) -> bool {
    why.contains(&unoffered(mode))
}

/// The words of [`unoffered_mode`] that name `mode`.
fn unoffered(mode: PermissionMode) -> String {
    format!("doesn't offer permission mode `{mode}`")
}

/// The env var that carries a `byo_key` runtime's key value into the session
/// (`SessionSpec::env`, so it is redacted from every event).
pub const BYO_KEY_ENV: &str = "SUN_BYO_KEY";

/// A `byo_key` runtime source: a provider API (Anthropic,
/// Google, or any OpenAI-compatible endpoint such as OpenAI, OpenRouter or a
/// local Ollama at `http://localhost:11434/v1`), a model and the *name* of
/// the ecosystem secret holding the key (stored as `runtime.config_json`;
/// the value stays in `secret`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ByoKey {
    /// Which provider API the endpoint speaks; `openai_compatible` when absent.
    #[serde(default)]
    pub kind: ByoKeyKind,
    /// Base URL of the API, e.g. `http://localhost:11434/v1`. Required for
    /// `openai_compatible`; empty means the provider's own for the others.
    #[serde(default)]
    pub endpoint: String,
    /// Model id at that endpoint, e.g. `qwen2.5-coder:7b`.
    pub model: String,
    /// Name of the secret holding the API key; `None` for keyless endpoints
    /// such as a local Ollama.
    #[serde(default)]
    pub key_secret: Option<String>,
}

impl ByoKey {
    /// Checks the source. The message starts with the field to fix
    /// (`endpoint: `, `model: `, `key_secret: `) and never holds a key.
    pub fn validate(&self) -> Result<()> {
        let invalid = |msg: &str| Err(crate::Error::InvalidInput(msg.into()));
        let endpoint = self.endpoint.trim();
        if endpoint.is_empty() && self.kind != ByoKeyKind::OpenaiCompatible {
            // The provider's own endpoint.
        } else if endpoint.is_empty() {
            return invalid(
                "endpoint: no endpoint; set an OpenAI-compatible base URL, \
                 e.g. http://localhost:11434/v1",
            );
        }
        let authority = endpoint
            .strip_prefix("https://")
            .or_else(|| endpoint.strip_prefix("http://"))
            .and_then(|rest| rest.split('/').next())
            .unwrap_or_default();
        // `@` userinfo or a `?key=` query would put a key in the URL, which
        // every credential response returns and logs keep.
        if !endpoint.is_empty()
            && (authority.is_empty()
                || authority.contains('@')
                || endpoint.contains(['?', '#'])
                || endpoint.contains(char::is_whitespace))
        {
            return invalid("endpoint: must be an http(s) URL without credentials or a query");
        }
        if self.model.trim().is_empty() || self.model.contains(char::is_whitespace) {
            return invalid("model: needs a model id without spaces, e.g. qwen2.5-coder:7b");
        }
        // Upper case only: a key pasted here instead of its secret's name
        // (`gsk_…`, `sk-…`) nearly always has lower case, and the message
        // must not repeat it.
        let env_name = |n: &str| {
            (1..=64).contains(&n.len())
                && !n.starts_with(|c: char| c.is_ascii_digit())
                && n.chars()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
        };
        match &self.key_secret {
            Some(name) if !env_name(name) => invalid(
                "key_secret: the secret's name, not the key: 1–64 of A-Z, 0-9 or _, \
                 not starting with a digit, e.g. OPENROUTER_KEY",
            ),
            _ => Ok(()),
        }
    }
}

string_enum! {
    /// The provider API a [`ByoKey`] endpoint speaks.
    #[derive(Default)]
    pub enum ByoKeyKind {
        /// Anthropic's Messages API (Claude Code, OpenCode).
        Anthropic => "anthropic",
        /// Google's Gemini API (OpenCode).
        Google => "google",
        /// The OpenAI chat API, as OpenAI, OpenRouter or Ollama serve it
        /// (Codex, OpenCode).
        #[default]
        OpenaiCompatible => "openai_compatible",
    }
}

impl ByoKeyKind {
    /// Whether runtime adapter `adapter` can use this kind of key.
    pub fn runs_on(self, adapter: &str) -> bool {
        match adapter {
            "opencode" => true,
            "claude-code" => self == Self::Anthropic,
            "codex" => self == Self::OpenaiCompatible,
            _ => false,
        }
    }
}

/// The execution environment a session runs in, picked by the
/// workflow or task. Wire form: `{"kind":"bare"}`,
/// `{"kind":"container","image":…,…}`, `{"kind":"vm","template":…,…}` or
/// `{"kind":"extension","name":"<mod>.<kind>","config":…}`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ExecEnvSpec {
    /// Directly in the worktree on the host (the default).
    #[default]
    Bare,
    /// In a docker/podman container with the worktree bind-mounted.
    Container(ContainerSpec),
    /// In a Linux VM (Lima: Virtualization.framework on macOS, QEMU/KVM on
    /// Linux) with the worktree mounted; the agent has root inside.
    Vm(VmSpec),
    /// In an environment an extension serves (`provides.exec_envs`, mod API
    /// 1.2). Only a host in the kernel's process (`sun serve --with-host`)
    /// reaches mods, so only it reports these kinds.
    Extension(ExtensionEnvSpec),
}

/// An extension's environment: which one, and its own settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtensionEnvSpec {
    /// `<mod>.<kind>`, e.g. `firecracker.microvm`; also its capability kind.
    pub name: String,
    /// Settings passed to the extension's `exec_env.prepare` as is.
    #[serde(default)]
    pub config: serde_json::Value,
}

impl ExtensionEnvSpec {
    /// The mod and the kind it declared, split from [`Self::name`].
    pub fn split(&self) -> Option<(&str, &str)> {
        self.name.split_once('.')
    }
}

/// A container environment: image and resource caps.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContainerSpec {
    /// Image, e.g. `ghcr.io/acme/toolchain:1.4`; a repo's
    /// `.sun/exec_env.yaml` `image:` replaces the workflow's. It must
    /// contain the agent CLI.
    pub image: String,
    /// Memory cap in MiB (`--memory`).
    #[serde(default)]
    pub memory_mb: Option<u64>,
    /// CPU cap in thousandths of a CPU (`--cpus`), e.g. 1500 = 1.5 CPUs.
    #[serde(default)]
    pub cpu_millis: Option<u32>,
    /// Docker network (`--network`); `none` cuts the network off. `None`
    /// uses the engine's default bridge.
    #[serde(default)]
    pub network: Option<String>,
}

/// A VM environment: template and resource caps.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VmSpec {
    /// Lima template name, e.g. `ubuntu-lts`; a repo's
    /// `.sun/exec_env.yaml` `template:` replaces the workflow's. The VM it
    /// makes must contain the agent CLI (or install it with root).
    pub template: String,
    /// Memory cap in MiB.
    #[serde(default)]
    pub memory_mb: Option<u64>,
    /// Number of virtual CPUs.
    #[serde(default)]
    pub cpus: Option<u32>,
    /// Outbound network allowlist: host names the VM may reach (any port).
    /// `None` leaves the network open. With a list, the VM's only exit is a
    /// host-side CONNECT proxy to these names (QEMU `restrict=on` netdev),
    /// so root inside can't widen it; the agent keeps root (a marketplace
    /// host takes it away).
    #[serde(default)]
    pub network: Option<Vec<String>>,
    /// Keep the VM, stopped, when the session's env is released (a snapshot
    /// to inspect or resume by hand). `false` (the default) deletes it, so
    /// every task starts from a fresh template (reset per task). A kept VM
    /// with a [`Self::network`] allowlist is locked against a hand restart,
    /// which would boot it without the host-side filter.
    #[serde(default)]
    pub keep: bool,
}

impl ExecEnvSpec {
    /// `true` for [`ExecEnvSpec::Bare`] (serde `skip_serializing_if`).
    pub fn is_bare(&self) -> bool {
        *self == Self::Bare
    }

    /// What a host reports ready as `ExecEnvCapability::kind`: the wire tag
    /// `bare`, `container` or `vm`, or an extension env's `<mod>.<kind>`.
    pub fn kind(&self) -> &str {
        match self {
            Self::Bare => "bare",
            Self::Container(_) => "container",
            Self::Vm(_) => "vm",
            Self::Extension(e) => &e.name,
        }
    }

    /// Rejects names that could turn into a CLI option or path trick, and
    /// caps of 0. Checked by the kernel on input and again by the host.
    pub fn validate(&self) -> crate::Result<()> {
        let bad = |what: &str, value: &str| {
            Err(crate::Error::InvalidInput(format!("bad {what} `{value}`")))
        };
        match self {
            Self::Bare => {}
            Self::Container(c) => {
                if !plain_name(&c.image, "/:@") {
                    return bad("container image", &c.image);
                }
                if let Some(network) = c.network.as_deref().filter(|n| !plain_name(n, "")) {
                    return bad("container network", network);
                }
                if c.memory_mb == Some(0) || c.cpu_millis == Some(0) {
                    return bad("container cap", "0");
                }
            }
            Self::Vm(vm) => {
                if !plain_name(&vm.template, "/") {
                    return bad("vm template", &vm.template);
                }
                if vm.memory_mb == Some(0) || vm.cpus == Some(0) {
                    return bad("vm cap", "0");
                }
            }
            Self::Extension(e) => {
                let ok = e.split().is_some_and(|(m, kind)| {
                    plain_name(m, "") && plain_name(kind, "") && !kind.contains('.')
                });
                if !ok {
                    return bad("extension env (want <mod>.<kind>)", &e.name);
                }
            }
        }
        Ok(())
    }
}

/// `true` when `value` can't turn into a CLI option or path trick: ASCII
/// alphanumerics, `._-` and `extra`, not starting with `-./:`, no `..`.
fn plain_name(value: &str, extra: &str) -> bool {
    !value.is_empty()
        && value.len() <= 255
        && !value.starts_with(['-', '.', '/', ':'])
        && !value.contains("..")
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c) || extra.contains(c))
}

/// Env var an exec environment sets in `SessionSpec::env` to the dir holding
/// its agent CLI shims; adapters must start `<dir>/<binary file name>` instead
/// of their binary when it is set, so the CLI never runs on the host.
pub const EXEC_SHIMS_ENV: &str = "SUN_EXEC_SHIMS";

/// File in the ecosystem skill store (`<data>/skills/`) mapping a skill to
/// the runtime adapter ids that get it (`{"tdd": ["claude-code"]}`); a skill
/// it doesn't list goes to every runtime. `sun-catalog` writes it, the
/// host's skills sync reads it.
pub const SKILL_RUNTIMES_FILE: &str = ".runtimes.json";

/// Where runtime adapter `adapter_id` reads project skills, relative to the
/// repo: the folder Sun writes first, then the other folders the CLI also
/// scans (from each CLI's docs). Empty for a runtime without skills.
pub fn skill_folders(adapter_id: &str) -> &'static [&'static str] {
    match adapter_id {
        "claude-code" => &[".claude/skills"],
        "codex" => &[".agents/skills"],
        "opencode" => &[".opencode/skills", ".claude/skills", ".agents/skills"],
        _ => &[],
    }
}

/// The folders a skill committed to a repo goes in so each of `runtimes`
/// (adapter ids; `None` = every runtime with a skills folder) sees it once.
/// Unknown ids add nothing.
pub fn repo_skill_dirs(runtimes: Option<&[String]>) -> Vec<&'static str> {
    let mut ids: Vec<&str> = match runtimes {
        Some(ids) => ids.iter().map(String::as_str).collect(),
        None => vec!["claude-code", "codex", "opencode"],
    };
    // A runtime that also scans other folders (OpenCode) goes last and reuses one.
    ids.sort_by_key(|id| skill_folders(id).len());
    let mut dirs: Vec<&'static str> = Vec::new();
    for id in ids {
        let folders = skill_folders(id);
        if let Some(native) = folders.first()
            && !folders.iter().any(|f| dirs.contains(f))
        {
            dirs.push(native);
        }
    }
    dirs
}

/// A started session.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionHandle {
    /// Sun's session id.
    pub session_id: SessionId,
    /// The CLI's own session id, when it has one (used to resume).
    pub native_id: Option<String>,
    /// OS process id, when the session runs a local process.
    pub pid: Option<u32>,
}

/// A message to steer a running agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Steer {
    /// The message text.
    pub text: String,
}

string_enum! {
    /// How to stop a session.
    pub enum StopMode {
        /// Ask the CLI to finish and exit (interrupt, then wait).
        Graceful => "graceful",
        /// Kill the process now.
        Kill => "kill",
    }
}

/// A session transcript for hand-off.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Transcript {
    /// The normalized events of the session.
    pub events: Vec<SunEvent>,
    /// The CLI's native transcript bytes, when the CLI can resume from them.
    pub native: Option<Vec<u8>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bypass_runs_only_in_a_container_or_vm() {
        let container = ExecEnvSpec::Container(ContainerSpec {
            image: "img".into(),
            memory_mb: None,
            cpu_millis: None,
            network: None,
        });
        let bypass = Some(PermissionMode::BypassPermissions);
        assert_eq!(bypass_refusal(bypass, &container), None);
        let bare = bypass_refusal(bypass, &ExecEnvSpec::Bare).unwrap();
        assert!(bare.contains("`bare`"), "{bare}");
        // `auto` is not a bypass: it runs bare.
        let others = [
            PermissionMode::AcceptEdits,
            PermissionMode::Plan,
            PermissionMode::Default,
            PermissionMode::Auto,
        ];
        for mode in others.map(Some).into_iter().chain([None]) {
            assert_eq!(bypass_refusal(mode, &ExecEnvSpec::Bare), None);
        }
    }

    #[test]
    fn a_permission_mode_has_one_spelling() {
        let names: Vec<&str> = PermissionMode::ALL.map(PermissionMode::as_str).into();
        assert_eq!(names, PERMISSION_MODES);
        for mode in PermissionMode::ALL {
            assert_eq!(mode.as_str().parse(), Ok(mode));
            let json = serde_json::to_value(mode).unwrap();
            assert_eq!(json, mode.as_str());
        }
        for forged in [
            "bypassPermissions",
            "BYPASS_PERMISSIONS",
            " bypass_permissions",
            "",
        ] {
            let err = forged.parse::<PermissionMode>().unwrap_err();
            assert!(err.contains("is not a permission mode"), "{err}");
            // Refused at placement whatever the env.
            assert!(mode_refusal(Some(forged), &ExecEnvSpec::Bare).is_some());
        }
        assert_eq!(mode_refusal(None, &ExecEnvSpec::Bare), None);
        assert_eq!(mode_refusal(Some("accept_edits"), &ExecEnvSpec::Bare), None);
    }
}
