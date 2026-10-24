# ADR: repo editor — worktree, file frames, path confinement, PTY transport

Status: accepted (Sun Editor stage 1, BLO-186; terminal as built in stage 2, BLO-188). Protocol 2.2, terminals 2.3.

## Editor worktree

One long-lived git worktree per (repo, host, member) at `<host data>/editor/<repo_id>/<member_id>`, made lazily by the first `editor.open` from the repo's cached bare clone (`repos/`, shared with tasks) on branch `sun/editor/<member_id>/<repo_id>`, cut from the repo's default branch. It is never refreshed or pushed by Sun; the member uses git in the terminal. Task worktrees live in `work/` and task removal only deletes `work/<task_id>` and prunes the clone's records of *missing* worktrees, so it never touches an editor worktree. Nothing is persisted in the kernel: no migration.

The branch carries the repo id as well as the member id (the issue said `sun/editor/<member_id>`): two repos with the same URL share one bare clone, and git refuses to check one branch out twice.

## Frames (kernel → host requests, each answered by one `reply`)

| Frame | Reply | Notes |
|---|---|---|
| `editor.open {tree, url, base}` | `worktree {path}` | idempotent; may clone (15 min wait) |
| `fs.list {tree, path}` | `entries {entries, truncated}` | one level; `.git` and `node_modules` `collapsed` |
| `fs.index {tree}` | `index {paths, truncated}` | `git ls-files --cached --others --exclude-standard`, minus `node_modules`, ≤ 50 000 |
| `fs.read {tree, path}` | `file {size, mtime_ms, hash, too_large, binary, content}` | ≤ 2 MiB; NUL or non-UTF-8 is binary |
| `fs.write {tree, path, content, expected_hash}` | `stat {stat}` or `conflict {current}` | atomic temp + rename, mode kept; `expected_hash` = SHA-256 of what was read |
| `fs.create {tree, path, kind}` | `done` | file or dir; never replaces |
| `fs.rename {tree, from, to}` | `done` | `to` must not exist |
| `fs.delete {tree, path}` | `done` | a symlink itself, or a dir recursively |

`tree` is `{repo_id, member_id}`; the host derives the root, so the kernel never sends a host path. The kernel refuses editor calls to a host below protocol 2.2 at once (503) instead of timing out. REST: `/api/v1/workspaces/{id}/repos/{repo_id}/editor/{open,tree,index,file,rename}?host_id=&path=`; `PUT file` takes the raw text (not JSON, so the workspace guard's 2 MiB body scan doesn't apply) and `If-Match: <hash>`, and a stale hash is 409 with the current version. Every handler passes `editor_access::require(api, caller, workspace, repo, cap)` once; until grants land (BLO-187) it allows the deployment owner and the workspace owner role only.

## Path confinement (host, `sun-host/src/editor.rs`)

1. Lexical: every part must be a plain name. Absolute paths, `..`, and any `.git` part in any letter case are refused (editing `.git` could plant a hook or repoint the worktree).
2. Paths that must exist are canonicalized (symlinks followed) and must still start with the canonical root and contain no `.git` part.
3. Create, rename, delete and a new file act on the entry itself: the *parent* is canonicalized and checked, the final name is joined unresolved. Deleting a symlink removes the link, never its target. Writing an existing symlink writes its target only if that is inside the root.
4. Errors name the repo-relative path, never the host path.

Known gap: a process on the host that swaps a checked directory for a symlink between check and use can escape. Only the host's own users can do that, and a terminal user already has the host. Fixing it needs `openat2(RESOLVE_BENEATH)` (Linux only).

## PTY transport for stage 2 (decided now)

