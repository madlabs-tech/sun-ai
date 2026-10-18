//! `GET /api/v1/runtimes` (v2, TRD BLO-117 §6): one entry per `(host, tool)`
//! with `presence`, `availability`, `load` and the derived `state`
//! ([`RuntimeState`]). Two hosts with the same CLI are two entries. The
//! workspace's credentials get the same `state` in `GET /credentials`
//! ([`credential_state`]).
//!
//! Each host's CLIs come from the source `sun.yaml` `runtimes.model` picks
//! ([`RuntimesModel`]): `v2` reads the `host_runtime` rows (row `id`,
//! `probed_at`), `v1` the raw capabilities report (`id` `<host>:<tool>`, no
//! `probed_at`). Presence and load come from the live hub. A `(host, tool)`
//! is `limited` while an account it would spend cools down: the host's own
//! login of the tool, or a credential on that tool of a workspace the host
//! serves. Each `(host, tool)` entry carries the effort levels and models the
//! host reported for the tool (`effort_options`, `models`) and the permission
//! modes the CLI there can enforce (`permission_modes`), which the agent form
//! offers.

use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, HashMap, HashSet};

use serde_json::{Value, json};
use sun_core::model::{Account, Credential, Host, HostRuntime, RuntimeSource};
use sun_core::{
    Availability, HostId, HostLink, Presence, Readiness, RuntimeState, Timestamp, WorkspaceId,
};

use crate::error::ApiResult;
use crate::hosts::credential_readiness;
use crate::{Api, RuntimesModel, wire};

/// Every entry of `workspace` (`None`: every workspace's).
pub(crate) async fn list(api: &Api, workspace: Option<WorkspaceId>) -> ApiResult<Vec<Value>> {
    let cooling = api.engine.cooldowns().await?;
    let active = api.hosts.active_sessions();
    let keys = api.engine.credentials(workspace).await?;
    // The hosts serving each workspace with a key.
    let mut serving: HashMap<WorkspaceId, HashSet<HostId>> = HashMap::new();
    for key in &keys {
        if let Entry::Vacant(entry) = serving.entry(key.workspace_id) {
            let hosts = api.engine.hosts(Some(key.workspace_id)).await?;
            entry.insert(hosts.iter().map(|h| h.id).collect());
        }
    }
    let mut projected: BTreeMap<_, Vec<HostRuntime>> = BTreeMap::new();
    if api.runtimes_model == RuntimesModel::V2 {
        for row in api.engine.host_runtimes(workspace).await? {
            projected.entry(row.host_id).or_default().push(row);
        }
    }
    let mut entries = Vec::new();
    for host in api.engine.hosts(workspace).await? {
        let presence = if api.hosts.is_online(host.id) {
            Presence::Online
        } else {
            Presence::Offline
        };
        let last_seen = api.hosts.last_seen(host.id);
        let reported = match api.runtimes_model {
            RuntimesModel::V1 => installed(&host),
            RuntimesModel::V2 => projected
                .remove(&host.id)
                .unwrap_or_default()
                .into_iter()
                .filter_map(from_row)
                .collect(),
        };
        for Reported {
            id,
            tool,
            readiness,
            probed_at,
        } in reported
        {
            let spends = keys.iter().filter(|key| {
                key.default_tool == tool
                    && serving
                        .get(&key.workspace_id)
                        .is_some_and(|hosts| hosts.contains(&host.id))
            });
            let accounts = spends
                .map(|key| Account::credential_key(key.id))
                .chain([Account::Login(tool.clone()).key(host.id)]);
            let cooldown_until = accounts.filter_map(|a| cooling.get(&a).copied()).max();
            let load = active.get(&(host.id, tool.clone())).copied().unwrap_or(0);
            let observed = Observed {
                presence,
                availability: Availability::of(&readiness, cooldown_until.is_some()),
                active: load,
                cooldown_until,
            };
            let detail = host_detail(&observed, &host.name, &readiness);
            let mut entry = observed.view(&tool, readiness.version.as_deref(), detail);
            entry["id"] = json!(id.unwrap_or_else(|| format!("{}:{tool}", host.id)));
            entry["probed_at"] = json!(probed_at);
            entry["host"] = json!({ "id": host.id, "name": host.name });
            entry["load"] = json!({ "active": load, "max": null });
            // Not seen since this kernel started: the row's last probe
            // (`v2`), else the enrolment (the host id's ULID time).
            let last_seen = last_seen
                .or(probed_at)
                .unwrap_or_else(|| wire::created_at(&host.id.to_string()));
            entry["last_seen_at"] = json!(last_seen);
            entry["name"] = json!(tool);
            entry["source"] = json!(RuntimeSource::ByoRuntime);
            entry["workspace_id"] = Value::Null;
            entry["allow_bypass"] = json!(api.allow_bypass);
            // What the agent form's model and effort pickers offer for
            // this tool on this host (BLO-119), from the raw report.
            let catalog = host.effort_catalog(&tool);
            entry["effort_options"] = json!(catalog.effort_options);
            entry["models"] = json!(catalog.models);
            entry["permission_modes"] = json!(host.permission_modes(&tool).unwrap_or_default());
            entries.push(entry);
        }
    }
    // Same tool side by side, then by host.
    entries.sort_by(|a, b| {
        (a["tool"].as_str(), a["host"]["name"].as_str())
            .cmp(&(b["tool"].as_str(), b["host"]["name"].as_str()))
    });
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
            "effort_options": [], "models": [], "permission_modes": null,
        })
    }
}

