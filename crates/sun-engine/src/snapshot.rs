//! Task-folder snapshots: what a repo-less task's steps leave in
//! its folder becomes versioned task artifacts.
//!
//! The executor runs a [`Job`] after each agent step of a task with no repo,
//! and before an `<plugin>.upload` action (so the delivery carries the files
//! as they are now). The job reads the folder through [`HostLink::exec`];
//! the executor stores each file as the next version of its artifact, and an
//! unchanged file is not a new version (`ArtifactRepo::add_artifact`).

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use sun_core::host_protocol::{ExecRequest, Worktree};
use sun_core::{HostId, HostLink};

use crate::gates::quote;

/// Most files one snapshot keeps.
const MAX_FILES: usize = 50;
/// Files under 16 MiB are kept (larger ones are skipped).
const LIST: &str = "find . -type f ! -path '*/.*' -size -16384k | LC_ALL=C sort";
/// Most bytes one snapshot reads; files past it are left out.
const MAX_TOTAL: usize = 64 * 1024 * 1024;
/// Bytes per read: exec keeps the last 64 KiB of output and base64 (with
/// its line breaks) grows a chunk by about 35%, so 40 KiB fits.
// ponytail: one exec per 40 KiB (26 for 1 MiB); add a host file-read frame
// if task folders hold many big files.
const CHUNK: usize = 40 * 1024;
const TIMEOUT_S: u64 = 30;

/// One snapshot, built in the engine and run off its task.
#[derive(Debug, Clone)]
pub(crate) struct Job {
    pub host: HostId,
    pub worktree: Worktree,
}

impl Job {
    /// The task folder's files as `(path, bytes)`, paths relative to it.
    /// Hidden files and folders are left out.
    pub(crate) async fn run(
        self,
        hosts: &dyn HostLink,
    ) -> std::result::Result<Vec<(String, Vec<u8>)>, String> {
        let dir = hosts
            .prepare_worktree(self.host, self.worktree)
            .await
            .map_err(|e| format!("task folder: {e}"))?;
        let exec = |command: String| {
            let request = ExecRequest {
                cwd: dir.clone(),
                command,
                env: Default::default(),
                timeout_s: TIMEOUT_S,
            };
            hosts.exec(self.host, request)
        };
        let listing = exec(LIST.to_owned())
            .await
            .map_err(|e| format!("task folder: {e}"))?;
        let mut files = Vec::new();
        let mut total = 0;
        for path in listing.output.lines().filter_map(relative).take(MAX_FILES) {
            let mut bytes = Vec::new();
            loop {
                let command = format!(
                    "[ -r {file} ] || exit 1; tail -c +{from} {file} | head -c {CHUNK} | base64",
                    file = quote(path),
                    from = bytes.len() + 1,
                );
                let out = exec(command)
                    .await
                    .map_err(|e| format!("reading `{path}`: {e}"))?;
                if out.code != Some(0) {
                    return Err(format!("reading `{path}`: {}", out.output.trim()));
                }
                let text: String = out.output.split_whitespace().collect();
                let chunk = BASE64
                    .decode(text)
                    .map_err(|e| format!("reading `{path}`: {e}"))?;
                bytes.extend_from_slice(&chunk);
                // A file still growing stops at the snapshot's cap.
                if chunk.len() < CHUNK || bytes.len() > MAX_TOTAL {
                    break;
                }
            }
            total += bytes.len();
            if total > MAX_TOTAL {
                break;
            }
            files.push((path.to_owned(), bytes));
        }
        Ok(files)
    }
}

/// `./out/a.md` → `out/a.md`; anything else `find` wouldn't print is dropped.
fn relative(line: &str) -> Option<&str> {
    let path = line.strip_prefix("./")?;
    (!path.is_empty() && !path.split('/').any(|part| part.is_empty() || part == ".."))
        .then_some(path)
}

#[cfg(test)]
mod tests {
    #[test]
    fn listing_lines_become_relative_paths() {
        assert_eq!(super::relative("./out/a.md"), Some("out/a.md"));
        for bad in ["", "./", "a.md", "./x/../y", ".//a"] {
            assert_eq!(super::relative(bad), None, "{bad}");
        }
    }
}
