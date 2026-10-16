//! `GET /api/v1/runtimes` (v2, TRD BLO-117 §6): one entry per `(host, tool)`
//! with `presence`, `availability`, `load` and the derived `state`
//! ([`RuntimeState`]), then the workspace's key-backed runtimes in the same
//! shape. Two hosts with the same CLI are two entries.
//!
//! Read from each host's stored capabilities and the live hub.
// ponytail: reads `host.capabilities` until the `host_runtime` projection
// (TRD item 3) lands; then `id` becomes its row id and `probed_at` is filled.

use std::collections::BTreeMap;

use serde_json::{Value, json};
use sun_core::model::{Host, Runtime, RuntimeSource};
use sun_core::{Availability, HostLink, Presence, Readiness, RuntimeState, Timestamp, WorkspaceId};

use crate::error::ApiResult;
use crate::hosts::{online_adapters, runtime_readiness};
use crate::{Api, wire};

/// Every entry of `workspace` (`None`: every workspace's).
pub(crate) async fn list(api: &Api, workspace: Option<WorkspaceId>) -> ApiResult<Vec<Value>> {
    let cooling = api.engine.cooldowns().await?;
    let modes: BTreeMap<String, String> =
        api.engine.permission_modes().await?.into_iter().collect();
    let active = api.hosts.active_sessions();
    let mut entries = Vec::new();
    for host in api.engine.hosts(workspace).await? {
        let presence = if api.hosts.is_online(host.id) {
            Presence::Online
        } else {
            Presence::Offline
        };
        let last_seen = api
            .hosts
            .last_seen(host.id)
            .unwrap_or_else(|| wire::created_at(&host.id.to_string()));
        for (tool, readiness) in installed(&host) {
            let cooldown_until = cooling.get(&tool).copied();
            let load = active.get(&(host.id, tool.clone())).copied().unwrap_or(0);
            let observed = Observed {
                presence,
                availability: Availability::of(&readiness, cooldown_until.is_some()),
                active: load,
                cooldown_until,
            };
            let detail = host_detail(&observed, &host.name, &readiness);
            let mut entry = observed.view(&tool, readiness.version.as_deref(), detail);
            entry["id"] = json!(format!("{}:{tool}", host.id));
            entry["host"] = json!({ "id": host.id, "name": host.name });
            entry["load"] = json!({ "active": load, "max": null });
            entry["last_seen_at"] = json!(last_seen);
            entry["name"] = json!(tool);
            entry["source"] = json!(RuntimeSource::ByoRuntime);
            entry["workspace_id"] = Value::Null;
            entry["permission_mode"] = json!(modes.get(&tool));
            entry["allow_bypass"] = json!(api.allow_bypass);
            entries.push(entry);
        }
    }
    // Same tool side by side, then by host.
    entries.sort_by(|a, b| {
        (a["tool"].as_str(), a["host"]["name"].as_str())
            .cmp(&(b["tool"].as_str(), b["host"]["name"].as_str()))
    });
    let online = online_adapters(api, workspace).await?;
    for runtime in api.engine.runtimes(workspace).await? {
        if runtime.source == RuntimeSource::ByoRuntime {
            continue;
        }
        let entry = keyed(api, &runtime, &online, &cooling, &modes).await?;
        entries.push(entry);
    }
    Ok(entries)
}

/// The facts a runtime's `state` is derived from.
struct Observed {
    presence: Presence,
    availability: Availability,
    active: u32,
    cooldown_until: Option<Timestamp>,
}

impl Observed {
    fn state(&self) -> RuntimeState {
        RuntimeState::derive(self.presence, self.availability, self.active)
    }

    /// The fields every entry has; the caller adds `id`, `host` and the rest.
    fn view(&self, tool: &str, version: Option<&str>, detail: Option<String>) -> Value {
        json!({
            "tool": tool, "adapter": tool, "family": null, "version": version,
            "host": null,
            "presence": self.presence, "availability": self.availability,
            "load": null, "state": self.state(), "detail": detail,
            "cooldown_until": self.cooldown_until,
            "probed_at": null, "last_seen_at": null,
            "config": null, "model": null,
        })
    }
}

