//! The `sun` binary: the command line. Wiring lives in the `sun_cli` library.

use std::io::{IsTerminal, Read};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use serde_json::{Value, json};
use sun_cli::client::Client;
use sun_cli::config::{Config, KernelSecrets};
use sun_cli::mods;
use sun_cli::ops;
use sun_cli::serve::{self, ExecEnvTools, ServeOptions};
use sun_core::model::{Priority, SecretScope};
use sun_store::{SecretRepo, Store};

/// Sun: one kernel that runs your coding agents through workflows.
#[derive(Parser)]
#[command(name = "sun", version)]
struct Cli {
    /// Data dir (store, token, host, mods). Default `~/.sun`.
    #[arg(long, global = true, env = "SUN_DATA_DIR")]
    data_dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the kernel: API, web UI, engine, queue and mods.
    Serve {
        /// Also run a runtime host in this process (over loopback WebSocket).
        #[arg(long)]
        with_host: bool,
        /// Listen address; overrides `listen` in sun.yaml.
        #[arg(long)]
        listen: Option<String>,
        /// Listen on this port of the `listen` host; 0 picks a free one. The
        /// bound address is written to <data dir>/address either way.
        #[arg(long, conflicts_with = "listen")]
        port: Option<u16>,
        /// Run installed mods without the OS sandbox (mod development only).
        #[arg(long)]
        no_mod_sandbox: bool,
        /// The `--with-host` host's container/VM tools.
        #[command(flatten)]
        exec_env_tools: ExecEnvTools,
    },
    /// Where this install lives and whether its kernel runs (`--json` for
    /// scripts).
    Status {
        /// Print one JSON object.
        #[arg(long)]
        json: bool,
    },
    /// The plan license (Sun Cloud plans; none = self-hosted Community).
    #[command(subcommand)]
    License(LicenseCommand),
    /// Move a whole ecosystem between data dirs (one archive file).
    #[command(subcommand)]
    Ecosystem(EcosystemCommand),
    /// Run or enrol a runtime host.
    #[command(subcommand)]
    Host(HostCommand),
    /// A workspace's git repositories (through the running kernel).
    #[command(subcommand)]
    Repo(RepoCommand),
    /// Create and inspect tasks (through the running kernel).
    #[command(subcommand)]
    Task(TaskCommand),
    /// Workspaces: each has its own workflows and tasks (through the running
    /// kernel; writes need an admin).
    #[command(subcommand)]
    Workspace(WorkspaceCommand),
    /// Check and save workflow files.
    #[command(subcommand)]
    Workflow(WorkflowCommand),
    /// Knowledge sources, uploads and search (through the running kernel).
    Knowledge {
        #[command(subcommand)]
        command: sun_cli::knowledge::KnowledgeCommand,
        /// The workspace (id or name) to add to, list, remove from or search;
        /// default Main (`list` without it shows every workspace's sources).
        #[arg(long, global = true)]
        workspace: Option<String>,
        #[command(flatten)]
        remote: Remote,
    },
    /// Replay past tasks against a changed instruction, memory or workflow.
    Eval {
        #[command(subcommand)]
        command: sun_cli::eval::EvalCommand,
        #[command(flatten)]
        remote: Remote,
    },
    /// Plugins: integrations with outside services.
    #[command(subcommand)]
    Plugin(PluginCommand),
    /// Extensions: new engine capabilities.
    #[command(subcommand)]
    Extension(ModCommand),
    /// Mod development.
    #[command(subcommand)]
    Mod(DevCommand),
    /// Sun Cloud marketplace: sell this host's runtime capacity, get paid,
    /// install marketplace mods (needs SUN_CLOUD_URL and SUN_CLOUD_TOKEN).
    #[command(subcommand)]
    Market(MarketCommand),
    /// Skill authoring (install skills from the web UI's Catalog).
    #[command(subcommand)]
    Skill(SkillCommand),
    /// Ecosystem and mod secrets (sealed in the store; values are never
    /// printed). Back up <data dir>/master.key (or set SUN_MASTER_KEY):
    /// losing it loses every secret.
    #[command(subcommand)]
    Secret(SecretCommand),
    /// Members: invite, re-invite, remove (through the running kernel).
    User {
        #[command(subcommand)]
        command: sun_cli::users::UserCommand,
        #[command(flatten)]
        remote: Remote,
    },
    /// The mail server invites go through (through the running kernel).
    Smtp {
        #[command(subcommand)]
        command: sun_cli::users::SmtpCommand,
        #[command(flatten)]
        remote: Remote,
    },
    /// BYO key runtimes (through the running kernel).
    #[command(subcommand)]
    Runtime(RuntimeCommand),
    /// Check this box: data dir, agent CLIs, git, disk, port, sandbox, service.
    Doctor,
    /// Upgrade: wait for a checkpoint, back up, swap binaries, migrate, restart.
    Upgrade {
        /// Only list what would break (incompatible mods, release notes).
        #[arg(long)]
        check: bool,
        /// Install from a folder holding the new `sun` instead of downloading.
        #[arg(long)]
        from: Option<PathBuf>,
        /// The release to download. Default: the latest.
        #[arg(long)]
        version: Option<String>,
        /// Seconds to wait for running steps to reach a checkpoint.
        #[arg(long, default_value_t = 600)]
        wait: u64,
        /// Run by the new binary during an upgrade.
        #[arg(long, hide = true)]
        check_local: bool,
    },
    /// Undo the last upgrade: previous binaries and the pre-upgrade store.
    Rollback,
    /// Copy the store to <data dir>/backups (safe while running).
    Backup,
    /// Apply pending store migrations (run by `sun upgrade`).
    #[command(hide = true)]
    Migrate,
    /// Print the local-only features this machine can use: simulators and
    /// VMs on a local install, nothing on a VPS (run by `install.sh`).
    #[command(hide = true)]
    Offers,
    /// The `sun` MCP server over stdio, for an agent session (the engine
    /// writes the config that runs it). The session token is read from
    /// `SUN_MCP_TOKEN`, never argv.
    Mcp {
        /// The kernel's base URL.
        #[arg(long)]
        kernel: Option<String>,
        #[command(subcommand)]
        sub: Option<McpCommand>,
    },
}

#[derive(Subcommand)]
enum SkillCommand {
    /// Scaffold `<dir>/<name>/SKILL.md` from the skill template.
    New {
        /// Skill name, `^[a-z0-9]+(-[a-z0-9]+)*$`; also the folder name.
        name: String,
        /// The folder to create it in.
        #[arg(long, default_value = ".")]
        dir: PathBuf,
    },
}

#[derive(Subcommand)]
enum McpCommand {
    /// Scaffold `<dir>/<name>/server.mjs`, a dependency-free MCP server
    /// (Node), and print how to register it.
    New {
        /// Server name, `[a-z][a-z0-9_-]*`; also the folder name.
        name: String,
        /// The folder to create it in.
        #[arg(long, default_value = ".")]
        dir: PathBuf,
    },
    /// A registered MCP server filtered to a grant, served from this data
    /// dir (debugging; sessions use `relay`).
    #[command(hide = true)]
    Proxy {
        /// The grant as JSON: `{"server": "docs", "tools": [..]?}`.
        grant: String,
    },
    /// A session's granted MCP server through the kernel, with the
    /// token in SUN_MCP_TOKEN. The engine writes the config that runs it.
    #[command(hide = true)]
    Relay {
        /// The grant as JSON; the kernel uses its own copy.
        grant: String,
    },
}

#[derive(Subcommand)]
enum PluginCommand {
    #[command(flatten)]
    Mod(ModCommand),
    /// MCP servers agents can use: add, list, show tools, remove.
    #[command(subcommand)]
    Mcp(sun_cli::mcp::McpServerCommand),
}

