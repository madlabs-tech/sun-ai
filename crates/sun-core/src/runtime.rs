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
/// (Claude Code's `--permission-mode` spelling lives only in its adapter;
/// OpenCode answers its permission requests by it).
/// `bypass_permissions` needs the operator's `allow_bypass`.
pub const PERMISSION_MODES: [&str; 4] = ["default", "accept_edits", "plan", "bypass_permissions"];

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
        // `@` would mean credentials in the URL, which end up in logs.
        if !endpoint.is_empty()
            && (authority.is_empty()
                || authority.contains('@')
                || endpoint.contains(char::is_whitespace))
        {
            return invalid("endpoint: must be an http(s) URL without credentials");
        }
        if self.model.trim().is_empty() || self.model.contains(char::is_whitespace) {
            return invalid("model: needs a model id without spaces, e.g. qwen2.5-coder:7b");
        }
        match &self.key_secret {
            Some(name) if crate::model::Secret::validate_name(name).is_err() => invalid(
                "key_secret: a secret name is 1–64 letters, digits or _, not starting with a digit",
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
