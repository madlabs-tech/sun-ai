//! Linux: the `sun-mod-sandbox` launcher. The kernel starts
//! `sun-mod-sandbox <policy json> -- <program> <args..>`; the launcher jails
//! itself, then runs the mod as its child, which inherits the jail:
//!
//! 1. a new user namespace (unprivileged), then a network namespace with only
//!    loopback. A forwarder thread, left in the host network namespace,
//!    relays a loopback port inside the namespace to the kernel's CONNECT
//!    proxy: the mod's only way out.
//! 2. Landlock: read-only system paths and the mod's folder, read-write its
//!    data dir and declared paths; TCP connect only to the forwarder port.
//! 3. seccomp: denies syscalls that could widen the jail (ptrace, mount,
//!    namespaces, bpf, io_uring, keyrings, module loading, ...).
//!
//! Any step failing refuses to start the mod (fail closed).

use std::ffi::OsString;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::fd::AsRawFd;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::sync::mpsc;

use serde::{Deserialize, Serialize};
use sun_core::{Error, Result};

use super::{PROXY_VARS, Sandbox, resolve, system_read_paths};

/// Device files every mod may use.
const DEVICES: [&str; 5] = [
    "/dev/null",
    "/dev/zero",
    "/dev/full",
    "/dev/random",
    "/dev/urandom",
];

/// What the launcher enforces; passed as its first argument.
#[derive(Debug, Serialize, Deserialize)]
struct Policy {
    read: Vec<PathBuf>,
    write: Vec<PathBuf>,
    /// The only paths it may execute from; `None`: anything it can read.
    #[serde(default)]
    exec: Option<Vec<PathBuf>>,
    proxy_port: u16,
}

/// `sun-mod-sandbox <policy> -- <program> <args..>`.
pub(super) fn wrap(
    sandbox: &Sandbox,
    program: PathBuf,
    args: &[String],
    extra_read: &[PathBuf],
    proxy_port: u16,
) -> Result<(PathBuf, Vec<OsString>)> {
    let mut read: Vec<PathBuf> = system_read_paths().iter().map(PathBuf::from).collect();
    read.extend(extra_read.iter().cloned());
    let mut write: Vec<PathBuf> = DEVICES.iter().map(PathBuf::from).collect();
    write.push(sandbox.data_dir.clone());
    write.extend(sandbox.paths.iter().cloned());
    let exec = sandbox.processes.as_ref().map(|processes| {
        let mut exec: Vec<PathBuf> = extra_read.to_vec();
        exec.extend(processes.iter().cloned());
        // execve opens a dynamic program's ELF interpreter with exec access:
        // grant those loader files, not the lib dirs holding them (/usr/lib
        // holds helper binaries, and on Ubuntu >= 26.04 every coreutil).
        let started = resolve(&program.to_string_lossy());
        let programs = exec.iter().cloned().chain(started);
        let loaders: Vec<PathBuf> = programs.filter_map(|p| elf_interpreter(&p)).collect();
        exec.extend(loaders);
        exec
    });
    let policy = Policy {
        read,
        write,
        exec,
        proxy_port,
    };
    let policy = serde_json::to_string(&policy).map_err(|e| Error::InvalidInput(e.to_string()))?;
    let mut wrapped: Vec<OsString> = vec![policy.into(), "--".into(), program.into()];
    wrapped.extend(args.iter().map(OsString::from));
    Ok((sandbox.launcher.clone(), wrapped))
}