/// `(tool, readiness)` of each CLI `host` reported installed in its last
/// `hello`/`capabilities`. A tool it has no binary for is not a runtime.
fn installed(host: &Host) -> Vec<(String, Readiness)> {
    let reported = host.capabilities.get("runtimes").and_then(Value::as_array);
    reported
        .into_iter()
        .flatten()
        .filter_map(|r| {
            let tool = r.get("adapter")?.as_str()?.to_owned();
            let readiness: Readiness = serde_json::from_value(r.get("readiness")?.clone()).ok()?;
            readiness.installed.then_some((tool, readiness))
        })
        .collect()
}

/// The line under a host runtime's badge, from what the host reported.
fn host_detail(observed: &Observed, host: &str, readiness: &Readiness) -> Option<String> {
    match observed.state() {
        RuntimeState::Offline => Some(format!("{host} is offline")),
        RuntimeState::NeedsLogin => {
            Some(match readiness.detail.as_deref().and_then(login_command) {
                Some(command) => format!("Run `{command}` on {host}"),
                None => readiness
                    .detail
                    .clone()
                    .unwrap_or_else(|| format!("Log in on {host}")),
            })
        }
        RuntimeState::Limited => observed
            .cooldown_until
            .map(|until| format!("Usage limit reached; back at {}", until.to_rfc3339())),
        // The version warning on a ready CLI (`outdated`), else nothing.
        RuntimeState::Unsupported | RuntimeState::Busy | RuntimeState::Idle => {
            readiness.detail.clone()
        }
    }
}

/// The command in a probe's "… is not logged in. Run: <command>" detail.
fn login_command(detail: &str) -> Option<&str> {
    let (_, command) = detail.rsplit_once("Run: ")?;
    let command = command.trim().trim_end_matches('.');
    (!command.is_empty()).then_some(command)
}

/// A `byo_key` or `credits` runtime: it runs on any online host with its
/// CLI, so `host` is `null`, `presence` is whether one is online, and
/// `load` is `null` (sessions are counted per CLI, not per key).
// ponytail: these become credentials (TRD item 7) and leave this list.
async fn keyed(
    api: &Api,
    runtime: &Runtime,
    online: &BTreeMap<String, (bool, bool)>,
    cooling: &BTreeMap<String, Timestamp>,
    modes: &BTreeMap<String, String>,
) -> ApiResult<Value> {
    let ready = runtime_readiness(api, runtime, online).await?;
    let cooldown_until = cooling.get(&runtime.name).copied();
    let presence = if ready.installed {
        Presence::Online
    } else {
        Presence::Offline
    };
    let availability = match runtime.source {
        RuntimeSource::Credits if !ready.logged_in => Availability::Unsupported,
        _ if !ready.logged_in => Availability::NeedsLogin,
        _ if cooldown_until.is_some() => Availability::Limited,
        _ => Availability::Ready,
    };
    let observed = Observed {
        presence,
        availability,
        active: 0,
        cooldown_until,
    };
    let detail = match observed.state() {
        RuntimeState::Limited => cooldown_until
            .map(|until| format!("Usage limit reached; back at {}", until.to_rfc3339())),
        _ => ready.detail.clone(),
    };
    let mut entry = observed.view(&runtime.adapter, None, detail);
    entry["id"] = json!(runtime.name);
    entry["name"] = json!(runtime.name);
    entry["source"] = json!(runtime.source);
    entry["workspace_id"] = json!(runtime.workspace_id);
    entry["permission_mode"] = json!(modes.get(&runtime.adapter));
    entry["allow_bypass"] = json!(api.allow_bypass);
    entry["config"] = json!(runtime.byo_key);
    if runtime.source == RuntimeSource::Credits {
        entry["model"] = json!(api.engine.credits_model(runtime.id).await?);
    }
    Ok(entry)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_command_is_read_from_the_probe_detail() {
        let claude = "Claude Code is not logged in. Run: claude auth login";
        assert_eq!(login_command(claude), Some("claude auth login"));
        assert_eq!(login_command("Run: codex login."), Some("codex login"));
        assert_eq!(login_command("extension x: not running"), None);
        assert_eq!(login_command("Run: "), None);
    }
}