/// One CLI a host reported installed. `id` and `probed_at` are the
/// `host_runtime` row's (`v2`); `None` on the raw report (`v1`).
struct Reported {
    id: Option<String>,
    tool: String,
    readiness: Readiness,
    probed_at: Option<Timestamp>,
}

/// Each CLI `host` reported installed in its last `hello`/`capabilities`
/// (`v1`). A tool it has no binary for is not a runtime.
fn installed(host: &Host) -> Vec<Reported> {
    let reported = host.capabilities.get("runtimes").and_then(Value::as_array);
    reported
        .into_iter()
        .flatten()
        .filter_map(|r| {
            let tool = r.get("adapter")?.as_str()?.to_owned();
            let readiness: Readiness = serde_json::from_value(r.get("readiness")?.clone()).ok()?;
            readiness.installed.then_some(Reported {
                id: None,
                tool,
                readiness,
                probed_at: None,
            })
        })
        .collect()
}

/// A `host_runtime` row (`v2`) when its CLI is installed. `auth` other than
/// `logged_in` reads as logged out, as the raw report's `logged_in` does.
fn from_row(row: HostRuntime) -> Option<Reported> {
    row.installed.then(|| Reported {
        id: Some(row.id.to_string()),
        readiness: Readiness {
            installed: true,
            version: row.version,
            logged_in: row.auth == "logged_in",
            detail: row.detail,
        },
        tool: row.tool,
        probed_at: Some(row.probed_at),
    })
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

/// A credential's `state`, `detail` and `cooldown_until` (`GET
/// /credentials`): it pays on any online host with its tool, so it is
/// `offline` while none has it, `needs_login` while its settings or key
/// secret are missing, `unsupported` for credits this deployment doesn't
/// sell, and `limited` while its key cools down.
pub(crate) async fn credential_state(
    api: &Api,
    credential: &Credential,
    online: &BTreeMap<String, (bool, bool)>,
    cooling: &BTreeMap<String, Timestamp>,
) -> ApiResult<Value> {
    let ready = credential_readiness(api, credential, online).await?;
    let cooldown_until = cooling
        .get(&Account::credential_key(credential.id))
        .copied();
    let presence = if ready.installed {
        Presence::Online
    } else {
        Presence::Offline
    };
    let availability = match credential.source {
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
        _ => ready.detail,
    };
    Ok(json!({
        "state": observed.state(), "detail": detail, "cooldown_until": cooldown_until,
    }))
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
