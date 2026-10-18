# CLI reference

Every command of the `sun` binary, one line each. Run `sun <command> --help` for every flag. Back to the [overview](README.md).

Commands marked *kernel* talk to a running kernel over REST: by default `http://<listen>` from `sun.yaml` with the local token in `kernel.json`, or `--url` / `SUN_URL` and `--token` / `SUN_TOKEN`. Every command takes `--data-dir` (else `$SUN_DATA_DIR`, else `~/.sun`).

## Run and operate

| Command | What it does |
|---|---|
| `sun serve [--with-host] [--listen ADDR \| --port N] [--no-devices]` | Run the kernel: API, web UI, engine, queue and mods. `--with-host` also runs a runtime host in the same process. |
| `sun status [--json]` | Where this install lives, whether its kernel runs, and its resource use. |
| `sun doctor` | Check this machine: data dir, agent CLIs, git, disk, port, backups, sandbox, service. |
| `sun upgrade [--check] [--version V]` · `sun rollback` | Upgrade: wait for a checkpoint, back up, swap the binaries, migrate, restart. `--check` only lists what would break. `rollback` undoes the last upgrade. |
| `sun backup` | Copy the store to `<data dir>/backups` (safe while running). |
| `sun ecosystem export\|import` | Move a whole ecosystem between data dirs as one archive. |

## Hosts

| Command | What it does |
|---|---|
| `sun host join --kernel KERNEL_URL [--name NAME] <token>` | Enrol this machine with a kernel (token from **Hosts → Add host**, single use), install and start the host service, and wait until the kernel sees it. `--no-service` enrols only. |
| `sun host stop` · `start` | Stop the host service until the next login or boot; start it again. |
| `sun host leave` | Remove the host service, take the host out of the kernel's workspaces and delete its credentials. |
| `sun host run` | Run the enrolled host in the foreground (for your own supervisor). |

## Work *(kernel)*

| Command | What it does |
|---|---|
| `sun repo add <url> [--description TEXT] [--branch B] [--workspace WS]` · `list` · `remove <id>` | A workspace's git repositories. A run works in the repos the task text names. |
| `sun task create <title> [--body] [--workflow] [--priority] [--container IMG \| --vm TPL]` | Create and queue a task (default workflow: `default`). |
| `sun task list` · `show <id>` · `quality <id>` · `metrics` | Inspect tasks, their runs and post-merge quality. |
| `sun task pause\|resume\|cancel <id>` | Control a task's run. |
| `sun task retry <id> [--step KEY]` · `steer <id> <message>` | Retry a failed step (finished steps are kept), or message the running agent. |
| `sun task switch <id> --to <runtime>` · `on-limit <id> [policy]` | Move a running step to another runtime, or set what happens at a usage limit (`switch`, `pause_and_notify`, `wait_until_reset`). |
| `sun workspace list\|create\|rename\|delete` | Workspaces, each with its own workflows and tasks. |
| `sun knowledge add\|list\|rm\|ingest\|reindex\|search` | Knowledge sources, uploads and search. |
| `sun eval run\|list\|show` | Replay past tasks against a changed instruction, memory or workflow. |

## Workflows

| Command | What it does |
|---|---|
| `sun workflow validate <file>` | Check a workflow file; errors carry `line:column`. |
| `sun workflow save <file> [--id ID]` · `list` | Save a new workflow or the next version of one; list them. |

## People and settings *(kernel)*

| Command | What it does |
|---|---|
| `sun user list\|invite\|reinvite\|promote\|deactivate\|activate` | Members of the deployment. |
| `sun user set-password <email>` | Set a member's password (lockout recovery). |
| `sun smtp set\|show\|test` | The mail server invitations go through. |
| `sun credential add\|check\|rm` | API key credentials that pay for sessions: add one (its key lives in a named secret), check it can pay, remove it. |
| `sun runtime check` · `on-limit` | A tool's readiness, and the default usage-limit policy. |

## Mods, skills and secrets

| Command | What it does |
|---|---|
| `sun plugin add\|update\|list\|remove` · `sun extension ...` | Install mods from a folder, a package file, `git+https://` or a registry name (e.g. `github`). |
| `sun plugin mcp add\|tools\|list\|remove` | MCP servers agents can use. |
| `sun mod dev <dir>` | Develop a mod with hot reload. |
| `sun skill new <name>` | Scaffold a skill. |
| `sun secret set\|list\|rm [--mod M \| --workspace W]` | Sealed secrets. Values come from stdin, never argv. Back up `<data dir>/master.key` (or set `SUN_MASTER_KEY`). |
| `sun market ...` | The Sun Cloud marketplace. Needs `SUN_CLOUD_URL` and `SUN_CLOUD_TOKEN`. |
| `sun mcp` | The `sun` MCP server an agent session runs; the engine starts it. |