#[derive(Subcommand)]
enum HostCommand {
    /// Enrol this machine with a kernel, using a token from the UI's
    /// "Add host" (single use).
    Join {
        /// The enrolment token.
        #[arg(required_unless_present = "marketplace")]
        token: Option<String>,
        /// The kernel's URL, e.g. https://sun.example.com.
        #[arg(long, env = "SUN_URL", required_unless_present = "marketplace")]
        kernel: Option<String>,
        /// The name shown in the UI. Default: the hostname.
        #[arg(long)]
        name: Option<String>,
        /// Serve marketplace leases instead: poll Sun Cloud
        /// (`SUN_CLOUD_URL`, `SUN_CLOUD_TOKEN`) for tasks leased on this
        /// host's listings, join each buyer's kernel and run its sessions
        /// only in reset VMs with a network allowlist.
        #[arg(long, conflicts_with_all = ["token", "kernel"])]
        marketplace: bool,
        /// The `host_id` this host's listings name. Default: the id of
        /// <data dir>/host.key, which `sun market list` signs listings with.
        #[arg(long, requires = "marketplace")]
        host_id: Option<String>,
        /// Lima CLI for the task VMs (with `--marketplace`).
        #[arg(long, default_value = "limactl")]
        vm_cli: PathBuf,
    },
    /// Run the enrolled host (reconnects on its own).
    Run {
        /// Container/VM tools.
        #[command(flatten)]
        exec_env_tools: ExecEnvTools,
    },
}

#[derive(Args)]
struct Remote {
    /// The kernel's URL. Default: `http://<listen>` from sun.yaml.
    #[arg(long, global = true, env = "SUN_URL")]
    url: Option<String>,
    /// API token. Default: the local token in the data dir.
    #[arg(long, global = true, env = "SUN_TOKEN", hide_env_values = true)]
    token: Option<String>,
}

#[derive(Subcommand)]
enum RepoCommand {
    /// Add a repository to a workspace. Agents and workflows pick repos by
    /// name (the URL's last segment) or by the words of the description.
    Add {
        /// Clone URL: https://host/owner/name or git@host:owner/name.
        // `id`: `--url` is the kernel's, from `Remote`.
        #[arg(value_name = "URL", id = "clone_url")]
        url: String,
        /// What it is, e.g. "Go backend + Next.js frontend".
        #[arg(long, default_value = "")]
        description: String,
        /// Branch task branches start from; default: the host's, else main.
        #[arg(long)]
        branch: Option<String>,
        /// The workspace (id or name).
        #[arg(long, default_value = "main")]
        workspace: String,
        #[command(flatten)]
        remote: Remote,
    },
    /// List a workspace's repositories.
    List {
        /// The workspace (id or name).
        #[arg(long, default_value = "main")]
        workspace: String,
        #[command(flatten)]
        remote: Remote,
    },
    /// Remove a repository from a workspace.
    Remove {
        /// Repo id (`sun repo list`).
        id: String,
        /// The workspace (id or name).
        #[arg(long, default_value = "main")]
        workspace: String,
        #[command(flatten)]
        remote: Remote,
    },
}

#[derive(Subcommand)]
enum WorkspaceCommand {
    /// List workspaces.
    List {
        #[command(flatten)]
        remote: Remote,
    },
    /// Create a workspace; you become its owner.
    Create {
        /// Name, 1–64 characters, unique ignoring case.
        name: String,
        /// Its slug (default: from the name, made unique).
        #[arg(long)]
        slug: Option<String>,
        #[command(flatten)]
        remote: Remote,
    },
    /// Rename a workspace.
    Rename {
        /// Workspace id or name.
        workspace: String,
        /// The new name.
        name: String,
        #[command(flatten)]
        remote: Remote,
    },
    /// Delete a workspace that has no workflows (not Main).
    Delete {
        /// Workspace id or name.
        workspace: String,
        #[command(flatten)]
        remote: Remote,
    },
    /// Add an existing knowledge source to a workspace too (admin): the same index.
    ImportKnowledge {
        /// Workspace id or name.
        workspace: String,
        /// Source id (`sun knowledge list`).
        source: String,
        #[command(flatten)]
        remote: Remote,
    },
}

#[derive(Subcommand)]
enum TaskCommand {
    /// Create a task and queue it.
    Create {
        /// Title. Name the workspace repos to work in here or in the body
        /// (`sun repo list`); a workflow run picks them up from the text.
        title: String,
        /// Details for the agent.
        #[arg(long, default_value = "")]
        body: String,
        /// Workflow id or name. Default: the `default` workflow.
        #[arg(long, default_value = "default")]
        workflow: String,
        /// Run the agent in a container of this image (overrides the
        /// workflow's `exec_env:`; the host must report containers ready).
        #[arg(long, conflicts_with = "vm")]
        container: Option<String>,
        /// Run the agent in a Lima VM from this template, e.g. `default`.
        #[arg(long)]
        vm: Option<String>,
        /// Queue priority: urgent, high, medium, low or none.
        #[arg(long, default_value = "none")]
        priority: Priority,
        #[command(flatten)]
        remote: Remote,
    },
    /// List tasks.
    List {
        /// Only this workspace's tasks (id or name).
        #[arg(long)]
        workspace: Option<String>,
        #[command(flatten)]
        remote: Remote,
    },
    /// Show one task with its run, steps and linked pull requests.
    Show {
        /// Task id.
        id: String,
        #[command(flatten)]
        remote: Remote,
    },
    /// Data the task touched (detected) and how its merged PR held up.
    Quality {
        /// Task id.
        id: String,
        /// Re-check CI, reverts and fix commits now.
        #[arg(long)]
        check: bool,
        #[command(flatten)]
        remote: Remote,
    },
    /// Post-merge quality over every merged task.
    Metrics {
        #[command(flatten)]
        remote: Remote,
    },
    /// Move the task's running (or limit-hit) agent step to another runtime
    /// of its workflow's chain, with a hand-off pack, in the same worktree.
    Switch {
        /// Task id.
        id: String,
        /// A runtime of the workflow's `runtime.chain`.
        #[arg(long)]
        to: String,
        #[command(flatten)]
        remote: Remote,
    },
    /// What a runtime limit does for this task: show it, or set
    /// `switch`, `pause_and_notify`, `wait_until_reset`, or `default` (the
    /// workflow's, else the ecosystem's).
    OnLimit {
        /// Task id.
        id: String,
        /// The policy to set; omit it to show the current one.
        policy: Option<String>,
        #[command(flatten)]
        remote: Remote,
    },
    /// Pause, resume or cancel the task's run.
    #[command(flatten)]
    Control(TaskControl),
    /// Retry the task: a failed step from where it failed with `--step`,
    /// else a new run.
    Retry {
        /// Task id.
        id: String,
        /// The step to retry; succeeded steps are kept.
        #[arg(long)]
        step: Option<String>,
        #[command(flatten)]
        remote: Remote,
    },
    /// Send the running agent a message now, or with `--queue` when its
    /// step ends.
    Steer {
        /// Task id.
        id: String,
        /// The message.
        message: String,
        /// Deliver it with the next step instead of now.
        #[arg(long)]
        queue: bool,
        #[command(flatten)]
        remote: Remote,
    },
}

/// `sun task pause|resume|cancel <id>`: the same-named `POST /tasks/{id}/…`.
#[derive(Subcommand)]
enum TaskControl {
    /// Pause the running task: its agents stop, nothing new starts.
    Pause {
        /// Task id.
        id: String,
        #[command(flatten)]
        remote: Remote,
    },
    /// Resume a paused task.
    Resume {
        /// Task id.
        id: String,
        #[command(flatten)]
        remote: Remote,
    },
    /// Cancel the task's run.
    Cancel {
        /// Task id.
        id: String,
        #[command(flatten)]
        remote: Remote,
    },
}

#[derive(Subcommand)]
enum WorkflowCommand {
    /// Check a workflow file; errors carry line:column.
    Validate {
        /// The YAML file.
        file: PathBuf,
    },
    /// Save a workflow file: a new workflow, or the next version of `--id`.
    #[command(alias = "create")]
    Save {
        /// The YAML file.
        file: PathBuf,
        /// The workflow to add a version to.
        #[arg(long)]
        id: Option<String>,
        /// The workspace a new workflow goes on (id or name). Default: Main.
        #[arg(long, conflicts_with = "id")]
        workspace: Option<String>,
        #[command(flatten)]
        remote: Remote,
    },
    /// List workflows.
    List {
        /// Only this workspace's workflows (id or name).
        #[arg(long)]
        workspace: Option<String>,
        #[command(flatten)]
        remote: Remote,
    },
}

