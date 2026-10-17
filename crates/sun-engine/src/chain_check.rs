//! Save-time checks of a chain's `{tool, credential, host, model, effort}`
//! entries against the workspace (TRD BLO-117 FR6, §15 item 8). The YAML
//! parser checks their shape; only here are the workspace's key-backed
//! runtimes and hosts known. Bare names are not checked here: they resolve
//! as they always did (market, tool id, then key-backed runtime name).
//!
//! - `credential` names one of the workspace's `byo_key`/`credits` runtimes
//!   (they keep their names when they become credentials), whose adapter is
//!   `tool`.
//! - `host` is a host attached to the workspace that reported `tool`.
//! - `effort` is a level the host's catalog lists for `tool` and `model`
//!   (BLO-119): the pinned host's, else any attached host's. The kernel
//!   checks it again at session start against the host that runs it.
//!
//! - a `permission_mode` (an agent's, a workflow step's) is one every tool
//!   of the chain reports it can enforce (BLO-163) on some host, the pinned
//!   one for a pinned entry ([`check_mode`]).
//!
//! Errors name the field: `runtime_chain[1].effort: ...`.

use sun_core::model::{Host, RuntimeSource};
use sun_core::{ChainEntry, ChainSpec, Error, PermissionMode, Result, WorkspaceId};
use sun_store::{HostRepo, RuntimeRepo, Tx};

use crate::executor::failover::Reported;
use crate::executor::runtime_rows;

/// Checks every object entry of `chain` against `workspace`; `field` is how
/// the caller names the chain in errors (`runtime_chain`, `runtime.chain`).
pub(crate) async fn check(
    tx: &mut Tx,
    workspace: WorkspaceId,
    chain: &[ChainEntry],
    field: &str,
) -> Result<()> {
    let specs: Vec<(usize, &ChainSpec)> = chain
        .iter()
        .enumerate()
        .filter_map(|(i, e)| e.spec().map(|s| (i, s)))
        .collect();
    if specs.is_empty() {
        return Ok(());
    }
    let hosts = tx.hosts_in(workspace).await?;
    let rows = runtime_rows(tx).await?;
    for (i, spec) in specs {
        let at = |name: &str| format!("{field}[{i}].{name}");
        let invalid = |name: &str, why: String| Error::InvalidInput(format!("{}: {why}", at(name)));
        let tool = &spec.tool;
        let keyed = |row: &sun_core::model::Runtime| row.source != RuntimeSource::ByoRuntime;
        match &spec.credential {
            Some(name) => match tx.runtime_by_name(workspace, name).await? {
                Some(row) if keyed(&row) && row.adapter == *tool => {}
                Some(row) if keyed(&row) => {
                    let why = format!("`{name}` runs on {}, not {tool}", row.adapter);
                    return Err(invalid("credential", why));
                }
                _ => {
                    let why =
                        format!("this workspace has no key or credits runtime named `{name}`");
                    return Err(invalid("credential", why));
                }
            },
            // Without a credential the entry means the CLI's own login; a
            // key-backed runtime of the same name would silently pay instead.
            None => {
                if let Some(row) = tx.runtime_by_name(workspace, tool).await?
                    && keyed(&row)
                {
                    let why = format!(
                        "this workspace has a key runtime named `{tool}`; set `credential: {tool}` to use it"
                    );
                    return Err(invalid("tool", why));
                }
            }
        }
        let has_tool =
            |host: &Host| Reported::of(host, rows.as_deref()).is_some_and(|r| r.has(tool));
        let candidates: Vec<&Host> = match &spec.host {
            Some(pin) => {
                let Some(host) = hosts.iter().find(|h| h.id.to_string() == *pin) else {
                    let why = format!("no host `{pin}` is attached to this workspace");
                    return Err(invalid("host", why));
                };
                if !has_tool(host) {
                    let why = format!("{} has not reported {tool}", host.name);
                    return Err(invalid("host", why));
                }
                vec![host]
            }
            None => hosts.iter().filter(|h| has_tool(h)).collect(),
        };
        if let Some(effort) = &spec.effort {
            let model = spec.model.as_deref();
            let offered = candidates
                .iter()
                .any(|h| h.effort_catalog(tool).allows(model, effort));
            if !offered {
                let on = match &spec.host {
                    Some(_) => candidates
                        .first()
                        .map_or("", |h| h.name.as_str())
                        .to_owned(),
                    None => "any host of this workspace".to_owned(),
                };
                let model = model.map(|m| format!(" with {m}")).unwrap_or_default();
                let why = format!("`{effort}` is not an effort level {tool}{model} offers on {on}");
                return Err(invalid("effort", why));
            }
        }
    }
    Ok(())
}

/// Checks that every tool of `chain` can run in `mode` (`field` names the
/// mode in errors): some attached host (the pinned one, for a pinned entry)
/// reports the mode for the tool. A tool no attached host reported, or a
/// `market:` entry, has nothing to check against: the CLI itself refuses an
/// unknown mode at session start, so the run fails visibly.
pub(crate) async fn check_mode(
    tx: &mut Tx,
    workspace: WorkspaceId,
    chain: &[ChainEntry],
    mode: Option<PermissionMode>,
    field: &str,
) -> Result<()> {
    let Some(mode) = mode else {
        return Ok(());
    };
    let hosts = tx.hosts_in(workspace).await?;
    for entry in chain {
        let tool = match entry {
            ChainEntry::Spec(spec) => spec.tool.clone(),
            ChainEntry::Name(name) if name.starts_with(crate::market::MARKET_PREFIX) => continue,
            ChainEntry::Name(name) => match tx.runtime_by_name(workspace, name).await? {
                Some(row) => row.adapter,
                None => name.clone(),
            },
        };
        let reports: Vec<Vec<PermissionMode>> = hosts
            .iter()
            .filter(|h| entry.host().is_none_or(|pin| h.id.to_string() == pin))
            .filter_map(|h| h.permission_modes(&tool))
            .collect();
        if reports.is_empty() || reports.iter().any(|modes| modes.contains(&mode)) {
            continue;
        }
        let offered: Vec<&str> = PermissionMode::ALL
            .into_iter()
            .filter(|m| reports.iter().any(|modes| modes.contains(m)))
            .map(PermissionMode::as_str)
            .collect();
        return Err(Error::InvalidInput(format!(
            "{field}: {tool} can't run in `{mode}` on {}; it offers {}",
            match entry.host() {
                Some(_) => "its pinned host",
                None => "any host of this workspace",
            },
            offered.join(", ")
        )));
    }
    Ok(())
}
