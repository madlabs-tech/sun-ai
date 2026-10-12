# Sun: high-level overview

Sun is one Rust binary, `sun`. It runs the agent CLIs you already have (Claude Code, Codex, OpenCode) through versioned workflows. The engine, not the agent, decides when a task is done.

This page shows the parts and how they talk. Two more pages go one level deeper:
- [Task lifecycle](task-lifecycle.md): how one task runs, from create to done.
- [Deployment](deployment.md): the ways to run it.

The crate table and the dependency rule are in the README, under [Architecture](../README.md#architecture).

## The picture

```text
 Browser (web UI)       sun CLI       scripts · webhooks     agents, via `sun mcp`
        │                  │                  │                      │
        │ HTTP + SSE       │ REST /api/v1     │ REST /api/v1         │ WebSocket /mcp
        ▼                  ▼                  ▼                      ▼
 ┌────────────────────────────────────────────────────────────────────────┐   ┌──────────────┐
 │      sun serve: the kernel (API · engine · queue · embedded web UI)    │──>│ SQLite WAL + │
 └──────────┬──────────────────────────────────────────────┬──────────────┘   │ zstd blobs   │
            │ JSON-RPC 2.0 over stdio                      │                  │ (data dir)   │
            ▼                                              │                  └──────────────┘
 ┌──────────────────────┐                                  │ WebSocket /host, seq/ack:
 │ mods (child procs):  │                                  │ the host dials out
 │ GitHub · GitLab ·    │                                  │
 │ Slack · Jira · …     │                          ┌───────┴────────┐
 └──────────────────────┘                          │    sun host    │  runs next to your code:
                                                   └───────┬────────┘  repos and worktrees
                                                           │ spawns one session per agent step
                                    ┌──────────────────────┴───────────────────────┐
                                    │  Claude Code · Codex · OpenCode · BYO key    │
                                    └──────────────────────────────────────────────┘

 ┌ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ┐
   Sun Cloud (optional, closed source, private repo): control plane · credits
 │ gateway · billing · mod registry · marketplace. Used only when SUN_CLOUD_URL  │
   and SUN_CLOUD_TOKEN are set. Self-hosting needs none of it.
 └ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ┘
```

## The boxes

**Browser (web UI).**
- A Svelte 5 + Vite app in `web/`, built to `web/dist` and embedded in the binary. `sun-api` serves it at `/`.
- The live run timeline is SSE: `GET /api/v1/runs/{id}/events` with `Accept: text/event-stream`.

**`sun` CLI.**
- The same binary (`crates/sun-cli`).
- Client commands (`sun task`, `sun repo`, …) call the kernel's REST API with the local token from `kernel.json`, or `SUN_TOKEN`.
- Install-time commands (`sun plugin`, `sun secret`) write the store directly and take effect on the next `sun serve`.

**Scripts and webhooks.**
- Anything can call REST under `/api/v1`.
- A workflow with `webhook` in its `on:` gets one task per `POST /api/v1/workflows/{id}/trigger`.
- Mods receive outside webhooks at `/api/v1/webhooks/{mod_name}`.
- Cron (`cron:<expr>`, UTC) and mod events (e.g. `jira.issue_labeled:agent`) fire inside the kernel: `crates/sun-engine/src/triggers.rs`.

**Agents, via `sun mcp`.**
- Each agent session runs `sun mcp` as its `sun` MCP server. It is a stdio shim that dials the kernel's `/mcp` WebSocket with a per-session token (`SUN_MCP_TOKEN`).
- Agents use it to pull more context and to search knowledge. Code: `crates/sun-cli/src/mcp.rs`, `crates/sun-api/src/mcp.rs`, `crates/sun-context`.

**`sun serve`: the kernel.**
- It owns all state. Listens on `127.0.0.1:7700` by default (`listen` in `sun.yaml`, `--listen` or `--port`).
- It speaks plain HTTP. Sun does not terminate TLS itself.
- Inside: `sun-api` (REST, SSE, auth, `/host`, `/mcp`, the UI), `sun-engine` (queue, step state machine, gates, approvals, watchdog, failover, outbox, triggers), `sun-context` (context packs, hand-off packs, knowledge), `sun-workflow` and `sun-catalog`.

**Store.**
- `sun-store` is the only crate that touches it.
- `<data dir>/sun.db` is SQLite in WAL mode; blobs are content-addressed and zstd-compressed.
- The data dir is `--data-dir`, else `$SUN_DATA_DIR`, else `~/.sun`. A backup goes to `<data dir>/backups/` before any migration.

**Mods.**
- Child processes of the kernel, speaking JSON-RPC 2.0 over stdio (`crates/sun-mod`).
- Plugins connect outside services; extensions add steps, gates and runtimes.
- First-party mods (github, gitlab, gitea, slack, jira, trello, confluence, gdrive, otel, sync) are each in its own repo, listed in `catalog/mods.json`; the SDKs are in `sdk/`.
- Installed mods run in an OS sandbox: `sandbox-exec` on macOS, namespaces with Landlock and seccomp on Linux. The Linux sandbox has **not had a green CI run yet**.
- The engine reaches mods only through the `ModLink` trait; side effects go through the outbox (see [Task lifecycle](task-lifecycle.md)).

**`sun host`.**
- Runs agent sessions on a machine (`crates/sun-host`): bare clones under `repos/`, one worktree per task on branch `sun/<task_id>`, exec envs (bare, container, VM) and device verify (iOS Simulator, Android emulator).
- Enrol once with `sun host join <token> --kernel URL` (a single-use token from the UI's "Add host"), then `sun host run`.
- It dials out to `ws(s)://<kernel>/host`, so a machine behind NAT works.
- Every event carries `(session_id, seq)` and stays in the on-disk `buffer/` until the kernel acks it.
- The wire format is `sun_core::host_protocol`.

**Agent CLIs.**
- One Adapter per CLI in `crates/sun-runtime`: Claude Code, Codex, OpenCode. Each turns CLI output into `SunEvent`; a quota message becomes `SunEvent::LimitHit`.
- A bring-your-own-key runtime (`sun runtime add … --byo-key`) runs through OpenCode with a provider config.
- An extension can serve a runtime too, but extension runtimes reach only the in-process host for now.
- A real quota hit has **not been tested** against a real account for any CLI.

**Sun Cloud (optional).**
- A separate, closed-source cargo workspace in a private repo.
- The kernel talks to it only when `SUN_CLOUD_URL` and `SUN_CLOUD_TOKEN` are set: credits (a short-lived gateway key per session), plan licenses and the marketplace.
- Real cloud hosting and live payments are **unproven**.

## Process model

- `sun serve` is the kernel. It owns all state.
- `sun host` is a runtime host. It dials out to the kernel.
- `sun serve --with-host` runs both in one process. The host still dials `ws://<listen>/host` over loopback, with the same protocol, so there is one code path.
- Mods are child processes of the kernel. Agent CLIs are child processes of the host.
