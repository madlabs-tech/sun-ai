//! Each run's base commits (eval fidelity): once a run's worktree is
//! prepared, the commit each repo's worktree forked from its base branch is
//! journaled as `run_base` `{repos: [{url, sha}]}` ([`read`]). Eval replays
//! move their fresh worktrees to it ([`reset`]), so a replay never sees work
//! merged into the base after the original ran.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sun_core::host_protocol::{ExecRequest, Worktree};
use sun_core::{Error, HostId, HostLink, Result};

use crate::gates::quote;

/// Event kind; payload `{repos: [RepoBase]}`.
pub(crate) const RUN_BASE: &str = "run_base";
const TIMEOUT_S: u64 = 60;

/// The commit one repo's worktree was cut from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RepoBase {
    /// The repo's clone URL.
    pub url: String,
    /// Full commit id.
    pub sha: String,
}

/// Each repo worktree of `worktree` prepared at `root`: dir, url, base branch.
fn repos(worktree: &Worktree, root: &Path) -> Vec<(PathBuf, String, Option<String>)> {
    match worktree {
        Worktree::Folder { .. } => Vec::new(),
        Worktree::Repo { url, base, .. } => vec![(root.to_owned(), url.clone(), base.clone())],
        Worktree::Repos { repos, .. } => repos
            .iter()
            .map(|r| (root.join(&r.name), r.url.clone(), r.base.clone()))
            .collect(),
    }
}

/// A full hex commit id (SHA-1 or SHA-256): safe to put in a command.
fn is_sha(text: &str) -> bool {
    matches!(text.len(), 40 | 64) && text.chars().all(|c| c.is_ascii_hexdigit())
}

/// The fork point of each repo's worktree from its base branch
/// (`git merge-base`, so a retry on an older task branch still reads the
/// original base). Repos the host can't tell are left out.
pub(crate) async fn read(
    hosts: &dyn HostLink,
    host: HostId,
    worktree: &Worktree,
    root: &Path,
) -> Vec<RepoBase> {
    let mut out = Vec::new();
    for (cwd, url, base) in repos(worktree, root) {
        let Some(base) = base else { continue };
        let command = format!(
            "git merge-base HEAD {}",
            quote(&format!("refs/remotes/origin/{base}"))
        );
        let request = ExecRequest {
            cwd,
            command,
            env: Default::default(),
            timeout_s: TIMEOUT_S,
        };
        let Ok(output) = hosts.exec(host, request).await else {
            continue;
        };
        let sha = output.output.trim();
        if output.code == Some(0) && is_sha(sha) {
            out.push(RepoBase {
                url,
                sha: sha.to_owned(),
            });
        }
    }
    out
}

/// Moves each repo's freshly cut worktree to its commit in `bases` (by
/// url). A repo without one stays on its base branch's tip.
pub(crate) async fn reset(
    hosts: &dyn HostLink,
    host: HostId,
    worktree: &Worktree,
    root: &Path,
    bases: &HashMap<String, String>,
) -> Result<()> {
    for (cwd, url, _) in repos(worktree, root) {
        let Some(sha) = bases.get(&url).filter(|s| is_sha(s)) else {
            continue;
        };
        let request = ExecRequest {
            cwd,
            command: format!("git reset -q --hard {sha}"),
            env: Default::default(),
            timeout_s: TIMEOUT_S,
        };
        let output = hosts.exec(host, request).await?;
        if output.code != Some(0) {
            return Err(Error::Unavailable(format!(
                "{url}: the original base commit {sha} is gone: {}",
                output.output.trim()
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eval_base_shas_are_full_hex_only() {
        assert!(is_sha(&"a".repeat(40)));
        assert!(is_sha(&"0f".repeat(32)));
        assert!(!is_sha("main"));
        assert!(!is_sha(&format!("{};rm", "a".repeat(37))));
    }
}
