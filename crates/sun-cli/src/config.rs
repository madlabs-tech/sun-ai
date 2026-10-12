//! The data dir and what lives in it: `sun.yaml` (operator settings, optional)
//! and `kernel.json` (the generated local token and host secret, mode 0600).

use std::collections::BTreeMap;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// `<data_dir>/sun.yaml`. Every field is optional; unknown fields are refused
/// so a typo doesn't silently fall back to a default.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Where `sun serve` listens. Default `127.0.0.1:7700`.
    pub listen: Option<String>,
    /// The URL people and hosts reach the kernel at, e.g.
    /// `https://sun.example.com`. Default `http://<listen>`.
    pub public_url: Option<String>,
    /// `RuntimeAdapter::id` for workflows without `runtime:`. Default `claude-code`.
    pub default_runtime: Option<String>,
    /// Per-runtime settings of the local host's adapters.
    #[serde(default)]
    pub runtimes: Runtimes,
    /// Deprecated: imported once into Settings → Sign-in (the client
    /// secret from `SUN_OIDC_CLIENT_SECRET`), then ignored.
    pub oidc: Option<Oidc>,
    /// Non-secret install-time settings per mod, by mod name.
    #[serde(default)]
    pub mods: BTreeMap<String, ModSettings>,
    /// Active runs at once in the ecosystem; unset = no cap.
    pub max_active_runs: Option<usize>,
    /// Caps per runtime and per host; extra tasks queue.
    #[serde(default)]
    pub caps: Caps,
    /// Hours a done or cancelled task's worktrees are kept before they are
    /// removed. Default 0: at once. Failed tasks keep theirs for a retry.
    pub worktree_retention_hours: Option<u64>,
    /// On start, fire each `cron:` trigger missed while Sun was down once
    /// (its latest missed minute, up to 7 days back). Default true.
    pub cron_catch_up: Option<bool>,
    /// Minutes between syncs of connector knowledge sources. Default 15.
    pub knowledge_sync_minutes: Option<u64>,
    /// Where Sun says a step needs a person (an approval, a stalled agent),
    /// with a `public_url` deep link, e.g. `[slack:#eng]`. Default: nowhere.
    #[serde(default)]
    pub notify_to: Vec<String>,
    /// Sun Cloud's URL for a self-hosted license account, e.g.
    /// `https://sun.example.com`. `SUN_REGISTRY_URL` overrides it.
    pub registry_url: Option<String>,
    /// Stall detection for every workflow without its own `watchdog:`:
    /// `{stall_after: 15m, on_stall: nudge | restart | fail}`.
    #[serde(default)]
    pub watchdog: Watchdog,
    /// Context fill (percent of the window) at which an agent session is
    /// rotated to a fresh one. Default 80; 50 to 95.
    pub rotate_at_percent: Option<u8>,
    /// The decision engine `decide` steps ask (Laya or Jev). The API
    /// key comes from `SUN_DECISIONS_API_KEY` (never in the file).
    pub decisions: Option<Decisions>,
    /// Built-in features to turn off, e.g. `{knowledge: false, market: false}`:
    /// their routes answer 404, their workers don't start and the
    /// UI hides them. Default: all on.
    #[serde(default)]
    pub features: sun_api::Features,
    /// Days a finished run's full timeline is kept; older ones are compacted
    /// into one blob per run (usage stays). Unset = keep forever.
    pub event_retention_days: Option<u32>,
}

/// `caps:` in sun.yaml, e.g.
/// `{per_runtime: {claude-code: 2}, per_host: {vps: {runs: 4, memory_mb: 8192, cpus: 4}, "*": {runs: 2}}}`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Caps {
    /// Active runs per runtime (a workflow chain entry).
    #[serde(default)]
    pub per_runtime: BTreeMap<String, usize>,
    /// Per host name (`*` = every other host): active runs, and the RAM and
    /// CPUs their VMs may take together.
    #[serde(default)]
    pub per_host: BTreeMap<String, sun_engine::HostCap>,
}

/// `watchdog:` in `sun.yaml`; unset fields keep the engine defaults.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Watchdog {
    /// A duration like `15m` or `1h30m`.
    pub stall_after: Option<String>,
    /// What to do with a stalled step.
    pub on_stall: Option<sun_workflow::StallPolicy>,
}

impl Watchdog {
    /// `stall_after` in seconds; `None` when unset.
    pub fn stall_after_secs(&self) -> Result<Option<u64>> {
        self.stall_after
            .as_deref()
            .map(|d| {
                sun_workflow::parse_duration(d).with_context(|| {
                    format!(
                        "sun.yaml: watchdog.stall_after `{d}`: use a duration like 15m or 1h30m"
                    )
                })
            })
            .transpose()
    }
}