#[derive(Subcommand)]
enum ModCommand {
    /// Install from a folder holding mod.yaml, a package file, a git URL or
    /// a name from the mod registry. A running `sun serve` loads it within seconds.
    #[command(alias = "install")]
    Add(ModSource),
    /// Replace an installed one with a newer package; keeps its data.
    Update(ModSource),
    /// List installed ones.
    List,
    /// Uninstall; keeps its data and secrets unless --purge.
    Remove {
        /// Mod name.
        name: String,
        /// Also delete its data and secrets.
        #[arg(long)]
        purge: bool,
    },
}

#[derive(clap::Args)]
struct ModSource {
    /// Folder; .tar, .tar.gz, .zip or .sunpkg file; git+https://host/repo[#tag];
    /// or a name from the mod registry (e.g. github).
    source: String,
    /// Approve the requested permissions without asking.
    #[arg(long)]
    yes: bool,
    /// Hex ed25519 publisher keys a signed package must match.
    #[arg(long = "trust", env = "SUN_TRUSTED_KEYS", value_delimiter = ',')]
    trusted: Vec<String>,
}

#[derive(Subcommand)]
enum DevCommand {
    /// Run a mod from its folder, reloading it on every change.
    Dev {
        /// The mod's folder.
        dir: PathBuf,
    },
}

#[derive(Subcommand)]
enum MarketCommand {
    /// Sell this host's runtime capacity. The listing names this host by
    /// <data dir>/host.key (created on first use), whose signature proves
    /// it is yours; serve it with `sun host join --marketplace`.
    List {
        /// What is sold.
        #[arg(long, value_parser = ["machine", "open_model", "api", "subscription"])]
        offering: String,
        /// Runtime adapter, e.g. claude-code.
        #[arg(long)]
        runtime: String,
        /// The model it serves (always shown to buyers).
        #[arg(long)]
        model: String,
        /// What the price is per.
        #[arg(long, value_parser = ["hour", "task", "contract"])]
        unit: String,
        /// Points per unit (per contract for `contract`).
        #[arg(long)]
        price_points: i64,
        /// Most tasks at once.
        #[arg(long, default_value_t = 1)]
        concurrency: i64,
        /// Where it runs, e.g. id-jkt.
        #[arg(long)]
        region: String,
        /// Lima template each task's VM boots.
        #[arg(long, default_value = "default")]
        vm_template: String,
        /// Something buyers can filter on (gpu, ios_simulator, kvm); repeatable.
        #[arg(long = "capability")]
        capabilities: Vec<String>,
        /// A contract's term in hours (with --unit contract).
        #[arg(long)]
        contract_hours: Option<i64>,
    },
    /// This ecosystem's listings, active and paused.
    Listings,
    /// Stop taking new leases on a listing (open ones run on).
    Pause {
        /// Listing id.
        id: String,
    },
    /// Take new leases on a paused listing again.
    Resume {
        /// Listing id.
        id: String,
    },
    /// Earnings: without flags, what can be paid out and past payouts.
    Payout {
        /// Points to pay out.
        #[arg(long, requires_all = ["currency", "destination"])]
        points: Option<i64>,
        /// Paid in.
        #[arg(long, value_parser = ["usd", "idr"])]
        currency: Option<String>,
        /// Provider payee: a Stripe acct_..., or Xendit CHANNEL:account:holder.
        #[arg(long)]
        destination: Option<String>,
    },
    /// Buy (when paid) and install a marketplace mod in one step.
    Install {
        /// Mod name.
        name: String,
        /// Version. Default: the newest that isn't yanked.
        #[arg(long)]
        version: Option<String>,
        /// Approve its permissions (and price) without asking.
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Subcommand)]
enum LicenseCommand {
    /// Verify a license token and install it (`-` reads stdin).
    Install {
        /// The license file, or `-`.
        file: String,
    },
    /// The effective plan and the license's state.
    Show,
    /// Remove the license (back to Community).
    Remove,
}

#[derive(Subcommand)]
enum EcosystemCommand {
    /// Write the whole ecosystem (store, blobs, sealed secrets, sun.yaml,
    /// mods, skills, MCP servers, catalog installs) to a new file.
    Export {
        /// The archive to create; it must not exist.
        file: PathBuf,
        /// Also put the master key in: then the file alone opens every secret.
        #[arg(long, conflicts_with = "drop_secrets")]
        include_master_key: bool,
        /// Leave every secret value out; the import lists them to re-enter.
        #[arg(long)]
        drop_secrets: bool,
    },
    /// Restore an archive into an empty data dir.
    Import {
        /// The archive.
        file: PathBuf,
    },
}

/// Which secrets: a mod's, a workspace's, else the deployment's own names
/// (`Secret::is_deployment`: SMTP, SSO, the git host apps, embeddings) in
/// the ecosystem and every other name in Main.
#[derive(Args)]
struct SecretScopeArgs {
    /// The mod they belong to.
    #[arg(long = "mod", conflicts_with = "workspace")]
    mod_name: Option<String>,
    /// The workspace they belong to (id, slug or name). Default: Main, or
    /// the ecosystem for a deployment secret.
    #[arg(long)]
    workspace: Option<String>,
}

impl SecretScopeArgs {
    /// The scope of secret `name` (see the type); without a name (a
    /// list), both default scopes.
    async fn scopes(self, store: &Store, name: Option<&str>) -> Result<Vec<SecretScope>> {
        if let Some(mod_name) = self.mod_name {
            return Ok(vec![SecretScope::Mod(mod_name)]);
        }
        if let Some(workspace) = self.workspace {
            let id = workspace_in_store(store, &workspace).await?;
            return Ok(vec![SecretScope::Workspace(id)]);
        }
        let ecosystem = sun_engine::ecosystem_scope()?;
        let main = SecretScope::Workspace(sun_core::WorkspaceId::MAIN);
        Ok(match name {
            Some(name) if sun_core::model::Secret::is_deployment(name) => vec![ecosystem],
            Some(_) => vec![main],
            None => vec![ecosystem, main],
        })
    }

    /// [`Self::scopes`] of one secret.
    async fn scope(self, store: &Store, name: &str) -> Result<SecretScope> {
        let scopes = self.scopes(store, Some(name)).await?;
        scopes.into_iter().next().context("no secret scope")
    }
}

/// The workspace `text` names (its id, slug or name, ignoring case), read
/// from the store.
async fn workspace_in_store(store: &Store, text: &str) -> Result<sun_core::WorkspaceId> {
    use sun_store::WorkspaceRepo;
    let workspaces = store.read().await?.workspaces().await?;
    workspaces
        .into_iter()
        .find(|w| w.id.to_string() == text || w.slug == text || w.name.eq_ignore_ascii_case(text))
        .map(|w| w.id)
        .with_context(|| format!("no workspace {text}: see `sun workspace list`"))
}

#[derive(Subcommand)]
enum SecretCommand {
    /// Set a secret; the value is read from stdin or a no-echo prompt (never argv).
    Set {
        /// Name, e.g. GITHUB_TOKEN.
        name: String,
        #[command(flatten)]
        scope: SecretScopeArgs,
    },
    /// List secrets (names and times only).
    List {
        #[command(flatten)]
        scope: SecretScopeArgs,
    },
    /// Delete a secret.
    Rm {
        /// Name.
        name: String,
        #[command(flatten)]
        scope: SecretScopeArgs,
    },
}

#[derive(Subcommand)]
enum RuntimeCommand {
    /// Add a BYO key runtime to a workspace: your own provider key on
    /// OpenCode (any kind), Claude Code (anthropic) or Codex
    /// (openai_compatible). Set its key in the same workspace with
    /// `sun secret set <NAME> [--workspace W]`.
    Add {
        /// Name used in workflow `runtime.chain`, e.g. ollama-qwen.
        name: String,
        /// Your own API key / endpoint (the only source added here).
        #[arg(long)]
        byo_key: bool,
        /// Provider API: anthropic, google or openai_compatible.
        #[arg(long, default_value = "openai_compatible")]
        kind: String,
        /// The CLI that runs it: opencode, claude-code or codex.
        #[arg(long, default_value = "opencode")]
        adapter: String,
        /// Base URL, e.g. http://localhost:11434/v1; optional for anthropic
        /// and google (the provider's own).
        #[arg(long, default_value = "")]
        endpoint: String,
        /// Model id at that endpoint, e.g. qwen2.5-coder:7b.
        #[arg(long)]
        model: String,
        /// The workspace's secret holding the key; omit for a keyless endpoint.
        #[arg(long)]
        key_secret: Option<String>,
        /// The workspace (id, slug or name). Default: Main.
        #[arg(long)]
        workspace: Option<String>,
        #[command(flatten)]
        remote: Remote,
    },
    /// Show whether a workspace's runtime can take work, and what to fix.
    Check {
        /// The runtime's name.
        name: String,
        /// The workspace (id, slug or name). Default: Main.
        #[arg(long)]
        workspace: Option<String>,
        #[command(flatten)]
        remote: Remote,
    },
    /// The ecosystem's limit policy, used when neither a task nor
    /// its workflow sets one: show it, or set `switch`, `pause_and_notify`,
    /// `wait_until_reset` or `default` (`switch`). Needs `manage_runtimes`.
    OnLimit {
        /// The policy to set; omit it to show the current one.
        policy: Option<String>,
        #[command(flatten)]
        remote: Remote,
    },
}