| Option | Pros | Cons |
|---|---|---|
| A. JSON frames on the host socket, output base64 | no new endpoint | +33% bytes, JSON per chunk, a `cat` of a big file delays every other reply and event on the one socket; violates "not JSON-escaped" |
| B. Binary messages multiplexed on the host socket | one connection | new framing on the control socket, same head-of-line blocking, per-PTY back-pressure must be hand-built |
| **C. One host-dialled WebSocket per PTY** | raw bytes both ways, TCP back-pressure per PTY, a flood can't stall tasks, the kernel relay is a byte pipe | one more connection per PTY (≤ 5 per member per host), its own auth |

**Choice: C.** As built in stage 2 (BLO-188, protocol 2.3):

- The control socket gains one frame, `pty.open {pty_id, tree, cols, rows}` → `done`. The **kernel** mints `pty_id` (a ULID) and registers a slot for it *before* sending, so the host's socket can never arrive ahead of its slot. That replaces the reply `{pty_id, token}` sketched above: the host proves itself with its `host_key`, which a token minted by the host would only repeat. `pty.kill` was dropped too: closing the PTY socket is the kill, and a socket with no slot waiting is closed at once.
- The host spawns the shell (`$SHELL`, else the `/etc/passwd` login shell, else `/bin/sh`) on a PTY (`pty-process`; `openpty`/`setsid` need the `unsafe` the workspace forbids) in the editor worktree, as a session leader with the host's own env plus `TERM=xterm-256color`, so `claude` is on `PATH` exactly as for task sessions. It dials `GET /host/pty`, sends `PtyHello {host_id, host_key, pty_id}`, then replies `done`. At most 5 PTYs per member per host (`MAX_PTYS`), enforced by the host, parked ones included.
- Binary messages are PTY bytes both ways (output in ≤ 64 KiB reads). Text messages are `PtyControl`: `{"kind":"resize","cols","rows"}` kernel → host (1–1000 each, checked on both sides), `{"kind":"exit","code"}` host → kernel, last. Input and output are separate tasks on both hops, so a big paste into a program that doesn't read stdin can't deadlock the output.
- Browser: `GET /api/v1/workspaces/{id}/repos/{repo_id}/terminal?host_id=&cols=&rows=` (or `&pty_id=` to reattach). `editor_access::require(.., Terminal)` and the same-origin check run on every connect, before the upgrade, so a refused connect spawns nothing. The kernel sends `{"kind":"open","pty_id"}` on each attach, then relays; `{"kind":"exit","code"}`, `{"kind":"dead"}` (host socket dropped) or `{"kind":"error","message"}` (no PTY: cap, worktree not opened, host older than 2.3) end it.
- Grace: a closed browser parks the PTY for 30 s; only the same member, repo and host reattaches it, and a second attach replaces the first. After the grace the kernel closes the host socket.
- Kill: when the socket closes, or the kernel is silent for the host's liveness window (30 s; the kernel pings both sockets every 10 s), the host SIGKILLs the shell's process group and every process of its session (`pkill -s`, so `vim` and `cmd &` jobs in groups of their own go too) and reaps the shell. A shell that exits takes its session with it, then the exit code goes out. A process that calls `setsid` itself (a daemon) outlives the terminal, as on a desktop. The container image runs `sun` under `tini` so orphans of killed processes are reaped.
- Audit: `terminal.open` and `terminal.close after <N>s` (`AuditAction::Terminal`, user, repo, host). Nothing typed or shown is parsed beyond the `kind` of text messages, logged or audited.

**Security posture:** the shell runs as the host's service user (`sun` on Linux installs, the installing user with launchd) with that user's files, network and environment, including whatever secrets that user can read (the kernel's data dir and env on a `sun serve --with-host` box). The editor worktree is only the starting directory, not a jail. Grant `terminal` only to people you would give a shell on that host.

Measured (BLO-188, kernel + host on one Linux box over loopback, debug build): keystroke echo p50 0.5 ms, p95 2–4 ms over 300 keys; `cat` of a 32 MiB text file 14–22 MiB/s. A LAN adds its round trip to the echo; `terminal::measure_echo_latency_and_throughput` (ignored) re-measures.