/// The ELF interpreter (`PT_INTERP`, the dynamic loader) `program` names;
/// `None` for a static binary, a script, or anything that isn't ELF.
fn elf_interpreter(program: &Path) -> Option<PathBuf> {
    use std::io::{Seek, SeekFrom};
    use std::os::unix::ffi::OsStrExt;
    const PT_INTERP: u64 = 3;
    // Native byte order: a foreign-endian binary can't run here anyway.
    let uint = |b: &[u8]| -> Option<u64> {
        Some(match b.len() {
            2 => u16::from_ne_bytes(b.try_into().ok()?).into(),
            4 => u32::from_ne_bytes(b.try_into().ok()?).into(),
            8 => u64::from_ne_bytes(b.try_into().ok()?),
            _ => return None,
        })
    };
    let mut file = std::fs::File::open(program).ok()?;
    let mut head = [0u8; 64];
    file.read_exact(&mut head).ok()?;
    if !head.starts_with(b"\x7fELF") {
        return None;
    }
    // (e_phoff, e_phentsize, e_phnum, p_offset, p_filesz) offsets per class.
    let (phoff, phentsize, phnum, offset, filesz) = match head[4] {
        1 => (0x1c..0x20, 0x2a..0x2c, 0x2c..0x2e, 4..8, 16..20),
        2 => (0x20..0x28, 0x36..0x38, 0x38..0x3a, 8..16, 32..40),
        _ => return None,
    };
    let (phoff, phentsize) = (uint(&head[phoff])?, uint(&head[phentsize])?);
    let entry = usize::try_from(phentsize)
        .ok()
        .filter(|&n| n >= filesz.end)?;
    let mut ph = vec![0u8; entry.min(64)];
    for i in 0..uint(&head[phnum])?.min(256) {
        file.seek(SeekFrom::Start(
            phoff.checked_add(i.checked_mul(phentsize)?)?,
        ))
        .ok()?;
        file.read_exact(&mut ph).ok()?;
        if uint(&ph[..4])? != PT_INTERP {
            continue;
        }
        let len = uint(&ph[filesz.clone()])?;
        let mut path = vec![0u8; usize::try_from(len.min(4096)).ok()?];
        file.seek(SeekFrom::Start(uint(&ph[offset.clone()])?))
            .ok()?;
        file.read_exact(&mut path).ok()?;
        let path = path.split(|&b| b == 0).next()?;
        return Some(PathBuf::from(std::ffi::OsStr::from_bytes(path)));
    }
    None
}

/// Entry point of the `sun-mod-sandbox` binary. Exits with the mod's status,
/// or 126 if the jail could not be set up.
pub fn launch() -> ExitCode {
    match jail_and_run() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("sun-mod-sandbox: refusing to start the mod: {e}");
            ExitCode::from(126)
        }
    }
}

fn jail_and_run() -> std::result::Result<ExitCode, String> {
    let mut args = std::env::args_os().skip(1);
    let policy: Policy = args
        .next()
        .and_then(|p| serde_json::from_str(&p.to_string_lossy()).ok())
        .ok_or("usage: sun-mod-sandbox <policy json> -- <program> [args..]")?;
    if args.next().as_deref() != Some("--".as_ref()) {
        return Err("expected -- before the program".into());
    }
    let program = args.next().ok_or("missing program")?;

    // ponytail: if the kernel kills this launcher, the mod lives on until it
    // reads EOF on stdin (the SDKs exit then), jailed and offline. Make the
    // launcher a PID-namespace init if a mod ever ignores EOF.
    enter_user_namespace()?;

    // The forwarder is spawned before the network namespace, so it (and the
    // threads it starts) keep the host network: namespaces are per thread.
    let (to_forwarder, listener_rx) = mpsc::channel::<TcpListener>();
    let proxy_port = policy.proxy_port;
    std::thread::spawn(move || forward(listener_rx, proxy_port));

    nix::sched::unshare(nix::sched::CloneFlags::CLONE_NEWNET).map_err(err("network namespace"))?;
    loopback_up()?;
    let listener = TcpListener::bind(("127.0.0.1", 0)).map_err(err("bind forwarder"))?;
    let inner_port = listener.local_addr().map_err(err("bind forwarder"))?.port();
    to_forwarder.send(listener).map_err(err("forwarder"))?;

    landlock(&policy, inner_port)?;
    seccomp()?;

    let proxy_url = format!("http://127.0.0.1:{inner_port}");
    let status = Command::new(program)
        .args(args)
        .envs(PROXY_VARS.map(|var| (var, proxy_url.as_str())))
        .status()
        .map_err(err("start mod"))?;
    let code = status
        .code()
        .or_else(|| status.signal().map(|sig| 128 + sig))
        .unwrap_or(1);
    Ok(ExitCode::from(u8::try_from(code).unwrap_or(1)))
}

fn err<E: std::fmt::Display>(what: &'static str) -> impl Fn(E) -> String {
    move |e| format!("{what}: {e}")
}

/// Unprivileged user namespace mapping our own uid/gid, so we may create a
/// network namespace. The mod is exec'd as a non-root uid in it, so it ends
/// up with no capabilities.
fn enter_user_namespace() -> std::result::Result<(), String> {
    let (uid, gid) = (nix::unistd::getuid(), nix::unistd::getgid());
    nix::sched::unshare(nix::sched::CloneFlags::CLONE_NEWUSER).map_err(err(
        "user namespace (are unprivileged user namespaces enabled?)",
    ))?;
    std::fs::write("/proc/self/setgroups", "deny").map_err(err("setgroups"))?;
    std::fs::write("/proc/self/uid_map", format!("{uid} {uid} 1")).map_err(err("uid_map"))?;
    std::fs::write("/proc/self/gid_map", format!("{gid} {gid} 1")).map_err(err("gid_map"))?;
    Ok(())
}