fn main() {
    let cli = Cli::parse();
    if let Err(e) = run(cli) {
        eprintln!("sun: {e:#}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<()> {
    let data_dir = ops::data_dir(cli.data_dir)?;
    ops::refuse_postgres(&data_dir)?;
    match cli.command {
        Command::Serve {
            with_host,
            listen,
            port,
            no_mod_sandbox,
            exec_env_tools,
        } => runtime()?.block_on(serve(ServeOptions {
            listen: match port {
                Some(port) => Some(with_port(Config::load(&data_dir)?.listen(), port)),
                None => listen,
            },
            data_dir,
            with_host,
            sandbox_mods: !no_mod_sandbox,
            sun_exe: std::env::current_exe().context("locating the sun binary")?,
            exec_env_tools,
        })),
        Command::Status { json } => {
            let status = ops::status(&data_dir)?;
            match json {
                true => println!("{status}"),
                false => {
                    for (key, value) in status.as_object().into_iter().flatten() {
                        match key.as_str() {
                            "resources" if value.is_object() => {
                                ops::resource_lines(value)
                                    .iter()
                                    .for_each(|l| println!("{l}"));
                            }
                            _ => println!("{key:<12} {}", value.as_str().unwrap_or("-")),
                        }
                    }
                }
            }
            Ok(())
        }
        Command::License(command) => license(&data_dir, command),
        Command::Ecosystem(command) => runtime()?.block_on(ecosystem_archive(&data_dir, command)),
        Command::Host(command) => runtime()?.block_on(host(&data_dir, command)),
        Command::Repo(command) => repo(&data_dir, command),
        Command::Task(command) => task(&data_dir, command),
        Command::Workspace(command) => workspace(&data_dir, command),
        Command::Workflow(command) => workflow(&data_dir, command),
        Command::Knowledge {
            command,
            workspace,
            remote,
        } => sun_cli::knowledge::run(&client(&data_dir, remote)?, command, workspace.as_deref()),
        Command::Eval { command, remote } => {
            sun_cli::eval::run(&client(&data_dir, remote)?, command)
        }
        Command::User { command, remote } => {
            sun_cli::users::run_user(&client(&data_dir, remote)?, command)
        }
        Command::Smtp { command, remote } => {
            sun_cli::users::run_smtp(&client(&data_dir, remote)?, command)
        }
        Command::Plugin(PluginCommand::Mod(command)) => {
            runtime()?.block_on(mod_command(&data_dir, command, sun_mod::ModKind::Plugin))
        }
        Command::Plugin(PluginCommand::Mcp(command)) => {
            runtime()?.block_on(sun_cli::mcp::server_command(&data_dir, command))
        }
        Command::Extension(command) => {
            runtime()?.block_on(mod_command(&data_dir, command, sun_mod::ModKind::Extension))
        }
        Command::Mod(DevCommand::Dev { dir }) => runtime()?.block_on(mods::dev(&dir)),
        Command::Market(command) => runtime()?.block_on(market(&data_dir, command)),
        Command::Secret(command) => runtime()?.block_on(secret(&data_dir, command)),
        Command::Runtime(command) => runtime_command(&data_dir, command),
        Command::Doctor => ops::doctor(&data_dir),
        Command::Upgrade {
            check_local: true, ..
        } => match ops::check_local(&data_dir) {
            0 => Ok(()),
            n => bail!("{n} installed mod(s) won't load in this version"),
        },
        Command::Upgrade {
            check,
            from,
            version,
            wait,
            ..
        } => runtime()?.block_on(ops::upgrade(
            &data_dir,
            &bin_dir()?,
            &ops::Service::detect(),
            ops::UpgradeOptions {
                check,
                from,
                version,
                wait: std::time::Duration::from_secs(wait),
            },
        )),
        Command::Rollback => {
            let record = runtime()?.block_on(ops::rollback(&data_dir, &ops::Service::detect()))?;
            println!("sun: rolled back to {}", record.from_version);
            Ok(())
        }
        Command::Backup => {
            let path = runtime()?.block_on(ops::backup(&data_dir, "sun"))?;
            println!("sun: backup {}", path.display());
            Ok(())
        }
        Command::Migrate => runtime()?.block_on(async {
            Store::open(&data_dir).await?;
            Ok(())
        }),
        Command::Offers => {
            use sun_cli::installer::{InstallKind, LocalTools, local_offers};
            for offer in local_offers(InstallKind::detect(), LocalTools::detect()) {
                let state = match offer.found.is_empty() {
                    true => "not set up".to_owned(),
                    false => format!("ready ({})", offer.found.join(", ")),
                };
                println!("{:<10} {state}: {}", offer.name, offer.note);
            }
            Ok(())
        }
        Command::Skill(SkillCommand::New { name, dir }) => {
            let dir = sun_cli::scaffold::new_skill(&name, &dir)?;
            println!("created {}/SKILL.md", dir.display());
            println!(
                "Edit its description and steps. Add it to a repo's agent skills folder \
                 (e.g. .claude/skills/{name}), or propose it to the catalog by PR \
                 (catalog/skills/{name}/ with a LICENSE, plus an entry in catalog/index.json)."
            );
            Ok(())
        }
        Command::Mcp {
            sub: Some(McpCommand::New { name, dir }),
            ..
        } => {
            let (entry, _) = sun_cli::scaffold::new_mcp_server(&name, &dir)?;
            println!("created {}", entry.display());
            println!(
                "Add tools to TOOLS, then register it (Node on PATH):\n  \
                 sun plugin mcp add {name} --command {}",
                entry.display()
            );
            Ok(())
        }
        Command::Mcp {
            sub: Some(McpCommand::Proxy { grant }),
            ..
        } => runtime()?.block_on(sun_cli::mcp::proxy(&data_dir, &grant)),
        Command::Mcp { kernel, sub } => {
            let grant = match sub {
                Some(McpCommand::Relay { grant }) => Some(grant),
                _ => None,
            };
            let kernel = kernel.context("--kernel is required")?;
            let token = std::env::var("SUN_MCP_TOKEN").context("SUN_MCP_TOKEN is not set")?;
            runtime()?.block_on(sun_cli::mcp::shim(&kernel, &token, grant.as_deref()))
        }
    }
}

/// The directory of the running `sun` (where upgrades swap binaries).
fn bin_dir() -> Result<PathBuf> {
    let exe = std::env::current_exe()?.canonicalize()?;
    Ok(exe.parent().context("sun has no directory")?.to_path_buf())
}

fn runtime() -> Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_multi_thread()
        .worker_threads(std::thread::available_parallelism().map_or(4, |n| n.get().min(4)))
        .enable_all()
        .build()?)
}

async fn serve(options: ServeOptions) -> Result<()> {
    sun_cli::log::init();
    let config = Config::load(&options.data_dir)?;
    let kernel = serve::start(options, serve::adapters(&config)).await?;
    println!("sun: listening on http://{}", kernel.addr);
    match &kernel.first_admin {
        Some(email) => println!(
            "sun: sign in at {} as {email} / {} (then choose your own password; this token stops working)",
            kernel.url, kernel.token
        ),
        None => println!("sun: sign in at {}", kernel.url),
    }
    tokio::select! {
        r = kernel.done => r.context("kernel task")?,
        _ = tokio::signal::ctrl_c() => Ok(()),
    }
}

