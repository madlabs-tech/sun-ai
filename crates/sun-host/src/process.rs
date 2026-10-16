//! OS process helpers: the `exec` op and process-tree resource sampling.
//! On Linux the sampler reads `/proc` ([`sun_core::procfs`]); elsewhere it
//! uses the `ps` binary, since the workspace forbids `unsafe` and so direct
//! syscalls. Both report the lifetime-average CPU (`ps %cpu`).

use std::collections::HashMap;
use std::process::Stdio;
use std::time::Duration;

use sun_core::host_protocol::{ExecOutput, ExecRequest};
use sun_core::redact::redact;
use sun_core::{Error, Result};
use tokio::process::Command;

pub use sun_runtime::process::kill_group;

/// How much of an exec's output is kept (the tail, where failures are).
const OUTPUT_TAIL: usize = 64 * 1024;

/// Runs `sh -c <command>` in `request.cwd` (already checked) with
/// `request.env` on top of the host's environment. See [`run`].
pub async fn exec(request: &ExecRequest) -> Result<ExecOutput> {
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg(&request.command)
        .current_dir(&request.cwd)
        .envs(&request.env);
    run(cmd, request).await
}

/// Runs `cmd` in its own process group with stdin closed, keeping the
/// redacted output tail (known secrets: `request.env` values). On
/// `request.timeout_s` the whole group is killed.
pub async fn run(mut cmd: Command, request: &ExecRequest) -> Result<ExecOutput> {
    cmd.process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let program = cmd.as_std().get_program().to_string_lossy().into_owned();
    let child = cmd
        .spawn()
        .map_err(|e| Error::Unavailable(format!("cannot run {program}: {e}")))?;
    let pgid = child.id();
    let timeout = Duration::from_secs(request.timeout_s);
    let secrets: Vec<&str> = request.env.values().map(String::as_str).collect();
    match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(output)) => {
            let mut bytes = output.stdout;
            bytes.extend_from_slice(&output.stderr);
            let start = bytes.len().saturating_sub(OUTPUT_TAIL);
            Ok(ExecOutput {
                code: output.status.code(),
                output: redact(&String::from_utf8_lossy(&bytes[start..]), &secrets),
                timed_out: false,
            })
        }
        Ok(Err(e)) => Err(Error::Unavailable(format!("exec: {e}"))),
        Err(_) => {
            if let Some(pgid) = pgid {
                kill_group(pgid).await;
            }
            // ponytail: output of a timed-out command is dropped; stream it
            // into a buffer if gates need it for diagnosis.
            Ok(ExecOutput {
                code: None,
                output: format!("timed out after {}s", request.timeout_s),
                timed_out: true,
            })
        }
    }
}

/// One process: id, parent, resident bytes and lifetime-average CPU percent.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Proc {
    pid: u32,
    ppid: u32,
    rss_bytes: u64,
    cpu: f32,
}

/// A snapshot of every process on the machine (empty when unreadable).
#[cfg(target_os = "linux")]
pub async fn snapshot() -> Vec<Proc> {
    tokio::task::spawn_blocking(snapshot_proc)
        .await
        .unwrap_or_default()
}

/// Reads `/proc/<pid>/{stat,status}` for every numeric entry of `/proc`.
/// Kernel threads and zombies (no `VmRSS`) are skipped.
#[cfg(target_os = "linux")]
fn snapshot_proc() -> Vec<Proc> {
    use sun_core::procfs;
    let read = |path: &str| std::fs::read_to_string(path).ok();
    let Some(uptime_s) = read("/proc/uptime").and_then(|text| procfs::parse_uptime(&text)) else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse::<u32>().ok())
        .filter_map(|pid| procfs::usage(pid, uptime_s, read))
        .map(|u| Proc {
            pid: u.pid,
            ppid: u.ppid,
            rss_bytes: u.rss_bytes,
            cpu: u.cpu_percent,
        })
        .collect()
}

/// A snapshot of every process on the machine (empty when `ps` fails).
#[cfg(not(target_os = "linux"))]
pub async fn snapshot() -> Vec<Proc> {
    let Ok(output) = Command::new("ps")
        .args(["-A", "-o", "pid=,ppid=,rss=,%cpu="])
        .stdin(Stdio::null())
        .output()
        .await
    else {
        return Vec::new();
    };
    parse_ps(&String::from_utf8_lossy(&output.stdout))
}

/// Parses `ps -A -o pid=,ppid=,rss=,%cpu=` (rss in KiB).
#[cfg(any(not(target_os = "linux"), test))]
fn parse_ps(text: &str) -> Vec<Proc> {
    text.lines()
        .filter_map(|line| {
            let mut cols = line.split_whitespace();
            Some(Proc {
                pid: cols.next()?.parse().ok()?,
                ppid: cols.next()?.parse().ok()?,
                rss_bytes: cols.next()?.parse::<u64>().ok()? * 1024,
                cpu: cols.next()?.parse().ok()?,
            })
        })
        .collect()
}

/// `root` and all its descendants.
pub fn tree(procs: &[Proc], root: u32) -> Vec<Proc> {
    let mut children: HashMap<u32, Vec<&Proc>> = HashMap::new();
    for p in procs {
        children.entry(p.ppid).or_default().push(p);
    }
    let mut out: Vec<Proc> = procs.iter().filter(|p| p.pid == root).copied().collect();
    let mut i = 0;
    while i < out.len() {
        let pid = out[i].pid;
        out.extend(children.get(&pid).into_iter().flatten().map(|p| **p));
        i += 1;
    }
    out
}

/// RSS in bytes and CPU percent summed over a process tree.
pub fn usage(tree: &[Proc]) -> (u64, f32) {
    tree.iter()
        .fold((0, 0.0), |(rss, cpu), p| (rss + p.rss_bytes, cpu + p.cpu))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sums_the_whole_tree_only() {
        let procs = parse_ps(
            "    1     0  1000  0.5\n  100     1   200  10.0\n  101   100   300  5.5\n  102   101   100  0.0\n  200     1  9999 50.0\nbad line\n",
        );
        assert_eq!(procs.len(), 5);
        let tree = tree(&procs, 100);
        assert_eq!(
            tree.iter().map(|p| p.pid).collect::<Vec<_>>(),
            [100, 101, 102]
        );
        assert_eq!(usage(&tree), (600 * 1024, 15.5));
        assert!(super::tree(&procs, 999).is_empty());
    }

    /// The `/proc` sampler sees this very process, with memory, under its parent.
    #[cfg(target_os = "linux")]
    #[test]
    fn proc_snapshot_includes_this_process() {
        let procs = snapshot_proc();
        let me = std::process::id();
        let own = procs.iter().find(|p| p.pid == me).copied();
        assert!(
            own.is_some_and(|p| p.rss_bytes > 0 && p.ppid != 0),
            "{own:?}"
        );
        assert_eq!(tree(&procs, me)[0].pid, me);
    }
}
