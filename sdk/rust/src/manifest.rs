//! `mod.yaml` manifest types and the kind rules every mod must pass.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Kind of mod: plugins connect third parties, extensions add
/// engine capabilities.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModKind {
    /// Integration with an outside service (GitHub, Slack, ...).
    Plugin,
    /// Extends the host (step types, gates, ...).
    Extension,
}

/// A parsed `mod.yaml`. Unknown fields are rejected.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    /// Unique mod name, `[a-z][a-z0-9_-]{0,63}`.
    pub name: String,
    /// Plugin or extension.
    pub kind: ModKind,
    /// The mod's own version (semver).
    pub version: String,
    /// Sun mod API range it supports, e.g. `">=1.0 <2.0"`.
    pub api: String,
    /// How to start it.
    pub entry: Entry,
    /// What it offers.
    #[serde(default)]
    pub provides: Provides,
    /// What it needs.
    #[serde(default)]
    pub permissions: Permissions,
}

/// The command that starts the mod. A relative path containing `/` is
/// resolved against the mod's folder.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    /// Executable, e.g. `./github-mod`.
    pub command: String,
    /// Arguments.
    #[serde(default)]
    pub args: Vec<String>,
}

/// What a mod offers. Plugin-only: `actions`, `triggers`, `mcp_tools`.
/// Extension-only: `step_types`, `gates`, `hooks`, `metrics`,
/// `runtime_adapters`, `exec_envs`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Provides {
    /// Outbound actions callable with `call` (plugin).
    #[serde(default)]
    pub actions: Vec<String>,
    /// Triggers it can emit as `event` after `subscribe` (plugin).
    #[serde(default)]
    pub triggers: Vec<String>,
    /// Whether it also serves MCP tools (plugin).
    #[serde(default)]
    pub mcp_tools: bool,
    /// Workflow step types callable with `call` (extension).
    #[serde(default)]
    pub step_types: Vec<String>,
    /// Gates callable with `call` (extension).
    #[serde(default)]
    pub gates: Vec<String>,
    /// Run hooks (extension, API 1.2): the [`hook::EVENTS`](crate::hook::EVENTS)
    /// it gets as `call observe {events}`.
    #[serde(default)]
    pub hooks: Vec<String>,
    /// Metric names (extension, API 1.2): at each run end the kernel calls
    /// `metrics {run}` and journals each returned name as a `metric` event.
    #[serde(default)]
    pub metrics: Vec<String>,
    /// Runtime adapter ids it serves through the
    /// [`runtime_action`](crate::runtime_action)s (extension, API 1.2); Sun
    /// names each `<mod>.<id>`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub runtime_adapters: Vec<String>,
    /// Execution environment kinds it serves through the
    /// [`exec_env_action`](crate::exec_env_action)s (extension, API 1.2);
    /// workflows pick one as `exec_env: {kind: extension, name: <mod>.<kind>}`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exec_envs: Vec<String>,
}

/// What a mod needs. Plugins may not request `devices`; extensions may not
/// request `network` or `secrets` (they reach third parties through plugins).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Permissions {
    /// Network hosts (plugin).
    #[serde(default)]
    pub network: Vec<String>,
    /// Secret names (plugin).
    #[serde(default)]
    pub secrets: Vec<String>,
    /// Host devices, e.g. `kvm` (extension).
    #[serde(default)]
    pub devices: Vec<String>,
    /// File paths.
    #[serde(default)]
    pub paths: Vec<String>,
    /// Processes it may start.
    #[serde(default)]
    pub processes: Vec<String>,
    /// Plugins it depends on (extension).
    #[serde(default)]
    pub plugins: Vec<String>,
}

/// Why a manifest was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidManifest(pub String);

impl fmt::Display for InvalidManifest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid mod.yaml: {}", self.0)
    }
}

impl std::error::Error for InvalidManifest {}