async fn host(data_dir: &Path, command: HostCommand) -> Result<()> {
    let dir = data_dir.join("host");
    match command {
        HostCommand::Join {
            marketplace: true,
            name,
            host_id,
            vm_cli,
            ..
        } => {
            let name = name.unwrap_or_else(hostname);
            let host_id = match host_id {
                Some(id) => id,
                None => sun_cli::market::HostKey::load_or_create(data_dir)?.id(),
            };
            market_seller(data_dir, &dir, name, &host_id, vm_cli).await
        }
        HostCommand::Join {
            token: Some(token),
            kernel: Some(kernel),
            name,
            ..
        } => {
            let url = host_url(&kernel)?;
            let name = name.unwrap_or_else(hostname);
            let credentials = sun_host::enroll(&url, &token, &name, &dir).await?;
            println!(
                "sun: enrolled as {name} ({}); start it with `sun host run`",
                credentials.host_id
            );
            Ok(())
        }
        HostCommand::Join { .. } => bail!("pass a token and --kernel, or --marketplace"),
        HostCommand::Run { exec_env_tools } => {
            let config = Config::load(data_dir)?;
            let credentials = sun_host::Credentials::load(&dir)?;
            let host_config = exec_env_tools.apply(sun_host::HostConfig::new(&dir));
            let host = sun_host::Host::open(host_config, serve::adapters(&config))?;
            println!(
                "sun: host {} dialing {}",
                credentials.host_id, credentials.kernel_url
            );
            tokio::select! {
                () = host.run(credentials) => Ok(()),
                _ = tokio::signal::ctrl_c() => Ok(()),
            }
        }
    }
}

/// `sun host join --marketplace`: every 30 s, joins the buyer kernels of new
/// dispatches for `host_id` (one marketplace host per lease, in
/// `host/market/<lease>`) and stops those whose lease closed.
async fn market_seller(
    data_dir: &Path,
    dir: &Path,
    name: String,
    host_id: &str,
    vm_cli: PathBuf,
) -> Result<()> {
    let cloud = sun_cli::cloud::CloudClient::from_env(|n| std::env::var(n).ok())
        .context("--marketplace needs SUN_CLOUD_URL and SUN_CLOUD_TOKEN")?;
    let config = Config::load(data_dir)?;
    let mut seller = sun_cli::market::Seller::new(cloud, host_id);
    let leases = dir.join("market");
    println!("sun: serving marketplace leases for host {host_id}");
    let mut poll = tokio::time::interval(std::time::Duration::from_secs(30));
    loop {
        tokio::select! {
            _ = poll.tick() => {}
            _ = tokio::signal::ctrl_c() => return Ok(()),
        }
        let join = |dispatch: sun_cli::market::Dispatch| {
            let lease_dir = leases.join(&dispatch.lease_id);
            let (name, vm_cli) = (name.clone(), vm_cli.clone());
            let adapters = serve::adapters(&config);
            async move {
                let url = host_url(&dispatch.kernel_url)
                    .map_err(|e| sun_core::Error::InvalidInput(e.to_string()))?;
                let credentials =
                    sun_host::enroll(&url, &dispatch.join_token, &name, &lease_dir).await?;
                let host = sun_host::Host::open(
                    sun_host::HostConfig {
                        marketplace: true,
                        vm_cli,
                        ..sun_host::HostConfig::new(&lease_dir)
                    },
                    adapters,
                )?;
                println!("sun: lease {} joined {}", dispatch.lease_id, url);
                Ok(tokio::spawn(host.run(credentials)))
            }
        };
        match seller.tick(join).await {
            Ok(stopped) => {
                for lease in stopped {
                    println!("sun: lease {lease} closed; its host stopped");
                    let _ = std::fs::remove_dir_all(leases.join(lease));
                }
            }
            Err(e) => eprintln!("sun: marketplace poll failed: {e} (retrying)"),
        }
    }
}

/// `sun market ...` against Sun Cloud, as this kernel's ecosystem.
async fn market(data_dir: &Path, command: MarketCommand) -> Result<()> {
    use sun_cli::market::{self, CloudMarket, HostKey};
    use sun_core::Marketplace;
    let client = || {
        sun_cli::cloud::CloudClient::from_env(|n| std::env::var(n).ok())
            .context("the marketplace needs SUN_CLOUD_URL and SUN_CLOUD_TOKEN")
    };
    let cloud = || anyhow::Ok(std::sync::Arc::new(client()?));
    let account = || anyhow::Ok(CloudMarket::new(client()?));
    let pretty = |value: &Value| serde_json::to_string_pretty(value).unwrap_or_default();
    let said = match command {
        MarketCommand::List {
            offering,
            runtime,
            model,
            unit,
            price_points,
            concurrency,
            region,
            vm_template,
            capabilities,
            contract_hours,
        } => {
            let key = HostKey::load_or_create(data_dir)?;
            let listing = json!({ "offering": offering, "runtime": runtime, "model": model,
                "unit": unit, "price_points": price_points, "concurrency": concurrency,
                "region": region, "vm_template": vm_template, "capabilities": capabilities,
                "contract_hours": contract_hours });
            let listed = market::create_listing(&cloud()?, &key, listing).await?;
            format!(
                "listed {} on host {}; serve it with `sun host join --marketplace`",
                listed["id"].as_str().unwrap_or("?"),
                key.id()
            )
        }
        MarketCommand::Listings => pretty(&account()?.my_listings().await?),
        MarketCommand::Pause { id } => {
            account()?.set_listing_status(&id, "paused").await?;
            format!("paused {id}")
        }
        MarketCommand::Resume { id } => {
            account()?.set_listing_status(&id, "active").await?;
            format!("resumed {id}")
        }
        MarketCommand::Payout {
            points: Some(points),
            currency: Some(currency),
            destination: Some(destination),
        } => {
            let payout = account()?
                .request_payout(points, &currency, &destination)
                .await?;
            let state = match payout["sent_at"].is_null() {
                true => "pending (retried until the provider takes it)",
                false => "sent",
            };
            format!("payout {} {state}", payout["id"].as_str().unwrap_or("?"))
        }
        MarketCommand::Payout { .. } => pretty(&account()?.payouts().await?),
        MarketCommand::Install { name, version, yes } => {
            let cloud = cloud()?;
            let listed = market::market_mod(&cloud, &name, version.as_deref()).await?;
            println!(
                "{} {} ({}) asks for:",
                listed.name, listed.version, listed.pricing
            );
            if listed.permissions.is_empty() {
                println!("  (nothing)");
            }
            for permission in &listed.permissions {
                println!("  {permission}");
            }
            let question = match listed.pricing.as_str() {
                "paid" => "Buy it (if not bought yet) and install with these permissions?",
                _ => "Install with these permissions?",
            };
            if !yes && !mods::confirm(question)? {
                bail!("not installed");
            }
            let record = market::install_mod(&cloud, &mods::mods_dir(data_dir), &listed).await?;
            let store = sun_store::Store::open(data_dir).await?;
            mods::record_install(&store, &record, "token:local").await?;
            format!(
                "installed {} {}; a running `sun serve` loads it within seconds",
                record.name, record.version
            )
        }
    };
    println!("{said}");
    Ok(())
}

/// `https://sun.example.com` → `wss://sun.example.com/host`.
fn host_url(kernel: &str) -> Result<String> {
    sun_cli::mcp::ws_url(kernel, "/host")
}

fn hostname() -> String {
    std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "host".into())
}

fn client(data_dir: &Path, remote: Remote) -> Result<Client> {
    let url = match remote.url {
        Some(url) => url,
        None => format!("http://{}", Config::load(data_dir)?.listen()),
    };
    let token = match remote.token {
        Some(token) => token,
        None => {
            let path = data_dir.join("kernel.json");
            if !path.exists() {
                bail!("no token: pass --token or run `sun serve` once to create one");
            }
            KernelSecrets::load_or_create(data_dir)?.token
        }
    };
    Ok(Client::new(&url, &token))
}