/// Where the decision engine (`/v1/systemone`) lives.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Decisions {
    /// Base URL, e.g. `http://127.0.0.1:8090` (a Laya sidecar) or Jev's.
    pub url: Option<String>,
    /// Instead of `url`: a mod that answers `systemone` (the Jev plugin
    /// path, `sun_engine::PLUGIN_METHOD`), e.g. `jev`.
    pub plugin: Option<String>,
    /// The sidecar `sun serve` starts and stops, e.g.
    /// `[laya, serve, --port, "8090"]`; `url` points at it.
    #[serde(default)]
    pub sidecar: Vec<String>,
}

/// The agent CLIs on this box and their permission policy. Claude Code takes the policy as flags; OpenCode's permission
/// requests are answered by the same policy.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Runtimes {
    /// One of `sun_core::PERMISSION_MODES`. `bypass_permissions` only
    /// with `allow_bypass`.
    #[serde(default = "accept_edits")]
    pub permission_mode: String,
    /// The operator's explicit opt-in to `bypass_permissions` (e.g. sessions
    /// run in a container or VM execution environment).
    #[serde(default)]
    pub allow_bypass: bool,
    /// Tools allowed without a prompt (`--allowedTools`). Default: the
    /// commands the default gates run, so the agent can run them first.
    #[serde(default = "gate_tools")]
    pub allowed_tools: Vec<String>,
    /// Scripted test runtimes (`sun-fake-runtime`) by runtime id, e.g.
    /// `fake-limit` and `fake-ok` for a failover chain.
    #[serde(default)]
    pub fake: BTreeMap<String, FakeSettings>,
    /// Runtimes paid with Sun credits (Sun Cloud) by name, e.g.
    /// `claude-credits: {adapter: claude-code}`; name them in a workflow chain.
    #[serde(default)]
    pub credits: BTreeMap<String, CreditsSettings>,
    /// Editable envelope templates: `<dir>/<adapter id>.md`
    /// (`claude-code.md`, `codex.md`, `opencode.md`) replaces that CLI's
    /// built-in one. Relative to the data dir. Unset = built-ins only.
    pub template_dir: Option<PathBuf>,
}

impl Default for Runtimes {
    fn default() -> Self {
        Self {
            permission_mode: accept_edits(),
            allow_bypass: false,
            allowed_tools: gate_tools(),
            fake: BTreeMap::new(),
            credits: BTreeMap::new(),
            template_dir: None,
        }
    }
}

/// A `credits` runtime: which agent CLI runs, and for OpenCode the model
/// from the gateway's price table.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreditsSettings {
    /// `claude-code`, `codex` or `opencode`.
    pub adapter: String,
    /// Gateway model id; required for `opencode`.
    pub model: Option<String>,
}

fn accept_edits() -> String {
    "accept_edits".into()
}

/// The default `test`/`lint` gate commands (sun-engine `gates`), as Claude
/// Code tool rules.
fn gate_tools() -> Vec<String> {
    [
        "cargo test",
        "cargo clippy",
        "cargo check",
        "npm test",
        "npm run lint",
        "go test",
        "go vet",
        "go build",
        "git status",
        "git diff",
    ]
    .iter()
    .map(|command| format!("Bash({command}:*)"))
    .collect()
}

/// `sun-fake-runtime` with a fixed script (tests, demos).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FakeSettings {
    /// The `sun-fake-runtime` binary.
    pub binary: PathBuf,
    /// The JSONL script every session replays.
    pub script: PathBuf,
}

/// The old single OIDC provider; see `sun_core::sso::SsoProvider`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Oidc {
    /// Shown on the login button.
    pub name: String,
    /// Issuer URL.
    pub issuer_url: String,
    /// Client id at the provider.
    pub client_id: String,
    /// Email domains allowed in; empty = anyone the provider signs in.
    #[serde(default)]
    pub allowed_domains: Vec<String>,
}

/// What a mod gets beyond its manifest .
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModSettings {
    /// Passed as `init.config` (e.g. `{api_url: https://jira.acme.com}`).
    #[serde(default)]
    pub config: Option<serde_json::Value>,
    /// Extra hosts the sandbox proxy may tunnel to (self-hosted Jira, Gitea).
    #[serde(default)]
    pub network: Vec<String>,
}

/// Generated once per data dir, `kernel.json`, mode 0600.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KernelSecrets {
    /// The kernel token: the CLI's `Authorization: Bearer`, and the
    /// first-run owner's password.
    pub token: String,
    /// What host keys are derived from.
    pub host_secret: String,
}

/// The data dir: `--data-dir`, `$SUN_DATA_DIR`, else `~/.sun`.
pub fn data_dir(flag: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(dir) = flag {
        return Ok(dir);
    }
    let home = std::env::var_os("HOME").context("HOME is not set; pass --data-dir")?;
    Ok(Path::new(&home).join(".sun"))
}