/// Brings `lo` (always ifindex 1 in a new namespace) up with one
/// RTM_NEWLINK netlink request.
fn loopback_up() -> std::result::Result<(), String> {
    use nix::sys::socket::{
        AddressFamily, MsgFlags, NetlinkAddr, SockFlag, SockProtocol, SockType, recv, sendto,
        socket,
    };
    let fd = socket(
        AddressFamily::Netlink,
        SockType::Raw,
        SockFlag::SOCK_CLOEXEC,
        SockProtocol::NetlinkRoute,
    )
    .map_err(err("netlink"))?;
    let mut msg = Vec::with_capacity(32);
    msg.extend(32u32.to_ne_bytes()); // nlmsg_len: header + ifinfomsg
    msg.extend(libc::RTM_NEWLINK.to_ne_bytes());
    msg.extend(((libc::NLM_F_REQUEST | libc::NLM_F_ACK) as u16).to_ne_bytes());
    msg.extend(1u32.to_ne_bytes()); // seq
    msg.extend(0u32.to_ne_bytes()); // pid: the kernel
    msg.extend([libc::AF_UNSPEC as u8, 0]); // ifi_family, padding
    msg.extend(0u16.to_ne_bytes()); // ifi_type
    msg.extend(1i32.to_ne_bytes()); // ifi_index: lo
    msg.extend((libc::IFF_UP as u32).to_ne_bytes()); // ifi_flags
    msg.extend((libc::IFF_UP as u32).to_ne_bytes()); // ifi_change
    sendto(
        fd.as_raw_fd(),
        &msg,
        &NetlinkAddr::new(0, 0),
        MsgFlags::empty(),
    )
    .map_err(err("netlink send"))?;
    let mut ack = [0u8; 64];
    let n = recv(fd.as_raw_fd(), &mut ack, MsgFlags::empty()).map_err(err("netlink ack"))?;
    // NLMSG_ERROR: a 16-byte header, then the errno (0 = success).
    let errno = ack
        .get(16..20)
        .filter(|_| n >= 20)
        .and_then(|b| b.try_into().ok())
        .map(i32::from_ne_bytes);
    match errno {
        Some(0) => Ok(()),
        other => Err(format!("bringing up loopback failed: {other:?}")),
    }
}

/// Accepts connections on the in-namespace listener and relays each to the
/// kernel's proxy (this thread is still in the host network namespace).
fn forward(listener: mpsc::Receiver<TcpListener>, proxy_port: u16) {
    let Ok(listener) = listener.recv() else {
        return;
    };
    for client in listener.incoming().flatten() {
        std::thread::spawn(move || {
            let Ok(upstream) = TcpStream::connect(("127.0.0.1", proxy_port)) else {
                return;
            };
            let (Ok(client2), Ok(upstream2)) = (client.try_clone(), upstream.try_clone()) else {
                return;
            };
            std::thread::spawn(move || pipe(client2, upstream2));
            pipe(upstream, client);
        });
    }
}

/// Copies `from` to `to` until EOF, then closes `to`'s write side.
fn pipe(mut from: TcpStream, mut to: TcpStream) {
    let mut buf = [0u8; 16 * 1024];
    while let Ok(n @ 1..) = from.read(&mut buf) {
        if to.write_all(&buf[..n]).is_err() {
            break;
        }
    }
    let _ = to.shutdown(std::net::Shutdown::Write);
}