fn repo(data_dir: &Path, command: RepoCommand) -> Result<()> {
    let print = |repo: &Value| {
        println!(
            "{}  {}  {}  {}",
            repo["id"].as_str().unwrap_or("?"),
            repo["default_branch"].as_str().unwrap_or("?"),
            repo["url"].as_str().unwrap_or("?"),
            repo["description"].as_str().unwrap_or("")
        );
    };
    match command {
        RepoCommand::Add {
            url,
            description,
            branch,
            workspace,
            remote,
        } => {
            let client = client(data_dir, remote)?;
            let workspace = client.workspace_id(&workspace)?;
            let added = client.post(
                &format!("/workspaces/{workspace}/repos"),
                &json!({ "url": url, "description": description, "default_branch": branch }),
            )?;
            print(&added["repo"]);
            if let Some(hook) = added["manual_hook"].as_object() {
                println!(
                    "Sun couldn't add the GitLab webhook; add it to the project yourself: \
                     {}  secret {}",
                    hook["url"].as_str().unwrap_or("?"),
                    hook["secret"].as_str().unwrap_or("?")
                );
            }
        }
        RepoCommand::List { workspace, remote } => {
            let client = client(data_dir, remote)?;
            let workspace = client.workspace_id(&workspace)?;
            let repos = client.get(&format!("/workspaces/{workspace}/repos"))?;
            for repo in repos.as_array().into_iter().flatten() {
                print(repo);
            }
        }
        RepoCommand::Remove {
            id,
            workspace,
            remote,
        } => {
            let client = client(data_dir, remote)?;
            let workspace = client.workspace_id(&workspace)?;
            client.delete(&format!("/workspaces/{workspace}/repos/{id}"))?;
            println!("removed repo {id}");
        }
    }
    Ok(())
}

fn task(data_dir: &Path, command: TaskCommand) -> Result<()> {
    match command {
        TaskCommand::Create {
            title,
            body,
            workflow,
            container,
            vm,
            priority,
            remote,
        } => {
            let exec_env = match (container, vm) {
                (Some(image), _) => json!({ "kind": "container", "image": image }),
                (_, Some(template)) => json!({ "kind": "vm", "template": template }),
                _ => Value::Null,
            };
            let client = client(data_dir, remote)?;
            let workflows = client.get("/workflows")?;
            let workflow_id = workflows
                .as_array()
                .into_iter()
                .flatten()
                .find(|w| w["id"] == workflow.as_str() || w["name"] == workflow.as_str())
                .and_then(|w| w["id"].as_str())
                .with_context(|| format!("no workflow {workflow}"))?
                .to_owned();
            let task = client.post(
                "/tasks",
                &json!({ "title": title, "body": body, "workflow_id": workflow_id,
                         "exec_env": exec_env, "priority": priority }),
            )?;
            println!(
                "{}  {}  {}",
                task["id"].as_str().unwrap_or("?"),
                task["status"].as_str().unwrap_or("?"),
                title
            );
            Ok(())
        }
        TaskCommand::List { workspace, remote } => {
            let client = client(data_dir, remote)?;
            let path = match workspace {
                Some(workspace) => format!("/tasks?workspace={}", client.workspace_id(&workspace)?),
                None => "/tasks".to_owned(),
            };
            for task in client.get(&path)?.as_array().into_iter().flatten() {
                println!(
                    "{}  {:<16}  {}",
                    task["id"].as_str().unwrap_or("?"),
                    task["status"].as_str().unwrap_or("?"),
                    task["title"].as_str().unwrap_or("")
                );
            }
            Ok(())
        }
        TaskCommand::Show { id, remote } => {
            let client = client(data_dir, remote)?;
            print_task(&client.get(&format!("/tasks/{id}"))?);
            print_pull_requests(&client.get(&format!("/tasks/{id}/pull_requests"))?);
            Ok(())
        }
        TaskCommand::Quality { id, check, remote } => {
            let client = client(data_dir, remote)?;
            if check {
                client.post(&format!("/tasks/{id}/quality/check"), &json!({}))?;
            }
            print_quality(&client.get(&format!("/tasks/{id}/quality"))?);
            Ok(())
        }
        TaskCommand::Switch { id, to, remote } => {
            let client = client(data_dir, remote)?;
            let task = client.get(&format!("/tasks/{id}"))?;
            let (run_id, step) = switchable_step(&task).with_context(|| {
                format!("task {id} has no running or limit-hit agent step to switch")
            })?;
            let path = format!("/runs/{run_id}/steps/{step}/switch");
            client.post(&path, &json!({ "to": to }))?;
            println!("{id}  {step}  switching to {to} with a hand-off pack");
            Ok(())
        }
        TaskCommand::OnLimit { id, policy, remote } => on_limit(
            &client(data_dir, remote)?,
            &format!("/tasks/{id}/on_limit"),
            policy,
        ),
        TaskCommand::Control(control) => {
            let (verb, id, remote) = match control {
                TaskControl::Pause { id, remote } => ("pause", id, remote),
                TaskControl::Resume { id, remote } => ("resume", id, remote),
                TaskControl::Cancel { id, remote } => ("cancel", id, remote),
            };
            let task =
                client(data_dir, remote)?.post(&format!("/tasks/{id}/{verb}"), &json!({}))?;
            print_task(&task);
            Ok(())
        }
        TaskCommand::Retry { id, step, remote } => {
            let client = client(data_dir, remote)?;
            let run = run_of(&client, &id)?;
            let path = match step {
                Some(step) => format!("/runs/{run}/steps/{step}/retry"),
                None => format!("/runs/{run}/retry"),
            };
            print_task(&client.post(&path, &json!({}))?);
            Ok(())
        }
        TaskCommand::Steer {
            id,
            message,
            queue,
            remote,
        } => {
            let client = client(data_dir, remote)?;
            let run = run_of(&client, &id)?;
            let body = json!({ "body": message, "queue": queue });
            let event = client.post(&format!("/runs/{run}/messages"), &body)?;
            println!(
                "{}  {}",
                event["kind"].as_str().unwrap_or("?"),
                event["payload"]["summary"].as_str().unwrap_or("")
            );
            Ok(())
        }
        TaskCommand::Metrics { remote } => {
            let m = client(data_dir, remote)?.get("/quality/metrics")?;
            let rate = match m["held_up_rate"].as_f64() {
                Some(rate) => format!("{:.0}%", rate * 100.0),
                None => "-".into(),
            };
            println!("merged      {}", m["merged"]);
            println!(
                "held up     {} of {} decided ({rate})",
                m["held_up"], m["decided"]
            );
            println!("ci failed   {}", m["ci_failed"]);
            println!("reverted    {}", m["reverted"]);
            println!("with fixes  {}", m["with_fixes"]);
            println!("with bugs   {}", m["with_bugs"]);
            Ok(())
        }
    }
}

/// The id of the task's latest run.
fn run_of(client: &Client, task_id: &str) -> Result<String> {
    let task = client.get(&format!("/tasks/{task_id}"))?;
    task["run"]["id"]
        .as_str()
        .map(str::to_owned)
        .with_context(|| format!("task {task_id} has not started yet"))
}

fn print_quality(view: &Value) {
    let s = |v: &Value| v.as_str().unwrap_or("-").to_owned();
    match view["data_touched"].as_array().filter(|f| !f.is_empty()) {
        Some(files) => {
            println!("data touched (detected)");
            for file in files {
                println!("  {:<12} {}", s(&file["rule"]), s(&file["path"]));
            }
        }
        None => println!("data touched  none detected"),
    }
    let q = &view["quality"];
    if q.is_null() {
        println!("merged        not yet");
        return;
    }
    let held = match view["held_up"].as_bool() {
        Some(true) => "yes",
        Some(false) => "no",
        None => "pending",
    };
    println!(
        "merged        {} into {} ({})",
        s(&q["merge_sha"]),
        s(&q["base"]),
        s(&q["merged_at"])
    );
    println!("held up       {held}");
    println!("ci            {}", s(&q["ci_state"]));
    println!("reverted by   {}", s(&q["reverted_by"]));
    for fix in q["fix_commits"].as_array().into_iter().flatten() {
        println!("fix           {}  {}", s(&fix["sha"]), s(&fix["subject"]));
    }
    for bug in q["bug_issues"].as_array().into_iter().flatten() {
        println!(
            "{:<13} {}  {}",
            s(&bug["relation"]),
            s(&bug["key"]),
            s(&bug["title"])
        );
    }
}

