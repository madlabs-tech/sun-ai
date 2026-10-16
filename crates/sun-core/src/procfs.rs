//! Process resource usage from Linux `/proc`, so no kernel or host has to
//! spawn `ps` for a number the OS already publishes (BusyBox's `ps`, in
//! Alpine images, has neither `%cpu` nor `-p`). Pure: the caller passes the
//! file reader, since sun-core does no I/O. Callers keep a `ps` path for
//! macOS, where `/proc` does not exist.
//!
//! `cpu_percent` is the **lifetime average** — CPU seconds over seconds
//! alive, in percent of one core — exactly what `ps %cpu` prints, so the
//! number means the same on Linux and macOS.

/// `/proc` reports CPU times in `USER_HZ` ticks: 100 on every Linux
/// architecture, fixed by the userspace ABI whatever the kernel's `HZ`.
pub const USER_HZ: f64 = 100.0;

/// One process's resources, as `ps -o pid,ppid,rss,%cpu` would report them.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ProcUsage {
    /// Process id.
    pub pid: u32,
    /// Parent process id.
    pub ppid: u32,
    /// Resident set size in bytes (`VmRSS`).
    pub rss_bytes: u64,
    /// Lifetime-average CPU use in percent of one core (see module doc).
    pub cpu_percent: f32,
}

/// The fields of `/proc/<pid>/stat` this module uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stat {
    /// Field 1: process id.
    pub pid: u32,
    /// Field 4: parent process id.
    pub ppid: u32,
    /// Fields 14 + 15: user plus system CPU time, in `USER_HZ` ticks.
    pub cpu_ticks: u64,
    /// Field 22: start time after boot, in `USER_HZ` ticks.
    pub start_ticks: u64,
}

/// Parses one `/proc/<pid>/stat` line. `None` on any missing or non-numeric field.
pub fn parse_stat(text: &str) -> Option<Stat> {
    // `comm` (field 2) is in parentheses and may itself hold spaces or
    // parentheses, so split at the last `)`: `tail` starts at field 3.
    let (head, tail) = text.rsplit_once(')')?;
    let pid = head.split_whitespace().next()?.parse().ok()?;
    let fields: Vec<&str> = tail.split_whitespace().collect();
    let field = |n: usize| fields.get(n - 3)?.parse::<u64>().ok();
    Some(Stat {
        pid,
        ppid: u32::try_from(field(4)?).ok()?,
        cpu_ticks: field(14)? + field(15)?,
        start_ticks: field(22)?,
    })
}

/// `VmRSS` from `/proc/<pid>/status`, in bytes. `None` for kernel threads
/// and zombies, which have no `VmRSS` line.
pub fn parse_vm_rss(status: &str) -> Option<u64> {
    let rest = status
        .lines()
        .find_map(|line| line.strip_prefix("VmRSS:"))?;
    let kb: u64 = rest.split_whitespace().next()?.parse().ok()?;
    Some(kb * 1024)
}

/// Seconds since boot, from `/proc/uptime`.
pub fn parse_uptime(text: &str) -> Option<f64> {
    text.split_whitespace().next()?.parse().ok()
}

/// Lifetime-average CPU percent of `stat` at `uptime_s` seconds after boot.
pub fn cpu_percent(stat: &Stat, uptime_s: f64) -> f32 {
    let alive_s = uptime_s - stat.start_ticks as f64 / USER_HZ;
    if alive_s <= 0.0 {
        return 0.0;
    }
    (100.0 * stat.cpu_ticks as f64 / USER_HZ / alive_s) as f32
}

/// Usage of process `pid`. `read` maps a path to its contents, e.g.
/// `|path| std::fs::read_to_string(path).ok()`; `uptime_s` comes from
/// [`parse_uptime`]. `None` when the process is gone or unreadable.
pub fn usage(
    pid: u32,
    uptime_s: f64,
    mut read: impl FnMut(&str) -> Option<String>,
) -> Option<ProcUsage> {
    let stat = parse_stat(&read(&format!("/proc/{pid}/stat"))?)?;
    let rss_bytes = parse_vm_rss(&read(&format!("/proc/{pid}/status"))?)?;
    Some(ProcUsage {
        pid: stat.pid,
        ppid: stat.ppid,
        rss_bytes,
        cpu_percent: cpu_percent(&stat, uptime_s),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // PID 1 of an alpine:3.21 container, captured on the VPS: utime 2,
    // stime 1, started at tick 577362079.
    const STAT: &str = "1 (cat) R 0 1 1 0 -1 4194560 1287 33029 0 47 2 1 37 20 20 0 1 0 577362079 1728512 192 18446744073709551615 103934122733568 103934123360802 140731757988160 0 0 0 0 4 0 0 0 0 17 1 0 0 0 0 0 103934123505680 103934123520048 103934201479168 140731757989741 140731757989758 140731757989758 140731757989871 0\n";
    const STATUS: &str = "Name:\tcat\nUmask:\t0022\nState:\tR (running)\nPPid:\t0\nVmPeak:\t    1688 kB\nVmRSS:\t     768 kB\nRssAnon:\t      88 kB\nThreads:\t1\n";

    #[test]
    fn parses_a_captured_stat_line() {
        let stat = parse_stat(STAT).unwrap();
        assert_eq!(
            stat,
            Stat {
                pid: 1,
                ppid: 0,
                cpu_ticks: 3,
                start_ticks: 577362079
            }
        );
        // 0.03 CPU seconds over 1.5 seconds alive.
        let alive = 577362079.0 / USER_HZ + 1.5;
        assert!((cpu_percent(&stat, alive) - 2.0).abs() < 1e-4);
        // Clock skew (uptime before start) never divides by zero or goes negative.
        assert_eq!(cpu_percent(&stat, 0.0), 0.0);
    }

    #[test]
    fn comm_with_spaces_and_parentheses_does_not_shift_fields() {
        let stat = parse_stat(
            "4242 (my (odd) name) S 7 4242 4242 0 -1 0 0 0 0 0 150 50 0 0 20 0 1 0 1000 0 0 0\n",
        )
        .unwrap();
        assert_eq!(
            stat,
            Stat {
                pid: 4242,
                ppid: 7,
                cpu_ticks: 200,
                start_ticks: 1000
            }
        );
    }

    #[test]
    fn rejects_garbage_and_kernel_threads() {
        assert_eq!(parse_stat("ps: unrecognized option: p\n"), None);
        assert_eq!(parse_stat(""), None);
        assert_eq!(parse_vm_rss("Name:\tkthreadd\nPPid:\t0\n"), None);
        assert_eq!(parse_vm_rss(STATUS), Some(768 * 1024));
        assert_eq!(parse_uptime("5773594.61 10704924.19\n"), Some(5773594.61));
    }

    #[test]
    fn usage_reads_stat_and_status_through_the_caller() {
        let read = |path: &str| match path {
            "/proc/1/stat" => Some(STAT.to_string()),
            "/proc/1/status" => Some(STATUS.to_string()),
            _ => None,
        };
        let alive = 577362079.0 / USER_HZ + 1.5;
        let got = usage(1, alive, read).unwrap();
        assert_eq!((got.pid, got.ppid, got.rss_bytes), (1, 0, 768 * 1024));
        assert!((got.cpu_percent - 2.0).abs() < 1e-4);
        assert_eq!(usage(99999, alive, read), None);
    }
}
