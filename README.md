# Sun

**One small Rust binary that turns the agent CLIs your team already pays for (Claude Code, Codex, OpenCode) into workflows that survive crashes, keep going when a quota runs out, and are checked by gates before anything counts as done.**

Sun is an AI orchestrator and workflow engine for individuals and small teams. Anyone on the team (marketing, product, design, engineering, leadership) hands work to agents as tasks. Each task runs through a versioned workflow of steps. The engine decides when a task is done, not the agent. It is source-available, free to self-host as one binary, and can also run hosted in Sun Cloud.

> **Status:** every planned feature has code, and the scope audit counts 107 of 122 user stories as done. Several things are still unproven against real accounts and hardware: a real quota hit, Linux sandbox CI, real VMs and simulators, live payments. The release host and signing key are placeholders, and CI has not run yet. See [Status and known limits](#status-and-known-limits).

---

## Contents

- [The four claims](#the-four-claims)
- [Features by milestone](#features-by-milestone)
- [Quick start](#quick-start)
- [CLI cheat sheet](#cli-cheat-sheet)
- [Configuration](#configuration)
- [Architecture](#architecture)
- [Project layout](#project-layout)
- [Development](#development)
- [Footprint](#footprint)
- [Status and known limits](#status-and-known-limits)
- [Licensing](#licensing)

---

## The four claims

| | Claim | How |
|---|---|---|
| **A** | **Runs survive anything.** A crash, restart or lost host never loses or re-runs a finished step. | Every state change is one transaction (step state + event + outbox row). Side effects go through an idempotent outbox. Hosts buffer events on disk and use seq/ack. |
| **B** | **Agents don't stop at a quota.** When a runtime hits its limit, the work switches to the next runtime and continues in the same worktree. | Hand-off packs are written ahead while the agent works, so building one needs no LLM call. A watchdog catches stalls, loops and blown budgets. Gates, not the agent, decide "done". |
| **C** | **Agents get a small, visible context instead of a hidden memory.** | One append-only ledger. Each step gets a budgeted context pack (≈ 10% of the model's window), delivered as files. Agents pull more through the `sun` MCP server. Retrieval uses SQLite FTS5, not a vector database. |
| **D** | **It stays small, and one person with AI agents can build it.** | One binary with the UI embedded: 25.7 MB, 21.5 MB idle RSS, 38 ms cold start. Small crates with narrow interfaces. Every build phase fits half a context window. |

## Features by milestone

Milestones are planned in phases sized to one agent session, not in weeks.

**M1 — v0.1 "Agents that don't stop"**
- One-step install, a lean kernel, and safe upgrades with backup and rollback.
- First-party plugins and extensions over one mod protocol.
- Workflow core: YAML step lists with `agent`, `gate`, `action`, `approval`, `wait`, `notify` and extension steps.
- Durable runs that resume from the journal.
- Watchdog for stalls, loops, budgets and silent exits.
- Engine-verified gates: `diff_not_empty`, `test`, `lint`, `diagnostics_clean`, `pr_exists`, `ci_green`, plus content gates.
- Flow control: priority, FIFO and caps per runtime, host and ecosystem.
- Run log and cost.
- Adapters for Claude Code, Codex and OpenCode.
- Shared runtime hosts.
- Failover with a hand-off pack.
- GitHub, GitLab and Slack.
- Non-code tasks and content gates.
- Bring your own runtime.

**M2 — v0.2 Team**
- Multi-repo tasks with linked PRs in merge order.
- OIDC, RBAC and sealed secrets.
- A connector SDK on MCP.
- Jira, Trello and Gitea.
- Device verify on the iOS Simulator and Android emulator.
- A mobile-first UI.
- A decision engine (Laya or Jev).
- Sessions in a container or a VM.
- A skills and MCP catalog.
- A workflow builder and role templates.
- Bring your own key.
- OTel export and alerts.
- The mod sandbox.

**M3 — v0.3 Smarter**
- Context rotation.
- Company knowledge as scoped, cited context.
- Eval and auto-improve from corrections.
- A design canvas for UI tasks.
- Confluence.
- Data-touched detection and post-merge code quality.

**M4 — v1.0 Paid** (Sun Cloud, closed source)
- Sun Cloud plans: control plane, billing, support.
- Sun credits through a metering gateway.
- Export and sync.

**M5 — Marketplace** (future; waits for a legal review)
- A mod registry and paid mods.
- Rented runtime capacity with escrow and kill switches.

## Quick start

> The release signing key is a **placeholder** and `get-sun.madlabs.tech` serves nothing yet. No public release exists yet. Until one does, build from source (below).

**Install** (Linux x86_64/arm64 with systemd, or macOS arm64 with launchd):

```sh
curl -fsSL https://get-sun.madlabs.tech | sudo sh   # Linux: writes a systemd unit
curl -fsSL https://get-sun.madlabs.tech | sh        # macOS: writes a launchd agent
```

The script detects your OS and architecture and verifies the release signature. It starts `sun serve --with-host` and prints the UI URL and a one-time sign-in link. If anything fails, it undoes every step.

**From source** (Rust is pinned in `rust-toolchain.toml`; the UI needs Node and read access to the private `@hryer/madlabs-design-system` package on GitHub Packages):

```sh
(cd web && npm ci && npm run build)          # the UI the binary embeds
cargo build --release -p sun-cli --bin sun
```

**Run the kernel with a local host:**

```sh
sun doctor                    # checks agent CLIs, git, disk, port, sandbox
sun serve --with-host         # prints "listening on http://127.0.0.1:7700" and a sign-in link
```

**First task** (from a second terminal):

```sh
sun repo add https://github.com/acme/app --description "the web app"
sun task create "Fix login timeout on mobile in app" \
  --body "Sessions expire after resume; add a test first." \
  --workflow default
sun task list
```

**Open the UI** at <http://127.0.0.1:7700>. On first run, sign in as `admin@sun.local` with the token `sun serve` prints (also in `kernel.json`); you then choose your own password (this retires the printed token: `kernel.json` gets a new one, so update anything that copied it into `SUN_TOKEN`) and give your real email. Settings → Account also turns on 2-step verification. Locked out? `sun user set-password <email>`. You'll see the tasks, each task's live timeline, approvals, runtimes, hosts, mods and the catalog.

## CLI cheat sheet

| Command | What it does |
|---|---|
| `sun serve [--with-host] [--listen ADDR \| --port N]` | Run the kernel: API, web UI, engine, queue and mods. `--with-host` also runs a runtime host in the same process, over loopback WebSocket. |
| `sun status [--json]` | Show where this install lives, whether its kernel runs, and kernel and session resource use. |
| `sun doctor` | Check this box: data dir, agent CLIs, git, disk, port, backup freshness (with Litestream), sandbox, service. |
| `sun user set-password <email>` | Set a member's password (prompt or stdin): lockout recovery. |
| `sun host join <token> --kernel URL` · `sun host run` | Enrol a machine with a kernel (the token is single use), then run it. |
| `sun repo add <url> [--description TEXT] [--workspace WS]` · `sun repo list` · `sun repo remove <id>` | A workspace's git repositories; a workflow run works in the ones the task text names. |
| `sun task create <title> [--body] [--workflow] [--container IMG \| --vm TPL] [--priority N]` | Create and queue a task. |
| `sun task list` · `show <id>` · `quality <id>` · `metrics` | Inspect tasks, the data they touched and post-merge quality. |
| `sun task pause\|resume\|cancel <id>` | Control a task's run. |
| `sun task retry <id> [--step KEY]` · `steer <id> <msg> [--queue]` | Retry a failed step (finished steps are kept), or message the running agent. |
| `sun task switch <id> --to <runtime>` · `on-limit <id> [policy]` | Move a running step to another runtime with a hand-off pack, or set the limit policy (`switch`, `pause_and_notify`, `wait_until_reset`). |
| `sun workflow validate <file>` · `save <file> [--id]` | Check a workflow (errors carry `line:column`), or save it as a new version. |
| `sun plugin add\|update\|list\|remove` · `sun extension …` | Install mods from a folder, a package file, `git+https://` or a first-party name. |
| `sun plugin mcp add\|list\|…` | Register the MCP servers agents can use. |
| `sun mod dev <dir>` · `publish` · `yank` | Develop a mod with hot reload, sign and submit it to the registry, or hide a version. |
| `sun skill new <name>` · `sun mcp new <name>` | Scaffold a skill or a dependency-free MCP server. |
| `sun secret set\|list\|rm [--mod M]` | Manage sealed ecosystem or mod secrets. Values come from stdin, never argv. `scripts/load-git-apps.sh [.env]` sets the GitHub App and GitLab OAuth app secrets from a `chmod 600` `.env` (or the environment); `scripts/dev.sh` runs it when `./.env` exists. |
| `sun runtime add <name> --byo-key --kind K --adapter A --model M [--endpoint] [--key-secret]` · `check` · `on-limit` | Add a bring-your-own-key runtime, check it, or set the ecosystem limit policy. |
| `sun knowledge …` · `sun eval …` | Manage knowledge sources and search, or replay past tasks against a changed instruction. |
| `sun upgrade [--check]` · `rollback` · `backup` | Upgrade (wait for a checkpoint, back up, swap, migrate, restart), undo the last upgrade, or back up the store. |
| `sun ecosystem export\|import` | Move a whole ecosystem as one archive. |
| `sun license install\|show\|remove` · `sun market …` | Sun Cloud plans and the marketplace. These need `SUN_CLOUD_URL` and `SUN_CLOUD_TOKEN`. |

Run `sun <command> --help` for every flag.

## Configuration

The data dir is `--data-dir`, else `$SUN_DATA_DIR`, else `~/.sun`. The installer uses `/var/lib/sun` on Linux and `~/Library/Application Support/sun` on macOS. The data dir holds:
- `sun.yaml`;
- `kernel.json` (the local token and host secret, mode 0600);
- the SQLite store;
- content-addressed blobs;
- `host/` and `mods/`.

**`sun.yaml` highlights** (every key is optional):

```yaml
listen: 127.0.0.1:7700
public_url: https://sun.example.com     # for hosts and deep links
default_runtime: claude-code
runtimes:
  permission_mode: accept_edits         # default | accept_edits | plan | bypass_permissions
  template_dir: templates               # per-CLI envelope overrides
caps:
  per_runtime: { claude-code: 2 }
  per_host: { vps: { runs: 4, memory_mb: 8192, cpus: 4 }, "*": { runs: 2 } }
watchdog: { stall_after: 15m, on_stall: nudge }   # nudge | restart | fail
rotate_at_percent: 80                   # context rotation
notify_to: ["slack:#eng"]
worktree_retention_hours: 24
event_retention_days: 90
features: { knowledge: true, canvas: true, evals: true, market: false, catalog: true }
```

Sign-in providers (Google, GitHub, any OIDC) are set up in Settings → Sign-in, with their client secrets sealed in the store. An `oidc:` entry from an older `sun.yaml` is imported there once, with `SUN_OIDC_CLIENT_SECRET`.

Secrets never go in the file. They come from environment variables:
- `SUN_DECISIONS_API_KEY`
- `SUN_MASTER_KEY`, or `<data dir>/master.key`

The store is SQLite only (`<data dir>/sun.db`). Postgres support was removed: `sun` refuses to start while `SUN_DATABASE_URL` holds a `postgres://` URL.

**High availability** is active-passive: Litestream replicates the store, and a standby restores it when the primary is gone. On each node, install `litestream` (plus `rclone` for a remote files replica), copy `packaging/litestream.yml` to `/etc/litestream.yml` (or point `LITESTREAM_CONFIG` at it) and run `scripts/ha.sh start [sun serve args]`. It needs `SUN_DATA_DIR`, `SUN_FILES_REPLICA` (an absolute dir, or an rclone remote such as `s3:my-bucket/sun/files`), `SUN_DB_REPLICA` for Litestream (e.g. `s3://my-bucket/sun/db`, with the usual `AWS_*` credentials) and the same `SUN_MASTER_KEY` on every node. The recommended path is a manual restore: stop the primary (`ha.sh` exits 0 on SIGTERM after a last file sync, so nothing is lost), then run `ha.sh start` on the other node; it restores the latest database and files, and refuses to start on an empty database when a backup should exist. Automatic takeover is **experimental, verified only by `scripts/ha-chaos.sh` on one machine**: a node that finds a live primary waits as a standby and takes over when the primary's heartbeat stops; exit code 3 means another node took over, so start it again to rejoin as a standby. Run `ha.sh` as the service's main process: systemd (default `KillMode=control-group`) and Docker (PID 1) stop or kill `sun` together with it, and if only Litestream dies, `ha.sh` stops Sun itself and exits 1 so the service restarts on the same database. `sun doctor` on the primary checks that the replica has caught up and the file sync is fresh; it needs `SUN_DB_REPLICA` (and the S3 credentials) in its shell, as it cannot discover them.

**A workflow** is a versioned YAML file. This is the bundled bug-fix role template:

```yaml
name: bug-fix
on: [manual]
runtime:
  chain: [claude-code, codex]          # failover order
  on_limit: switch
budget: { wall: 2h, turns: 200, cost_usd: 5 }
steps:
  - { id: reproduce, type: agent, prompt: "Reproduce the bug with a failing test…", skills: [tdd] }
  - { id: fix, type: agent, prompt: "Fix the root cause…", skills: [tdd] }
  - { id: verify, type: gate, gates: [diff_not_empty, test, lint], on_fail: { send_back_to: fix, max: 2 } }
  - { id: pr, type: action, use: github.open_pr }
  - { id: review, type: approval, who: [role:admin], timeout: { after: 24h, then: remind } }
```

The other role templates live in `templates/workflows/`: feature across repos, UI refactor, marketing campaign, PRD, weekly summary and client deployment.

## Architecture

For a plain-language overview with diagrams (the parts, how a task runs, the ways to deploy), see [`docs/`](docs/README.md).

```
 triggers: UI · CLI · REST · cron · webhook · plugin events
                     │
 ┌───────────────────▼────────────── sun serve (kernel, one binary) ───────────────────┐
 │ api (axum: REST, SSE, WS) · auth (local token, OIDC, RBAC) · embedded Svelte UI      │
 │ queue + flow control ─► engine: step state machine ─► transactional outbox           │
 │ watchdog · gates & approvals · failover · decision hooks                              │
 │ context: compiler · hand-off packs · knowledge (FTS5) · `sun` MCP server              │
 │ mod host: plugins & extensions over JSON-RPC stdio                                    │
 │ store: SQLite WAL + content-addressed zstd blobs                                      │
 └──────────────┬────────────────────────────────────────────────┬──────────────────────┘
                │ WebSocket, the host dials out (seq/ack)         │ JSON-RPC 2.0, stdio
 ┌──────────────▼─────────── sun host (same binary) ──┐   ┌───────▼────────────────────┐
 │ sessions · worktrees · exec envs: bare/container/VM │   │ plugins: GitHub, GitLab,   │
 │ adapters: Claude Code · Codex · OpenCode · mods     │   │ Slack, Jira, MCP, …        │
 │ devices: iOS Simulator · Android emulator           │   │ extensions: steps, gates…  │
 └─────────────────────────────────────────────────────┘   └────────────────────────────┘
          Sun Cloud (closed, private repo): control plane · gateway · billing · registry · marketplace
```

**Process model.**
- `sun serve` is the kernel. It owns all state.
- `sun host` is a runtime host. It dials out to the kernel, so a Mac behind NAT just works.
- On one box, `sun serve --with-host` runs both in one process. They still talk over the same WebSocket protocol on loopback, so there is one code path.
- Mods are child processes.

**Crates.** Each crate's `lib.rs` doc comment is its contract.

| Crate | Single responsibility |
|---|---|
| `sun-core` | The shared vocabulary: ids, domain types, the run/step state machines, `SunEvent`, and the boundary traits `RuntimeAdapter`, `HostLink`, `ModLink` and `EngineHandle`. It also holds the secret filter and entitlements. No I/O. |
| `sun-store` | The only crate that touches the database and blobs: migrations, backup, the event log (ledger), the step journal, the outbox, FTS5 and zstd blobs. It exposes Repository traits. |
| `sun-workflow` | Parse and validate workflow YAML (errors carry `line:column`), version by hash, publish the JSON Schema. |
| `sun-engine` | The single writer of run and step transitions: queue, state machine, gates, approvals, watchdog, failover, outbox worker and triggers. |
| `sun-context` | The context pack compiler, hand-off packs, the knowledge index and the `sun` MCP server. |
| `sun-runtime` | One Adapter per agent CLI, normalized to `SunEvent`. Adapters are tested by replaying recorded fixtures. |
| `sun-host` | The host daemon: sessions, worktrees, exec envs, resource accounting, device verify and the on-disk event buffer. |
| `sun-mod` | The mod (Plugin) host: manifests, JSON-RPC stdio, lifecycle, permissions and the OS sandbox. |
| `sun-catalog` | The curated skills and MCP catalog, with stack-based suggestions. |
| `sun-api` | The network edge: REST `/api/v1`, the SSE timeline, auth, the `/host` WebSocket and the embedded UI. |
| `sun-cli` | The `sun` binary. It wires the real implementations into the traits (dependency injection). |

**Dependency rule.** CI enforces this with `scripts/check-deps.sh`.

| Crate | May depend on |
|---|---|
| `sun-core` | nothing (std, serde, serde_json, ulid, chrono, thiserror, async-trait, futures-core, ed25519-dalek) |
| `sun-store`, `sun-workflow`, `sun-runtime` | `sun-core` |
| `sun-mod` | `sun-core`, `sun-mod-sdk` |
| `sun-context` | `sun-core`, `sun-store` |
| `sun-engine` | `sun-core`, `sun-store`, `sun-workflow`, `sun-context` |
| `sun-host` | `sun-core`, `sun-runtime` |
| `sun-catalog` | `sun-core`, `sun-mod` |
| `sun-api` | `sun-core`, `sun-engine`, `sun-context`, `sun-catalog` |
| `sun-cli` | everything |
| `sun-mod-sdk` (`sdk/rust`) | nothing |

`sun-engine` reaches hosts and mods only through the `HostLink` and `ModLink` traits in `sun-core`. `sun-cli` plugs in the real ones.

The wire formats live in the code: the REST routes in `sun-api`, the host frames in `sun_core::host_protocol` and the mod protocol in `sdk/rust/src/protocol.rs`.

## Project layout

```
crates/          the eleven engine crates above
web/             Svelte 5 + Vite UI (Madlabs design system), embedded by sun-api
catalog/mods.json   registry of first-party and community mods (each in its own repo, madlabs-tech/sun-plugin-*)
sdk/             mod SDKs: rust/ (sun-mod-sdk) and typescript/ (Apache-2.0)
templates/       role workflows, plugin/extension/skill/MCP-server scaffolds
catalog/         curated skills and MCP servers (index.json)
schema/          workflow JSON Schema
packaging/       systemd unit and launchd plist
scripts/         install, e2e, footprint, bench, mem-check, check-deps, cli-matrix
bench/           footprint stand-in
tests/           cross-crate fixtures
```

## Development

Read [`AGENTS.md`](AGENTS.md) first. It is short and it is binding. Its main rules:
- **Roles**: a technical manager plans, reviews and merges; subagents write the code, each in its own git worktree and branch. See "Roles" in `AGENTS.md`.
- **snake_case everywhere**, never camelCase: code, JSON, YAML, DB columns and file names. Types are PascalCase.
- **Conventional Commits**: `<type>(<scope>): <summary>`, e.g. `feat(engine): resume runs from the step journal after crash`.
- **DRY, SOLID and ACID.** Every state change is one transaction, side effects go only through the outbox, and migrations are never edited.
- **Named patterns**: Adapter for runtimes, State machine for runs, Transactional outbox, Repository, Event sourcing + Observer, Strategy / Chain of Responsibility for failover and gates, Plugin for mods, DI in `sun-cli`.
- **UI** uses the Madlabs design system only: dark, `data-product="orchestrator"`, Lucide icons, no emoji, contrast ≥ 4.5:1, from 360 px up.

**Before every commit:**

```sh
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
(cd web && npm run check)        # if web/ changed
```

**Resources.** The dev machine is shared, so keep 1 GB of RAM free:
- Run `scripts/mem-check.sh <need_gb> --wait` before any heavy step: a cargo build or test, Playwright or a VM.
- Limit parallelism with `CARGO_BUILD_JOBS=4 RUST_TEST_THREADS=4 CARGO_INCREMENTAL=0`.

**Run it all locally:** `bash scripts/dev.sh` builds the UI and the kernel, then runs the kernel (port 7700) and the vite dev server; Ctrl-C stops both (`--help` for `--no-web`, `--reset`).

**End-to-end checks:**
- `bash scripts/e2e.sh` runs the real binary through a whole run, then a failover to a second fake runtime, gates, a PR, an approval, and `kill -9` crash resumes.
- `bash scripts/footprint.sh` gates binary size, idle RSS and cold start.
- `bash scripts/check-deps.sh` enforces the crate dependency table.
- `bash scripts/cli-matrix.sh` re-records the CLI fixtures. It needs API keys.

## Footprint

`scripts/footprint.sh` on an Apple M4, macOS arm64, 2026-10-07. Release `sun` with the UI embedded, `serve --with-host`, no sessions:

| Metric | Measured | Budget |
|---|---|---|
| Binary size (with UI) | **25.7 MB** | ≤ 40 MB |
| Idle RSS | **21.5 MB** | ≤ 40 MB |
| Cold start to API | **38 ms** | ≤ 300 ms |

The agent CLI dominates memory, not the kernel. One idle `claude -p` session uses about **235 MB RSS**, and sessions share nothing. On a 1 GB VPS that means 1–2 concurrent sessions; 5 sessions want 2 GB or more. Linux and VPS numbers are not measured yet.

## Status and known limits

The code side of every story is done; what follows is still unproven.

**Needs real accounts or hardware (external):**
- A real quota hit per CLI. The OpenCode limit fixture is synthetic. This is also the M1 exit criterion.
- A real Xcode Simulator and Android emulator run, including the H.264 live view.
- A real Lima/QEMU VM with the egress allowlist.
- A green Linux CI run of the Landlock/seccomp mod sandbox.
- Real cloud hosting for the control plane, and live Xendit/Stripe payments.
- A timed install on a fresh Linux VPS, and a VPS benchmark that includes the agent CLIs.
- The default `on_limit` policy, which waits for user interviews.
- A legal review before the marketplace (M5).

**Placeholders:**
- `SUN_RELEASE_KEY` in `scripts/install.sh` (the release host `get-sun.madlabs.tech` serves nothing until the first release).
- Release and license signing keys.

**Never run:**
- The GitHub Actions workflows (`ci`, `footprint`, `release`, `cli-matrix`). Nothing has been pushed.

**Known limits:**
- Integrations are tested against mock servers only.
- Extension runtimes and exec envs reach only the in-process host.
- Cron runs in UTC.
- Spend caps count only the cost a runtime reports. Subscriptions report none.
- Same-provider account rotation is deliberately not shipped.

## Licensing

Sun has two licenses:
- **The engine** (everything outside `sdk/`): **FSL-1.1-ALv2**. Each release turns Apache-2.0 after two years. You can read, run, self-host and modify it, but not resell it as a competing hosted service.
- **The mod SDKs** (`sdk/rust`, `sdk/typescript`, each with its `LICENSE`) and **the runtime daemon**: **Apache-2.0**.

The engine's license is in `LICENSE`; each SDK carries its own.