/// `(run id, step key)` of a `TaskDetail`'s agent step that can switch
/// runtime: `running`, or `limit_hit` (a run paused on a limit).
fn switchable_step(task: &Value) -> Option<(&str, &str)> {
    let run = &task["run"];
    let step = run["steps"].as_array()?.iter().find(|s| {
        s["type"] == "agent" && matches!(s["status"].as_str(), Some("running" | "limit_hit"))
    })?;
    Some((run["id"].as_str()?, step["step_key"].as_str()?))
}

fn print_task(task: &Value) {
    let s = |v: &Value| v.as_str().unwrap_or("-").to_owned();
    println!("{}  {}", s(&task["id"]), s(&task["title"]));
    println!("status    {}", s(&task["status"]));
    println!(
        "workflow  {} v{}",
        s(&task["workflow"]["name"]),
        task["workflow"]["version"]
    );
    let run = &task["run"];
    if run.is_null() {
        return;
    }
    println!("run       {}  {}", s(&run["id"]), s(&run["status"]));
    for step in run["steps"].as_array().into_iter().flatten() {
        println!(
            "  {:<12} {:<9} {}",
            s(&step["step_key"]),
            s(&step["type"]),
            s(&step["status"])
        );
    }
}

/// `GET /tasks/{id}/pull_requests`: one line per linked PR, then what a
/// merge does to the task.
fn print_pull_requests(linked: &Value) {
    let s = |v: &Value| v.as_str().unwrap_or("-").to_owned();
    for (i, pr) in linked["pull_requests"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
    {
        let label = if i == 0 { "prs" } else { "" };
        println!(
            "{label:<9} {}/{}#{}  {:<7} {}  {}  (linked by {})",
            s(&pr["repo_owner"]),
            s(&pr["repo_name"]),
            pr["number"],
            s(&pr["state"]),
            s(&pr["title"]),
            s(&pr["html_url"]),
            s(&pr["link_source"])
        );
    }
    let auto = &linked["auto_complete"];
    let on_merge = match auto["state"].as_str() {
        Some("none") => return,
        Some("waiting") => format!(
            "moves to {} when every PR merges",
            s(&auto["target_status"])
        ),
        Some("all_merged") => "every PR merged".to_owned(),
        Some("not_merged") => "a PR closed without merging".to_owned(),
        Some("at_target") => "already at the target status".to_owned(),
        Some("workspace_disabled" | "task_disabled") => "PR merges don't move this task".to_owned(),
        _ => s(&auto["state"]),
    };
    println!("on merge  {on_merge}");
}

fn workflow(data_dir: &Path, command: WorkflowCommand) -> Result<()> {
    let read = |file: &Path| {
        std::fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))
    };
    match command {
        WorkflowCommand::Validate { file } => match sun_workflow::parse(&read(&file)?) {
            Ok(workflow) => {
                println!(
                    "{}: ok ({}, {} steps)",
                    file.display(),
                    workflow.name,
                    workflow.steps.len()
                );
                Ok(())
            }
            Err(errors) => {
                for error in &errors {
                    eprintln!("{}:{error}", file.display());
                }
                bail!("{} error(s)", errors.len());
            }
        },
        WorkflowCommand::Save {
            file,
            id,
            workspace,
            remote,
        } => {
            let yaml = read(&file)?;
            let client = client(data_dir, remote)?;
            let mut body = json!({ "source_yaml": yaml });
            if let Some(workspace) = workspace {
                body["workspace_id"] = json!(client.workspace_id(&workspace)?);
            }
            let saved = match id {
                Some(id) => client.post(&format!("/workflows/{id}/versions"), &body)?,
                None => client.post("/workflows", &body)?,
            };
            println!(
                "saved {} v{} ({})",
                saved["name"].as_str().unwrap_or("?"),
                saved["version"],
                saved["id"].as_str().unwrap_or("?")
            );
            Ok(())
        }
        WorkflowCommand::List { workspace, remote } => {
            let client = client(data_dir, remote)?;
            let path = match workspace {
                Some(workspace) => {
                    format!("/workflows?workspace={}", client.workspace_id(&workspace)?)
                }
                None => "/workflows".to_owned(),
            };
            for workflow in client.get(&path)?.as_array().into_iter().flatten() {
                println!(
                    "{}  v{:<3}  {}",
                    workflow["id"].as_str().unwrap_or("?"),
                    workflow["version"],
                    workflow["name"].as_str().unwrap_or("")
                );
            }
            Ok(())
        }
    }
}

fn workspace(data_dir: &Path, command: WorkspaceCommand) -> Result<()> {
    let print = |workspace: &Value| {
        println!(
            "{}  {}",
            workspace["id"].as_str().unwrap_or("?"),
            workspace["name"].as_str().unwrap_or("")
        );
    };
    match command {
        WorkspaceCommand::List { remote } => {
            for workspace in client(data_dir, remote)?
                .get("/workspaces")?
                .as_array()
                .into_iter()
                .flatten()
            {
                print(workspace);
            }
        }
        WorkspaceCommand::Create { name, slug, remote } => {
            let body = json!({ "name": name, "slug": slug });
            print(&client(data_dir, remote)?.post("/workspaces", &body)?);
        }
        WorkspaceCommand::Rename {
            workspace,
            name,
            remote,
        } => {
            let client = client(data_dir, remote)?;
            let id = client.workspace_id(&workspace)?;
            print(&client.patch(&format!("/workspaces/{id}"), &json!({ "name": name }))?);
        }
        WorkspaceCommand::Delete { workspace, remote } => {
            let client = client(data_dir, remote)?;
            let id = client.workspace_id(&workspace)?;
            client.delete(&format!("/workspaces/{id}"))?;
            println!("deleted {workspace}");
        }
        WorkspaceCommand::ImportKnowledge {
            workspace,
            source,
            remote,
        } => {
            let client = client(data_dir, remote)?;
            let id = client.workspace_id(&workspace)?;
            client.post(
                &format!("/workspaces/{id}/knowledge_sources/{source}"),
                &json!({}),
            )?;
            println!("imported knowledge source {source} into {workspace}");
        }
    }
    Ok(())
}

async fn add_mod(
    data_dir: &Path,
    source: ModSource,
    kind: sun_mod::ModKind,
    update: bool,
) -> Result<()> {
    let keys = source
        .trusted
        .iter()
        .map(|k| mods::parse_key(k))
        .collect::<Result<Vec<_>>>()?;
    let record = mods::install(data_dir, &source.source, kind, &keys, source.yes, update).await?;
    let done = if update { "updated" } else { "installed" };
    println!(
        "{done} {} {}; a running `sun serve` loads it within seconds",
        record.name, record.version
    );
    Ok(())
}

async fn mod_command(data_dir: &Path, command: ModCommand, kind: sun_mod::ModKind) -> Result<()> {
    match command {
        ModCommand::Add(source) => add_mod(data_dir, source, kind, false).await,
        ModCommand::Update(source) => add_mod(data_dir, source, kind, true).await,
        ModCommand::List => {
            let kind = match kind {
                sun_mod::ModKind::Plugin => sun_core::model::ModKind::Plugin,
                sun_mod::ModKind::Extension => sun_core::model::ModKind::Extension,
            };
            for install in mods::list(data_dir, kind).await? {
                let signer = install
                    .signed_by
                    .as_deref()
                    .map_or("unsigned", |_| "signed");
                println!(
                    "{:<16} {:<10} {signer}  {}",
                    install.name,
                    install.version,
                    install.granted_permissions.join(" ")
                );
            }
            Ok(())
        }
        ModCommand::Remove { name, purge } => {
            for extension in mods::remove(data_dir, &name, purge).await? {
                eprintln!(
                    "warning: extension {extension} needs plugin {name}; its calls to {name} fail until you reinstall it"
                );
            }
            println!("removed {name}");
            Ok(())
        }
    }
}