impl Config {
    /// Reads `<data_dir>/sun.yaml`; a missing file is all defaults.
    pub fn load(data_dir: &Path) -> Result<Self> {
        let path = data_dir.join("sun.yaml");
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        let mut config: Self = serde_saphyr::from_str(&text)
            .map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
        if let Some(dir) = &mut config.runtimes.template_dir {
            *dir = data_dir.join(&*dir);
        }
        config.check()?;
        Ok(config)
    }

    fn check(&self) -> Result<()> {
        if self.event_retention_days == Some(0) {
            bail!("sun.yaml: event_retention_days must be at least 1");
        }
        if let Some(p) = self.rotate_at_percent
            && !(50..=95).contains(&p)
        {
            bail!("sun.yaml: rotate_at_percent must be 50 to 95, got {p}");
        }
        self.watchdog.stall_after_secs()?;
        let runtimes = &self.runtimes;
        if !sun_core::PERMISSION_MODES.contains(&runtimes.permission_mode.as_str()) {
            bail!(
                "sun.yaml: runtimes.permission_mode `{}` is not a permission mode; use {}",
                runtimes.permission_mode,
                sun_core::PERMISSION_MODES.join(", ")
            );
        }
        if runtimes.permission_mode == "bypass_permissions" && !runtimes.allow_bypass {
            bail!(
                "sun.yaml: runtimes.permission_mode bypass_permissions lets the agent \
                 run anything; set allow_bypass: true only when sessions run in a container or VM"
            );
        }
        for (name, credits) in &runtimes.credits {
            sun_runtime::credits::session_env(
                &credits.adapter,
                "https://gateway.invalid",
                "",
                credits.model.as_deref(),
            )
            .map_err(|e| anyhow::anyhow!("sun.yaml: runtimes.credits.{name}: {e}"))?;
        }
        // A typo in a user template fails the boot, not the first session.
        if let Some(dir) = &runtimes.template_dir {
            for id in [
                sun_runtime::claude::ID,
                sun_runtime::codex::ID,
                sun_runtime::opencode::ID,
            ] {
                sun_runtime::envelope::load(id, Some(dir))
                    .and_then(|t| sun_runtime::envelope::render(&t, &Default::default()))
                    .map_err(|e| {
                        anyhow::anyhow!("sun.yaml: runtimes.template_dir: {id}.md: {e}")
                    })?;
            }
        }
        for to in &self.notify_to {
            if !matches!(to.split_once(':'), Some((channel, address)) if !channel.is_empty() && !address.is_empty())
            {
                bail!("sun.yaml: notify_to `{to}`: use channel:address, e.g. slack:#eng");
            }
        }
        Ok(())
    }

    /// The listen address.
    pub fn listen(&self) -> &str {
        self.listen.as_deref().unwrap_or("127.0.0.1:7700")
    }
}

impl KernelSecrets {
    /// Loads `kernel.json`, creating it with fresh random values on first boot.
    pub fn load_or_create(data_dir: &Path) -> Result<Self> {
        let path = data_dir.join("kernel.json");
        if let Ok(text) = std::fs::read_to_string(&path) {
            return serde_json::from_str(&text).with_context(|| format!("{}", path.display()));
        }
        let secrets = Self {
            token: random_hex()?,
            host_secret: random_hex()?,
        };
        write_private(&path, &serde_json::to_vec_pretty(&secrets)?)?;
        Ok(secrets)
    }

    /// Replaces the token in `kernel.json` with a fresh one (the
    /// first-run token was printed) and returns it; the host secret stays.
    pub fn rotate_token(data_dir: &Path) -> Result<String> {
        let mut secrets = Self::load_or_create(data_dir)?;
        secrets.token = random_hex()?;
        let path = data_dir.join("kernel.json");
        write_private(&path, &serde_json::to_vec_pretty(&secrets)?)?;
        Ok(secrets.token)
    }
}

/// Writes `bytes` to a new or truncated file only its owner can read.
pub fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .and_then(|mut f| f.write_all(bytes))
        .with_context(|| format!("writing {}", path.display()))
}