/// Landlock on this thread (inherited by the mod). Refuses to continue on a
/// kernel without Landlock.
fn landlock(policy: &Policy, inner_port: u16) -> std::result::Result<(), String> {
    use landlock::{
        ABI, Access, AccessFs, AccessNet, BitFlags, NetPort, Ruleset, RulesetAttr,
        RulesetCreatedAttr, RulesetStatus, path_beneath_rules,
    };
    let abi = ABI::V5;
    let existing = |paths: &[PathBuf]| -> Vec<PathBuf> {
        paths.iter().filter(|p| p.exists()).cloned().collect()
    };
    // With an exec list, execute rights come only from it (`processes`).
    let no_exec = match policy.exec {
        Some(_) => !BitFlags::from(AccessFs::Execute),
        None => BitFlags::all(),
    };
    let status = Ruleset::default()
        .handle_access(AccessFs::from_all(abi))
        .and_then(|r| r.handle_access(AccessNet::from_all(abi)))
        .and_then(|r| r.create())
        .and_then(|r| {
            r.add_rules(path_beneath_rules(
                existing(&policy.read),
                AccessFs::from_read(abi) & no_exec,
            ))
        })
        .and_then(|r| {
            r.add_rules(path_beneath_rules(
                existing(&policy.write),
                AccessFs::from_all(abi) & no_exec,
            ))
        })
        .and_then(|r| {
            r.add_rules(path_beneath_rules(
                existing(policy.exec.as_deref().unwrap_or_default()),
                AccessFs::from_read(abi),
            ))
        })
        .and_then(|r| r.add_rule(NetPort::new(inner_port, AccessNet::ConnectTcp)))
        .and_then(|r| r.restrict_self())
        .map_err(err("landlock"))?;
    match status.ruleset {
        RulesetStatus::NotEnforced => Err("landlock is not supported by this kernel".into()),
        RulesetStatus::FullyEnforced | RulesetStatus::PartiallyEnforced => Ok(()),
    }
}

/// seccomp on this thread (inherited by the mod): a deny-list, because mods
/// can be any runtime (Rust, Node, Python) and an allow-list would break them.
fn seccomp() -> std::result::Result<(), String> {
    use seccompiler::{
        SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter,
        SeccompRule, TargetArch,
    };
    let arch = TargetArch::try_from(std::env::consts::ARCH).map_err(err("seccomp arch"))?;
    let denied = [
        libc::SYS_ptrace,
        libc::SYS_process_vm_readv,
        libc::SYS_process_vm_writev,
        libc::SYS_mount,
        libc::SYS_umount2,
        libc::SYS_pivot_root,
        libc::SYS_chroot,
        libc::SYS_unshare,
        libc::SYS_setns,
        libc::SYS_bpf,
        libc::SYS_perf_event_open,
        libc::SYS_userfaultfd,
        libc::SYS_keyctl,
        libc::SYS_add_key,
        libc::SYS_request_key,
        libc::SYS_io_uring_setup,
        libc::SYS_io_uring_enter,
        libc::SYS_io_uring_register,
        libc::SYS_init_module,
        libc::SYS_finit_module,
        libc::SYS_delete_module,
        libc::SYS_kexec_load,
        libc::SYS_reboot,
        libc::SYS_swapon,
        libc::SYS_swapoff,
        libc::SYS_open_by_handle_at,
        libc::SYS_name_to_handle_at,
        libc::SYS_fsopen,
        libc::SYS_fsmount,
        libc::SYS_move_mount,
        libc::SYS_open_tree,
    ];
    let mut rules: std::collections::BTreeMap<i64, Vec<SeccompRule>> =
        denied.iter().map(|&nr| (nr, vec![])).collect();
    // clone() creating namespaces; plain fork/thread clones pass.
    let ns_flags = [
        libc::CLONE_NEWUSER,
        libc::CLONE_NEWNET,
        libc::CLONE_NEWNS,
        libc::CLONE_NEWPID,
        libc::CLONE_NEWIPC,
        libc::CLONE_NEWUTS,
        libc::CLONE_NEWCGROUP,
    ];
    let clone_rules = ns_flags
        .iter()
        .map(|&flag| {
            let flag = flag as u64;
            let cond = SeccompCondition::new(
                0,
                SeccompCmpArgLen::Qword,
                SeccompCmpOp::MaskedEq(flag),
                flag,
            )?;
            SeccompRule::new(vec![cond])
        })
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(err("seccomp clone rule"))?;
    rules.insert(libc::SYS_clone, clone_rules);
    let eperm = SeccompAction::Errno(libc::EPERM as u32);
    let deny =
        SeccompFilter::new(rules, SeccompAction::Allow, eperm, arch).map_err(err("seccomp"))?;
    // clone3 passes flags in memory, which seccomp can't inspect: ENOSYS
    // makes libc fall back to clone(), which the filter above checks.
    let clone3 = [(libc::SYS_clone3, vec![])].into_iter().collect();
    let enosys = SeccompAction::Errno(libc::ENOSYS as u32);
    let clone3 =
        SeccompFilter::new(clone3, SeccompAction::Allow, enosys, arch).map_err(err("seccomp"))?;
    for filter in [deny, clone3] {
        let program: seccompiler::BpfProgram = filter.try_into().map_err(err("seccomp compile"))?;
        seccompiler::apply_filter(&program).map_err(err("seccomp apply"))?;
    }
    Ok(())
}