fn runtime_command(data_dir: &Path, command: RuntimeCommand) -> Result<()> {
    match command {
        RuntimeCommand::Add {
            name,
            byo_key,
            kind,
            adapter,
            endpoint,
            model,
            key_secret,
            workspace,
            remote,
        } => {
            if !byo_key {
                bail!(
                    "pass --byo-key: BYO runtimes appear on their own when a host reports the CLI"
                );
            }
            let body = json!({ "name": name, "adapter": adapter, "source": "byo_key",
                "config": { "kind": kind, "endpoint": endpoint, "model": model,
                    "key_secret": key_secret } });
            let client = client(data_dir, remote)?;
            let workspace = workspace_or_main(&client, workspace.as_deref())?;
            client.post(&format!("/runtimes?workspace={workspace}"), &body)?;
            println!("added {name}; check it with `sun runtime check {name}`");
            Ok(())
        }
        RuntimeCommand::Check {
            name,
            workspace,
            remote,
        } => {
            let client = client(data_dir, remote)?;
            let workspace = workspace_or_main(&client, workspace.as_deref())?;
            let path = format!("/runtimes/{name}/readiness?workspace={workspace}");
            let ready = client.get(&path)?;
            let ok = ready["installed"] == true && ready["logged_in"] == true;
            println!("{name}: {}", if ok { "ready" } else { "not ready" });
            if let Some(detail) = ready["detail"].as_str() {
                println!("{detail}");
            }
            if !ok {
                std::process::exit(1);
            }
            Ok(())
        }
        RuntimeCommand::OnLimit { policy, remote } => {
            on_limit(&client(data_dir, remote)?, "/settings/on_limit", policy)
        }
    }
}

/// The id of `workspace` (id, slug or name), Main when none is named.
fn workspace_or_main(client: &Client, workspace: Option<&str>) -> Result<String> {
    match workspace {
        Some(workspace) => client.workspace_id(workspace),
        None => Ok(sun_core::WorkspaceId::MAIN.to_string()),
    }
}

/// Shows or sets a limit policy at `path` (`sun task|runtime on-limit`).
/// `default` clears it; the kernel validates the rest.
fn on_limit(client: &Client, path: &str, policy: Option<String>) -> Result<()> {
    let reply = match policy.as_deref() {
        None => client.get(path)?,
        Some("default") => client.put(path, &json!({ "on_limit": null }))?,
        Some(policy) => client.put(path, &json!({ "on_limit": policy }))?,
    };
    match reply["on_limit"].as_str() {
        Some(policy) => println!("on_limit: {policy}"),
        None => println!("on_limit: default"),
    }
    Ok(())
}

/// `host:port` with `port` in place of `listen`'s.
fn with_port(listen: &str, port: u16) -> String {
    let host = listen
        .rsplit_once(':')
        .map_or("127.0.0.1", |(host, _)| host);
    format!("{host}:{port}")
}

fn license(data_dir: &Path, command: LicenseCommand) -> Result<()> {
    use sun_cli::license::{describe, install, licensing};
    match command {
        LicenseCommand::Install { file } => {
            let token = match file.as_str() {
                "-" => {
                    let mut token = String::new();
                    std::io::stdin().read_to_string(&mut token)?;
                    token
                }
                path => std::fs::read_to_string(path).with_context(|| format!("reading {path}"))?,
            };
            std::fs::create_dir_all(data_dir)?;
            println!("installed: {}", describe(&install(data_dir, &token)?));
        }
        LicenseCommand::Show => println!("{}", describe(&licensing(data_dir)?.resolve())),
        LicenseCommand::Remove => {
            let path = licensing(data_dir)?.path;
            if path.exists() {
                std::fs::remove_file(&path)
                    .with_context(|| format!("removing {}", path.display()))?;
            }
            println!("removed: back on community");
        }
    }
    Ok(())
}

async fn ecosystem_archive(data_dir: &Path, command: EcosystemCommand) -> Result<()> {
    match command {
        EcosystemCommand::Export {
            file,
            include_master_key,
            drop_secrets,
        } => {
            let secrets = match (include_master_key, drop_secrets) {
                (true, _) => {
                    eprintln!(
                        "sun: warning: this file alone opens every secret; guard it like master.key"
                    );
                    sun_store::ArchiveSecrets::WithKey
                }
                (false, true) => sun_store::ArchiveSecrets::Dropped,
                (false, false) => sun_store::ArchiveSecrets::Sealed,
            };
            Store::open(data_dir).await?.export(&file, secrets).await?;
            println!("exported {}", file.display());
        }
        EcosystemCommand::Import { file } => match Store::import(&file, data_dir).await {
            Ok(store) => {
                println!("imported {} into {}", file.display(), data_dir.display());
                let pending = store.read().await?.secrets_to_reenter().await?;
                if !pending.is_empty() {
                    println!("{} secrets to re-enter:", pending.len());
                }
                for secret in pending {
                    let flag = match &secret.scope {
                        sun_core::model::SecretScope::Mod(name) => format!(" --mod {name}"),
                        sun_core::model::SecretScope::Ecosystem(_) => String::new(),
                        // A git connection's token: connect again in the UI.
                        sun_core::model::SecretScope::Workspace(id)
                            if sun_core::model::Secret::is_connection(&secret.name) =>
                        {
                            println!("  reconnect Git in workspace {id} (Settings → Code)");
                            continue;
                        }
                        sun_core::model::SecretScope::Workspace(id) => format!(" --workspace {id}"),
                    };
                    println!("  sun secret set{flag} {}", secret.name);
                }
            }
            Err(sun_store::StoreError::Core(sun_core::Error::InvalidInput(why))) => {
                eprintln!("sun: {why}");
                std::process::exit(2);
            }
            Err(sun_store::StoreError::Migrate(e)) => {
                bail!("{e}: the archive is from a newer sun; upgrade sun first")
            }
            Err(e) => return Err(e.into()),
        },
    }
    Ok(())
}

async fn secret(data_dir: &Path, command: SecretCommand) -> Result<()> {
    let store = Store::open(data_dir).await?;
    let actor = "token:local";
    match command {
        SecretCommand::Set { name, scope } => {
            let scope = scope.scope(&store, &name).await?;
            let value = read_secret(&name)?;
            sun_engine::set_secret(&store, &scope, &name, &value, actor).await?;
            println!("set {name} ({}:{})", scope.kind(), scope.id());
        }
        SecretCommand::List { scope } => {
            let scopes = scope.scopes(&store, None).await?;
            let mut tx = store.read().await?;
            let to_reenter = tx.secrets_to_reenter().await?;
            for scope in &scopes {
                let at = format!("{}:{}", scope.kind(), scope.id());
                for secret in tx.list_secrets(Some(scope)).await? {
                    println!("{:<24} {} {at}", secret.name, secret.updated_at);
                }
                for secret in to_reenter.iter().filter(|s| &s.scope == scope) {
                    println!("{:<24} not set: re-enter it after import", secret.name);
                }
            }
        }
        SecretCommand::Rm { name, scope } => {
            let scope = scope.scope(&store, &name).await?;
            if !sun_engine::delete_secret(&store, &scope, &name, actor).await? {
                bail!("no secret {name}");
            }
            println!("deleted {name}");
        }
    }
    Ok(())
}

/// The value from stdin: a no-echo prompt on a terminal, else the whole input.
fn read_secret(name: &str) -> Result<String> {
    let stdin = std::io::stdin();
    if !stdin.is_terminal() {
        let mut value = String::new();
        stdin.lock().read_to_string(&mut value)?;
        return Ok(value.trim_end_matches(['\r', '\n']).to_owned());
    }
    eprint!("{name}: ");
    let stty = |arg: &str| {
        std::process::Command::new("stty")
            .arg(arg)
            .stdin(std::process::Stdio::inherit())
            .status()
    };
    let _ = stty("-echo");
    let mut value = String::new();
    let read = stdin.read_line(&mut value);
    let _ = stty("echo");
    eprintln!();
    read?;
    Ok(value.trim_end_matches(['\r', '\n']).to_owned())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    #[test]
    fn switch_picks_the_running_or_limit_hit_agent_step() {
        let task = |status: &str| {
            json!({ "run": { "id": "r1", "steps": [
                { "step_key": "plan", "type": "agent", "status": "succeeded" },
                { "step_key": "check", "type": "gate", "status": "running" },
                { "step_key": "build", "type": "agent", "status": status },
            ] } })
        };
        assert_eq!(
            super::switchable_step(&task("running")),
            Some(("r1", "build"))
        );
        assert_eq!(
            super::switchable_step(&task("limit_hit")),
            Some(("r1", "build"))
        );
        assert_eq!(super::switchable_step(&task("queued")), None);
        assert_eq!(super::switchable_step(&json!({ "run": null })), None);
    }
}
