//! macOS: run the mod under `sandbox-exec` with a generated profile
//! (deny by default, Seatbelt profile language).

use std::ffi::OsString;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use super::{Sandbox, system_read_paths};

const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

/// Non-sensitive sysctls that runtimes read at start (CPU count, page size,
/// OS version). Process-table sysctls (`kern.procargs2`, `kern.proc.*`) stay
/// denied: they expose other processes' arguments and environment.
const SYSCTLS: &[&str] = &[
    "hw.activecpu",
    "hw.availcpu",
    "hw.cachelinesize",
    "hw.logicalcpu",
    "hw.logicalcpu_max",
    "hw.memsize",
    "hw.ncpu",
    "hw.pagesize",
    "hw.pagesize_compat",
    "hw.physicalcpu",
    "hw.physicalcpu_max",
    "hw.machine",
    "hw.cputype",
    "hw.cpusubtype",
    "hw.optional.arm64",
    "kern.argmax",
    "kern.hostname",
    "kern.osproductversion",
    "kern.osrelease",
    "kern.ostype",
    "kern.osversion",
    "kern.version",
    "kern.ngroups",
    "kern.usrstack64",
    "sysctl.proc_translated",
];

/// `sandbox-exec -p <profile> <program> <args..>`.
pub(super) fn wrap(
    sandbox: &Sandbox,
    program: PathBuf,
    args: &[String],
    read: &[PathBuf],
    proxy_port: u16,
) -> (PathBuf, Vec<OsString>) {
    let mut wrapped: Vec<OsString> = vec!["-p".into(), profile(sandbox, read, proxy_port).into()];
    wrapped.push(program.into());
    wrapped.extend(args.iter().map(OsString::from));
    (SANDBOX_EXEC.into(), wrapped)
}

/// The Seatbelt profile. Later rules win over earlier ones.
pub(super) fn profile(sandbox: &Sandbox, read: &[PathBuf], proxy_port: u16) -> String {
    let mut p = String::from(
        "(version 1)\n\
         (deny default)\n\
         (allow process-fork)\n\
         (allow signal (target same-sandbox))\n\
         (allow file-read-metadata)\n\
         (allow file-read* (literal \"/\"))\n\
         (allow file-read* file-write-data file-ioctl\n\
         \x20 (literal \"/dev/null\") (literal \"/dev/zero\") (literal \"/dev/stdin\")\n\
         \x20 (literal \"/dev/stdout\") (literal \"/dev/stderr\") (literal \"/dev/dtracehelper\"))\n\
         (allow file-read* (literal \"/dev/random\") (literal \"/dev/urandom\"))\n\
         (allow mach-lookup (global-name \"com.apple.system.opendirectoryd.libinfo\"))\n",
    );
    // Programs: its folder and entry runtime (`read`), plus the declared ones.
    p.push_str("(allow process-exec");
    if let Some(processes) = &sandbox.processes {
        for path in read {
            let _ = write!(p, " (subpath {})", path_literal(path));
        }
        for path in processes {
            let _ = write!(p, " (literal {})", path_literal(path));
        }
    }
    p.push_str(")\n");
    p.push_str("(allow sysctl-read");
    for name in SYSCTLS {
        let _ = write!(p, " (sysctl-name {})", quote(name));
    }
    p.push_str(")\n(allow file-read*");
    for path in system_read_paths() {
        let _ = write!(p, " (subpath {})", quote(path));
    }
    for path in read {
        let _ = write!(p, " (subpath {})", path_literal(path));
    }
    p.push_str(")\n");
    p.push_str("(allow file-read* file-write*");
    for path in std::iter::once(&sandbox.data_dir).chain(&sandbox.paths) {
        let _ = write!(p, " (subpath {})", path_literal(path));
    }
    p.push_str(")\n");
    // Seatbelt only filters remote addresses by "localhost" or "*", which is
    // all we need: the proxy is the one loopback port allowed.
    let _ = writeln!(
        p,
        "(allow network-outbound (remote tcp {}))",
        quote(&format!("localhost:{proxy_port}"))
    );
    p
}

/// Seatbelt matches resolved paths (`/tmp` is `/private/tmp`).
fn path_literal(path: &Path) -> String {
    let resolved = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    quote(&resolved.to_string_lossy())
}

/// A Seatbelt string literal.
fn quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}
