# ADR: repo editor — worktree, file frames, path confinement, PTY transport

Status: accepted (Sun Editor stage 1, BLO-186). Protocol 2.2.

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

**Choice: C.** The control socket gains `pty.open {tree, cols, rows}` → reply `{pty_id, token}` (spawned in the editor worktree) and `pty.kill {pty_id}`. The host then dials `GET /host/pty`, first text frame `{host_id, host_key, pty_id, token}` (single-use token, checked like `hello`), then binary messages are PTY bytes both ways; text messages are JSON control: kernel → host `{"kind":"resize","cols","rows"}`, host → kernel `{"kind":"exit","code"}` before close. The browser's `/workspaces/{id}/repos/{repo_id}/terminal?host_id=` socket is spliced to it. On a browser drop the kernel keeps the host socket for the 30 s grace, then closes it, and the host kills the process group. The `terminal` capability is checked on every browser connect.
