# Sun

Sun runs the agent CLIs you already use, Claude Code, Codex and OpenCode, through versioned workflows. You hand an agent a task; Sun runs it on a machine you choose, records every step, checks the result with gates, and asks a person when a step needs one. It is for individuals and small teams. Sun is one binary, `sun`, with the web UI built in. You can self-host it, or join your machines to Sun Cloud, the hosted kernel at <https://sun.madlabs.tech>.

## Contents

- [Install](#install)
- [Quick start](#quick-start)
- [Modes](#modes)
- [Uninstall](#uninstall)
- [Configuration](#configuration)
- [Workflows](#workflows)
- [Architecture](#architecture)
- [Development](#development)
- [Limitations](#limitations)
- [License](#license)

## Install

Linux (x86_64 or arm64, with systemd):

```sh
curl -fsSL https://get-sun.madlabs.tech | sudo sh
```

macOS (Apple silicon):

```sh
curl -fsSL https://get-sun.madlabs.tech | sh
```

The installer:

1. downloads the latest release for your OS and CPU, and checks its signature and checksums;
2. installs `sun` and `sun-mod-sandbox` (`/usr/local/bin` on Linux, `~/.local/bin` on macOS);
3. installs a service that runs `sun serve --with-host`: a systemd unit `sun` that starts at boot (Linux, as a `sun` system user), or a launchd agent `dev.sun.kernel` that starts at login (macOS);
4. waits for the kernel, then prints the UI URL and your first sign-in.

If a step fails, it says why, undoes what it did and exits non-zero.

Add `--no-service` to install the binaries only (`... | sudo sh -s -- --no-service`). Other options: `--version V`, `--prefix DIR`, `--data-dir DIR`. To remove Sun, see [Uninstall](#uninstall).

## Quick start

You need one agent CLI on the machine that runs the work, logged in: `claude`, `codex` or `opencode`. `sun doctor` tells you if none is found.

1. **Sign in.** Open the URL the installer printed (default <http://127.0.0.1:7700>). Sign in as `admin@sun.local` with the printed password. Sun then asks you to choose your own password, which retires the printed one, and to add your email. A short "Set up Sun" dialog asks your role and what to do when a runtime hits its usage limit.
2. **Create an agent.** Go to **Agents → New agent**. Give it a name, optional instructions, and its runtimes in failover order (for example `claude-code`). It shows **Ready** once one of those runtimes is installed and logged in on an online host.
3. **Give it a task.** Click **New task**, write a title and description, and pick the agent as the assignee. Tasks with no assignee run nothing.
4. **Watch it run.** The task page shows the status, a live execution log (runtime, cost, tokens, context use), and a timeline of every event: tool calls, file changes, gates. A comment on the task wakes the agent, which reads it and picks the task up again.
5. **Get the result.** Files the agent writes appear under **Artifacts** on the task page, ready to download. When the agent is done, the task moves to **Needs you** for your review. A task in a repository works on its own branch, `sun/<task id>`.

From the CLI, on the kernel's machine, a code task looks like this:

```sh
sun repo add https://github.com/acme/app --description "the web app"
sun task create "Fix the login timeout in app" --body "Add a test first."
sun task list
sun task show <id>
```

`sun task create` runs the `default` workflow: implement, run the gates (`diff_not_empty`, `test`, `lint`), open a pull request, then wait for an admin's approval. A run works in the repos the task names. Opening the pull request needs the GitHub plugin: `sun plugin add github`, then `sun secret set GITHUB_TOKEN --mod github` (it reads the token from a prompt or stdin).

Locked out? `sun user set-password <email>`. Every command has `--help`; the full list is in [`docs/cli.md`](docs/cli.md).

## Modes

Every mode runs the same binary. What changes is where the kernel runs and where agents run.

### Self-host the kernel (VPS, Mac mini)

```sh
curl -fsSL https://get-sun.madlabs.tech | sudo sh   # Linux
```

- **Data** lives in `/var/lib/sun` on Linux and `~/Library/Application Support/sun` on macOS: the SQLite store, `sun.yaml`, `kernel.json` (the local token, mode 0600) and backups.
- **Agent CLIs** on Linux run as the `sun` user, so install and log in to them as that user. For Claude Code:

  ```sh
  sudo -u sun -H bash -c 'curl -fsSL https://claude.ai/install.sh | bash'   # into /var/lib/sun/.local/bin
  sudo -u sun -H /var/lib/sun/.local/bin/claude                               # log in once
  ```

- **Reach the UI.** The kernel listens on `127.0.0.1:7700`. From your laptop: `ssh -L 7700:127.0.0.1:7700 <server>`, then open <http://127.0.0.1:7700>. To serve hosts and browsers directly, set `listen` and `public_url` in `sun.yaml` and put your own TLS proxy in front; `sun serve` speaks plain HTTP.
- **Check the box:** `sun doctor` checks the data dir, agent CLIs, git, disk, port, backups, sandbox and service.
- **Upgrade:** `sun upgrade --check` lists what would break; `sun upgrade` waits for running steps to reach a checkpoint, backs up the store, swaps the binaries, migrates and restarts. `sun rollback` undoes the last upgrade.

### Join a machine as a host

A host runs agent sessions for a kernel: your own kernel, or Sun Cloud. It dials out to the kernel, so a laptop behind NAT works with no open port.

1. In the UI, go to **Hosts → Add host**. The dialog shows the command with a single-use token, valid for a limited time.
2. On the machine, install `sun` if it isn't there (`curl -fsSL https://get-sun.madlabs.tech | sudo sh -s -- --no-service` on Linux, without `sudo` on macOS).
3. Run the command from the dialog:

   ```sh
   sun host join --kernel https://sun.example.com --name build-box <token>
   ```

That one command enrols the machine, installs and starts a background service that runs `sun host run`, and waits until the kernel sees the host. The service is a launchd agent `dev.sun.host` on macOS, or a systemd unit `sun-host` on Linux (a user unit, or a system unit when run as root). It starts again at login or boot. For a Linux user unit to keep running after you log out, Sun turns on lingering for you, or prints the `loginctl enable-linger` command to run.

- `sun host stop` stops the service until the next login or boot; `sun host start` starts it again.
- `sun host leave` removes the service, takes the host out of the kernel's workspaces and deletes its credentials.
- `--no-service` only enrols; run `sun host run` under your own supervisor.

### Run everything locally

Kernel and host on one laptop, as yourself, using the agent CLIs you are already logged in to.

- **macOS:** the [install](#install) one-liner is this mode. The launchd agent runs as you.
- **Linux:** install the binaries only, then run the kernel in a terminal. Data goes to `~/.sun`.

  ```sh
  curl -fsSL https://get-sun.madlabs.tech | sudo sh -s -- --no-service
  sun serve --with-host
  ```

`sun serve --with-host` runs the kernel and a host in one process and prints the sign-in line. Stop it with Ctrl-C.

## Uninstall

| Command | Removes | Keeps |
|---|---|---|
| `curl -fsSL https://get-sun.madlabs.tech \| sudo sh -s -- --uninstall` | the `sun` service, `sun` and `sun-mod-sandbox` | the data dir and the `sun` user |
| `... --uninstall --purge` | the same, plus the data dir | the `sun` user (`sudo userdel sun` removes it) |
| `sun host leave` | a joined host's service and credentials | the `sun` binary, and cloned repos under `<data dir>/host` |

On macOS, run the same commands without `sudo`; they remove the launchd agent and `~/.local/bin/sun`. On a joined host, run `sun host leave` before `--uninstall`.

## Configuration

The data dir is `--data-dir`, else `$SUN_DATA_DIR`, else `~/.sun`. When `~/.sun` doesn't exist, the `sun` CLI uses the installed service's data dir. Settings live in `<data dir>/sun.yaml`. Every key is optional, and unknown keys are refused, so a typo fails loudly. The keys most people set:

```yaml
listen: 127.0.0.1:7700                  # where `sun serve` listens
public_url: https://sun.example.com     # how hosts and links reach the kernel
default_runtime: claude-code            # for workflows without `runtime:`
runtimes:
  permission_mode: accept_edits         # default | accept_edits | plan | bypass_permissions
  allow_bypass: false                   # required before bypass_permissions can be chosen
caps:
  per_runtime: { claude-code: 2 }       # active runs per runtime; extra tasks queue
  per_host: { "*": { runs: 2 } }        # active runs per host
watchdog: { stall_after: 15m, on_stall: nudge }   # nudge | restart | fail
worktree_retention_hours: 24            # default 0: removed when a task is done
event_retention_days: 90                # default: keep forever
notify_to: ["slack:#eng"]               # where approvals and stalls are announced
```

**Permissions.** Each agent CLI has a permission mode. `runtimes.permission_mode` in a host's `sun.yaml` is that host's default (`accept_edits`). A deployment owner can set the mode per runtime on the **Runtimes** page, which applies to every new session of that runtime on any host. Claude Code takes the mode as a flag. OpenCode's permission requests are answered by it. Codex runs in its own `workspace-write` sandbox. `bypass_permissions` lets the agent run any command, so it needs `allow_bypass: true` first, and it only runs in a container or VM: the queue fails a run that would get it bare (`placement_refused_bypass_bare`), and a host refuses such a session on its own too.

**Secrets** never go in `sun.yaml`. `sun secret set` seals them in the store, with the key in `<data dir>/master.key` or `SUN_MASTER_KEY`. Back that key up: losing it loses every secret. Sign-in providers (Google, GitHub, any OIDC) and SMTP are set in **Settings**.

**High availability** (active-passive with Litestream) is described in [`docs/deployment.md`](docs/deployment.md#3-high-availability-active-passive).

## Workflows

A workflow is a versioned YAML file: a list of steps. Step types are `agent`, `gate`, `action`, `approval`, `wait`, `notify` and `decide`; mods can add more. The engine, not the agent, decides when a step is done: a `gate` step runs checks, and on failure sends the work back to an earlier step a limited number of times. Code gates include `diff_not_empty`, `test`, `lint`, `diagnostics_clean`, `pr_exists` and `ci_green`. Content gates for non-code work include `sections_present`, `length`, `format` and `links_resolve`.

This is the built-in `default` workflow (`crates/sun-workflow/defaults/default.yaml`):

```yaml
name: default
on: [manual]
steps:
  - id: implement
    type: agent
    prompt: |
      Implement the task. Keep the change focused, add or update tests for it,
      and run them before you say you are done.

  - id: verify
    type: gate
    gates: [diff_not_empty, test, lint]
    on_fail: { send_back_to: implement, max: 2 }

  - id: pr
    type: action
    use: github.open_pr

  - id: review
    type: approval
    who: [role:admin]
    timeout: { after: 24h, then: remind }
```

`runtime: { chain: [claude-code, codex], on_limit: switch }` sets the failover order: when a runtime hits its usage limit, the step moves to the next one in the same worktree. `on:` also takes `cron:<expr>` (UTC) and `webhook`. More examples are in [`templates/workflows/`](templates/workflows/): bug fix, feature across repos, UI refactor, marketing campaign, PRD, weekly summary, client deployment. A repo can keep its own in `.sun/workflows/`. `sun workflow validate <file>` checks one; errors carry `line:column`.

## Architecture

```
 UI · CLI · REST · cron · webhooks
              │
   sun serve (kernel) ── API, engine, queue, gates, approvals, mods ── SQLite store
              │  WebSocket; each host dials out
   sun host ── worktrees, sessions ── Claude Code · Codex · OpenCode
```

- **Kernel** (`sun serve`) owns all state: the task queue, the step state machine, gates, approvals and the store (SQLite plus compressed blobs in the data dir). Every step change is one transaction, so a crash or restart resumes a run without redoing finished steps.
- **Hosts** (`sun host`) run agent sessions, one git worktree per task. `sun serve --with-host` runs a host in the kernel's process, over the same protocol.
- **Runtimes** are the agent CLIs, each behind an adapter that turns its output into Sun events.
- **Mods** (plugins and extensions: GitHub, GitLab, Slack, Jira, ...) are child processes of the kernel, run in an OS sandbox.

More, with diagrams: [`docs/`](docs/README.md).

## Development

Read [`AGENTS.md`](AGENTS.md) first; its rules are binding. Rust is pinned in `rust-toolchain.toml`. The UI needs Node.

```sh
(cd web && npm ci && npm run build)        # the UI the binary embeds
cargo build --release -p sun-cli --bin sun
```

Before a commit:

```sh
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
(cd web && npm run check)                  # if web/ changed
```

- `bash scripts/dev.sh` builds the UI and the kernel, then runs the kernel on port 7700 and the Vite dev server. Ctrl-C stops both.
- `bash scripts/e2e.sh` runs the real binary through a whole run, failover, gates, a PR, an approval and `kill -9` crash resumes.
- `bash scripts/footprint.sh` checks binary size, idle RSS and cold start against the release budget (40 MB binary, 40 MB idle RSS).
- `bash scripts/check-deps.sh` enforces the crate dependency rule.
- `dist plan` shows the release artifacts; a pushed `v*` tag builds, signs and publishes a release.

Crates, the dependency rule and the project layout: [`docs/README.md`](docs/README.md#crates).

## Limitations

- **Platforms:** Linux x86_64 and arm64, and macOS on Apple silicon. Intel Macs and Windows are not supported.
- **Agent CLIs:** Claude Code, Codex and OpenCode. Bring-your-own-key runtimes run through OpenCode. You install and log in to the CLIs yourself.
- **Service install** on Linux needs systemd. Without it, use `--no-service` and your own supervisor.
- **TLS:** `sun serve` speaks plain HTTP. Put your own proxy in front for `https://` and `wss://`.
- **Extensions:** runtimes and execution environments that an extension adds run only on the kernel's own host (`sun serve --with-host`), not on a joined host.
- **Cron** schedules are in UTC.
- **Footprint:** the v0.1.0 release measured a 36.5 MiB binary and about 30 MiB idle RSS on Linux x86_64 (`footprint.json` in the release assets). Each agent CLI session uses its own memory on top of that.

## License

- The engine (everything outside `sdk/`): [FSL-1.1-ALv2](LICENSE). You can read, run, self-host and modify it, but not offer it as a competing hosted service. Each release becomes Apache-2.0 two years after it is published.
- The mod SDKs (`sdk/rust`, `sdk/typescript`): Apache-2.0, each with its own `LICENSE`.