impl Manifest {
    /// Checks the name, entry and kind rules. Version and API range are
    /// semver strings the kernel checks against the API it speaks.
    pub fn validate(&self) -> Result<(), InvalidManifest> {
        let bad = |msg: String| Err(InvalidManifest(msg));
        if !is_name(&self.name) {
            return bad(format!(
                "name {:?} must match [a-z][a-z0-9_-]{{0,63}}",
                self.name
            ));
        }
        if self.version.trim().is_empty() || self.api.trim().is_empty() {
            return bad("version and api are required".into());
        }
        if self.entry.command.trim().is_empty() {
            return bad("entry.command is required".into());
        }
        let (p, perms) = (&self.provides, &self.permissions);
        let (other, used) = match self.kind {
            ModKind::Plugin => (
                "extension",
                vec![
                    ("permissions.devices", !perms.devices.is_empty()),
                    ("provides.step_types", !p.step_types.is_empty()),
                    ("provides.gates", !p.gates.is_empty()),
                    ("provides.hooks", !p.hooks.is_empty()),
                    ("provides.metrics", !p.metrics.is_empty()),
                    (
                        "provides.runtime_adapters/exec_envs",
                        !p.runtime_adapters.is_empty() || !p.exec_envs.is_empty(),
                    ),
                ],
            ),
            ModKind::Extension => (
                "plugin",
                vec![
                    ("permissions.network", !perms.network.is_empty()),
                    ("permissions.secrets", !perms.secrets.is_empty()),
                    (
                        "provides.actions/triggers/mcp_tools",
                        !p.actions.is_empty() || !p.triggers.is_empty() || p.mcp_tools,
                    ),
                ],
            ),
        };
        if let Some((field, _)) = used.iter().find(|(_, set)| *set) {
            return bad(format!("{field} is {other}-only"));
        }
        if let Some(h) = p
            .hooks
            .iter()
            .find(|h| !crate::hook::EVENTS.contains(&h.as_str()))
        {
            return bad(format!(
                "provides.hooks: unknown event {h:?} (one of {})",
                crate::hook::EVENTS.join(", ")
            ));
        }
        if let Some(m) = p.metrics.iter().find(|m| !is_name(m)) {
            return bad(format!(
                "provides.metrics: {m:?} must match [a-z][a-z0-9_-]{{0,63}}"
            ));
        }
        if let Some(id) = p
            .runtime_adapters
            .iter()
            .chain(&p.exec_envs)
            .find(|id| !is_name(id))
        {
            return bad(format!(
                "runtime adapter or exec env id {id:?} must match [a-z][a-z0-9_-]{{0,63}}"
            ));
        }
        Ok(())
    }

    /// Whether `method` may be sent with `call` (an action, step type or
    /// gate, an [`mcp_action`](crate::mcp_action) when `mcp_tools` is set,
    /// [`hook::OBSERVE`](crate::hook::OBSERVE) with `hooks`,
    /// [`hook::METRICS`](crate::hook::METRICS) with `metrics`, a
    /// [`runtime_action`](crate::runtime_action) with `runtime_adapters` or an
    /// [`exec_env_action`](crate::exec_env_action) with `exec_envs`).
    pub fn is_callable(&self, method: &str) -> bool {
        let p = &self.provides;
        let mcp = [crate::mcp_action::TOOLS, crate::mcp_action::CALL];
        (p.mcp_tools && mcp.contains(&method))
            || (!p.runtime_adapters.is_empty() && crate::runtime_action::ALL.contains(&method))
            || (!p.exec_envs.is_empty() && crate::exec_env_action::ALL.contains(&method))
            || (!p.hooks.is_empty() && method == crate::hook::OBSERVE)
            || (!p.metrics.is_empty() && method == crate::hook::METRICS)
            || [&p.actions, &p.step_types, &p.gates]
                .iter()
                .any(|list| list.iter().any(|m| m == method))
    }
}

fn is_name(s: &str) -> bool {
    let mut chars = s.chars();
    s.len() <= 64
        && chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}