/// 256 random bits as hex.
pub fn random_hex() -> Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| anyhow::anyhow!("no randomness: {e}"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn config_parses_and_refuses_unsafe_bypass() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(Config::load(dir.path()).unwrap().listen(), "127.0.0.1:7700");
        std::fs::write(
            dir.path().join("sun.yaml"),
            "listen: 0.0.0.0:80\nmods: {jira: {config: {api_url: https://j.acme.com}, network: [j.acme.com]}}\n",
        )
        .unwrap();
        let config = Config::load(dir.path()).unwrap();
        assert_eq!(config.mods["jira"].network, ["j.acme.com"]);
        std::fs::write(
            dir.path().join("sun.yaml"),
            "runtimes: {permission_mode: bypass_permissions}\n",
        )
        .unwrap();
        assert!(Config::load(dir.path()).is_err());
        std::fs::write(
            dir.path().join("sun.yaml"),
            "runtimes: {permission_mode: acceptEdits}\n",
        )
        .unwrap();
        assert!(
            Config::load(dir.path()).is_err(),
            "Claude's spelling is not Sun's"
        );
        std::fs::write(
            dir.path().join("sun.yaml"),
            "rotate_at_percent: 70\ndecisions: {url: https://laya.test}\n",
        )
        .unwrap();
        let config = Config::load(dir.path()).unwrap();
        assert_eq!(config.rotate_at_percent, Some(70));
        assert_eq!(
            config.decisions.unwrap().url.as_deref(),
            Some("https://laya.test")
        );
        std::fs::write(
            dir.path().join("sun.yaml"),
            "caps: {per_runtime: {claude-code: 2}, per_host: {vps: {runs: 3, memory_mb: 8192}}}\nworktree_retention_hours: 24\n",
        )
        .unwrap();
        let config = Config::load(dir.path()).unwrap();
        assert_eq!(config.caps.per_runtime["claude-code"], 2);
        assert_eq!(config.caps.per_host["vps"].runs, Some(3));
        assert_eq!(config.caps.per_host["vps"].memory_mb, Some(8192));
        assert_eq!(config.worktree_retention_hours, Some(24));
        std::fs::write(
            dir.path().join("sun.yaml"),
            "caps: {per_host: {vps: {ram: 1}}}\n",
        )
        .unwrap();
        assert!(
            Config::load(dir.path()).is_err(),
            "typos in caps are refused"
        );
        std::fs::write(dir.path().join("sun.yaml"), "rotate_at_percent: 99\n").unwrap();
        assert!(
            Config::load(dir.path()).is_err(),
            "rotating at 99% is too late"
        );
        std::fs::write(
            dir.path().join("sun.yaml"),
            "features: {knowledge: false}\nevent_retention_days: 30\n",
        )
        .unwrap();
        let config = Config::load(dir.path()).unwrap();
        assert!(!config.features.knowledge && config.features.evals);
        std::fs::write(dir.path().join("sun.yaml"), "features: {knowlege: false}\n").unwrap();
        assert!(
            Config::load(dir.path()).is_err(),
            "feature typos are refused"
        );
        std::fs::write(dir.path().join("sun.yaml"), "lisen: x\n").unwrap();
        assert!(Config::load(dir.path()).is_err(), "typos are refused");
        let credits = |yaml: &str| {
            std::fs::write(dir.path().join("sun.yaml"), yaml).unwrap();
            Config::load(dir.path())
        };
        let ok = credits("runtimes: {credits: {cc: {adapter: claude-code}, oc: {adapter: opencode, model: m1}}}\n").unwrap();
        assert_eq!(ok.runtimes.credits["oc"].model.as_deref(), Some("m1"));
        assert!(
            credits("runtimes: {credits: {oc: {adapter: opencode}}}\n").is_err(),
            "opencode needs a model"
        );
        // Envelope templates: relative to the data dir; a typo fails the boot.
        std::fs::create_dir_all(dir.path().join("tpl")).unwrap();
        std::fs::write(dir.path().join("tpl/codex.md"), "Do {{task}}").unwrap();
        let ok = credits("runtimes: {template_dir: tpl}\n").unwrap();
        assert_eq!(ok.runtimes.template_dir, Some(dir.path().join("tpl")));
        std::fs::write(dir.path().join("tpl/codex.md"), "{{task}} {{gate}}").unwrap();
        let err = credits("runtimes: {template_dir: tpl}\n").unwrap_err();
        assert!(err.to_string().contains("codex.md"), "{err}");
        assert!(credits("runtimes: {credits: {x: {adapter: aider}}}\n").is_err());
        let watched = credits("watchdog: {stall_after: 20m, on_stall: restart}\n").unwrap();
        assert_eq!(watched.watchdog.stall_after_secs().unwrap(), Some(1200));
        assert_eq!(
            watched.watchdog.on_stall,
            Some(sun_workflow::StallPolicy::Restart)
        );
        assert!(credits("watchdog: {stall_after: soon}\n").is_err());
    }

    #[test]
    fn kernel_secrets_are_private_and_stable() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let first = KernelSecrets::load_or_create(dir.path()).unwrap();
        let again = KernelSecrets::load_or_create(dir.path()).unwrap();
        assert_eq!(first.token, again.token);
        let rotated = KernelSecrets::rotate_token(dir.path()).unwrap();
        let after = KernelSecrets::load_or_create(dir.path()).unwrap();
        assert_ne!(rotated, first.token);
        assert_eq!(
            (after.token, after.host_secret),
            (rotated, first.host_secret.clone())
        );
        let mode = std::fs::metadata(dir.path().join("kernel.json"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
