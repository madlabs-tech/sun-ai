//! # sun-catalog — skills and MCP servers to pick from
//!
//! **Responsibility:** the curated catalog of skills and MCP servers
//! (`catalog/index.json` plus each skill's folder under
//! `catalog/skills/<id>/`, embedded in the binary), stack-based suggestions
//! from a repo's files, and the ecosystem store. Workflow templates
//! are not indexed here: they are `sun_workflow::TEMPLATES` (`GET /templates`).
//!
//! **Contract:**
//! - **Nothing is installed unless picked**: only [`Catalog::install`] writes,
//!   and suggestions are a ranking, never an action.
//! - A skill install copies its folder into `<data>/skills/<id>/`, the store
//!   every runtime host syncs into each session's native skills folder
//!   (`sun_runtime::skills::sync`). An MCP install validates the server with
//!   `sun_mod::McpServer::validate` and writes the registration to
//!   `<data>/mcp_servers/<id>.json` (no secret values, only names).
//! - A skill may be limited to some runtimes (`<data>/skills/.runtimes.json`,
//!   [`sun_core::SKILL_RUNTIMES_FILE`]); unlisted skills reach every runtime.
//! - Installs are pinned: `<data>/catalog/installed.json` holds id → version.
//!   A newer catalog version is offered (listing shows both), never applied.
//! - Installs are per workspace, files shared: the files on disk are the
//!   deployment's, and which workspaces have an entry is the store's
//!   (`workspace_catalog_install`), passed in as `installs` where it matters
//!   ([`Catalog::list`], [`Catalog::mcp_servers_in`]).
// ponytail: one copy of each entry on disk, so a workspace's update (or
// `runtimes` limit) is every workspace's; per-workspace folders if they diverge.
//! - `<data>/mcp_servers/` is the one MCP server registry: catalog installs
//!   and custom servers ([`Catalog::add_mcp_server`]) live side by side. A
//!   custom server names its workspace (`McpServer::workspace_id`, Main when
//!   missing); names are unique in the deployment (one file each).
//! - Dismissed suggestions are kept per ecosystem in
//!   `<data>/catalog/dismissed.json` ([`Catalog::dismiss`]).
//!
//! **May depend on:** `sun-core`, `sun-mod` (for the MCP registration).

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rust_embed::RustEmbed;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sun_core::{Error, Result, SKILL_RUNTIMES_FILE, WorkspaceId};
pub use sun_mod::{McpClient, McpServer, McpTool, McpTransport, serve_mcp_proxy};

#[derive(RustEmbed)]
#[folder = "../../catalog/"]
struct CatalogFiles;

/// What an entry installs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// A `SKILL.md` folder, copied into each runtime's skills folder.
    Skill,
    /// An MCP server registration.
    Mcp,
}

/// One curated catalog entry, as written in `catalog/index.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    /// Unique id; also the skill folder or MCP server name.
    pub id: String,
    /// What it is.
    pub kind: Kind,
    /// What it does, one sentence.
    pub description: String,
    /// Where it comes from, e.g. `github.com/mattpocock/skills`.
    pub source: String,
    /// Author.
    pub author: String,
    /// SPDX license id.
    pub license: String,
    /// Pinned version.
    pub version: String,
    /// Stack tags it fits (`frontend`, `rust`, `mobile`, ...); empty = any repo.
    #[serde(default)]
    pub stacks: Vec<String>,
    /// MCP: the tools it offers, for display.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<String>,
    /// MCP: the registration an install creates.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<McpServer>,
    /// Installs reported by kernels that opted into telemetry, written
    /// into the curated list by its maintainers; the last ranking key.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub installs: u64,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

/// An entry with its install state and why it fits the repo.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Listing {
    /// The catalog entry.
    #[serde(flatten)]
    pub entry: Entry,
    /// On the curated list. Always true today: the catalog is the curated list.
    pub reviewed: bool,
    /// Installed version, if installed (may be older than `entry.version`).
    pub installed: Option<String>,
    /// The repo's stacks this entry fits; ranks suggestions.
    pub matches: Vec<String>,
    /// A saved workflow names it (an agent step's `skills:` or an `mcp:`
    /// grant); ranks suggestions first.
    pub in_workflows: bool,
    /// MCP: network hosts it reaches.
    pub hosts: Vec<String>,
    /// MCP: secret names it needs (values live in the secret store).
    pub secrets: Vec<String>,
    /// Skill: the runtime adapter ids that get it; `None` = every runtime.
    pub runtimes: Option<Vec<String>>,
}

/// A skill's files as (path inside the skill, bytes).
pub type SkillFiles = Vec<(String, Vec<u8>)>;

/// One file of an entry, shown in full before install.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EntryFile {
    /// Path inside the entry, e.g. `SKILL.md`.
    pub path: String,
    /// UTF-8 text (the catalog's; empty for a `removed` file).
    pub text: String,
    /// Against the installed copy (diff on update): `added`,
    /// `changed`, `unchanged` or `removed`; `None` when not installed.
    pub change: Option<&'static str>,
    /// A `changed` file's line diff, installed → catalog: every line
    /// prefixed with `' '`, `'-'` or `'+'`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diff: Option<Vec<String>>,
}

/// Most suggestions an ecosystem can dismiss.
pub const MAX_DISMISSED: usize = 1000;

/// The catalog plus the ecosystem store under a data dir.
#[derive(Debug)]
pub struct Catalog {
    data_dir: PathBuf,
    entries: Vec<Entry>,
    /// Serializes install/uninstall (read-modify-write of `installed.json`).
    lock: Mutex<()>,
}

#[derive(Deserialize)]
struct Index {
    entries: Vec<Entry>,
}

impl Catalog {
    /// Loads and checks the embedded index.
    pub fn open(data_dir: impl Into<PathBuf>) -> Result<Self> {
        let raw = CatalogFiles::get("index.json")
            .ok_or_else(|| Error::NotFound("catalog/index.json".into()))?;
        let index: Index = serde_json::from_slice(&raw.data)
            .map_err(|e| Error::InvalidInput(format!("catalog/index.json: {e}")))?;
        let mut entries = Vec::new();
        let mut seen = BTreeSet::new();
        for entry in index.entries {
            if !seen.insert(entry.id.clone()) {
                return Err(bad(&entry.id, "duplicate id"));
            }
            check(&entry)?;
            entries.push(entry);
        }
        Ok(Self {
            data_dir: data_dir.into(),
            entries,
            lock: Mutex::new(()),
        })
    }

    /// Every entry, best fit first: named by the workflows in use
    /// (`in_use`, skill and MCP server names), then the most `stacks`
    /// matched, then the most installs elsewhere, then catalog order. With
    /// `installs` (a workspace's), only those show as installed; `None`
    /// shows what is on disk.
    pub fn list(
        &self,
        stacks: &[String],
        in_use: &[String],
        installs: Option<&[String]>,
    ) -> Result<Vec<Listing>> {
        let mut installed = self.installed()?;
        if let Some(installs) = installs {
            installed.retain(|id, _| installs.contains(id));
        }
        let runtimes = self.skill_runtimes()?;
        let mut out: Vec<Listing> = self
            .entries
            .iter()
            .map(|entry| {
                let (hosts, secrets) = match &entry.server {
                    Some(server) => (hosts(server), server.secret_names()),
                    None => (Vec::new(), Vec::new()),
                };
                Listing {
                    reviewed: true,
                    installed: installed.get(&entry.id).cloned(),
                    matches: entry
                        .stacks
                        .iter()
                        .filter(|s| stacks.contains(s))
                        .cloned()
                        .collect(),
                    in_workflows: in_use.contains(&entry.id),
                    hosts,
                    secrets,
                    runtimes: runtimes.get(&entry.id).cloned(),
                    entry: entry.clone(),
                }
            })
            .collect();
        out.sort_by_key(|l| std::cmp::Reverse((l.in_workflows, l.matches.len(), l.entry.installs)));
        Ok(out)
    }

    /// The entry's full text: a skill's files or the MCP registration as JSON.
    /// When the entry is installed, each file says how it differs from the
    /// installed copy, and files only the installed copy has come last as
    /// `removed`.
    pub fn files(&self, id: &str) -> Result<Vec<EntryFile>> {
        let entry = self.entry(id)?;
        let installed = self.installed()?.contains_key(id);
        // (path in the entry, catalog bytes, where the installed copy lives)
        let files: Vec<(String, Vec<u8>, PathBuf)> = match entry.kind {
            Kind::Skill => {
                let dir = self.skills_dir().join(id);
                skill_files(id)
                    .into_iter()
                    .map(|(path, data)| {
                        let at = dir.join(&path);
                        (path, data, at)
                    })
                    .collect()
            }
            Kind::Mcp => {
                let json = serde_json::to_vec_pretty(&entry.server)
                    .map_err(|e| Error::InvalidInput(e.to_string()))?;
                vec![(format!("{id}.json"), json, self.mcp_file(id))]
            }
        };
        let mut out: Vec<EntryFile> = files
            .iter()
            .map(|(path, data, at)| {
                let text = String::from_utf8_lossy(data).into_owned();
                let (change, diff) = match installed.then(|| fs::read(at)) {
                    None => (None, None),
                    Some(Ok(old)) if old == *data => (Some("unchanged"), None),
                    Some(Ok(old)) => (
                        Some("changed"),
                        Some(line_diff(&String::from_utf8_lossy(&old), &text)),
                    ),
                    Some(Err(_)) => (Some("added"), None),
                };
                EntryFile {
                    path: path.clone(),
                    text,
                    change,
                    diff,
                }
            })
            .collect();
        if installed && entry.kind == Kind::Skill {
            let mut old = Vec::new();
            list_files(&self.skills_dir().join(id), Path::new(""), &mut old);
            old.retain(|p| !files.iter().any(|(path, ..)| path == p));
            old.sort();
            out.extend(old.into_iter().map(|path| EntryFile {
                path,
                text: String::new(),
                change: Some("removed"),
                diff: None,
            }));
        }
        Ok(out)
    }

    /// Installs a skill or MCP server into the ecosystem store (also an
    /// update to the catalog's version). A skill goes to the `runtimes`
    /// (adapter ids, e.g. `claude-code`) or, with `None`, to every runtime.
    pub fn install(&self, id: &str, runtimes: Option<&[String]>) -> Result<()> {
        let entry = self.entry(id)?;
        if let Some(runtimes) = runtimes {
            if entry.kind != Kind::Skill {
                return Err(bad(id, "only skills are installed per runtime"));
            }
            check_runtimes(runtimes)?;
        }
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        match (entry.kind, &entry.server) {
            (Kind::Skill, _) => {
                let dir = self.data_dir.join("skills").join(id);
                let tmp = self.data_dir.join("skills").join(format!(".{id}.tmp"));
                remove_dir(&tmp)?;
                for (path, data) in skill_files(id) {
                    write(&tmp.join(path), &data)?;
                }
                remove_dir(&dir)?;
                fs::rename(&tmp, &dir).map_err(io(&dir))?;
                self.save_skill_runtimes(id, runtimes)?;
            }
            (Kind::Mcp, Some(server)) => {
                server.validate()?;
                let json = serde_json::to_vec_pretty(server)
                    .map_err(|e| Error::InvalidInput(e.to_string()))?;
                write(&self.mcp_file(id), &json)?;
            }
            (Kind::Mcp, None) => return Err(bad(id, "an mcp entry needs a server")),
        }
        let mut installed = self.installed()?;
        installed.insert(id.to_owned(), entry.version.clone());
        self.save_installed(&installed)
    }

    /// A skill's catalog version and files, for a repo-scope install (a PR
    /// that commits them, `sun_engine`); `runtimes` is checked like
    /// [`Self::install`]'s. Writes nothing.
    pub fn skill_bundle(
        &self,
        id: &str,
        runtimes: Option<&[String]>,
    ) -> Result<(String, SkillFiles)> {
        let entry = self.entry(id)?;
        if entry.kind != Kind::Skill {
            return Err(bad(id, "only skills are installed into a repo"));
        }
        if let Some(runtimes) = runtimes {
            check_runtimes(runtimes)?;
        }
        Ok((entry.version.clone(), skill_files(id)))
    }

    /// Removes an installed skill or MCP server. Hosts drop the skill from
    /// each runtime folder at the next session.
    pub fn uninstall(&self, id: &str) -> Result<()> {
        let entry = self.entry(id)?;
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut installed = self.installed()?;
        if installed.remove(id).is_none() {
            return Err(Error::NotFound(format!("`{id}` is not installed")));
        }
        match entry.kind {
            Kind::Skill => {
                remove_dir(&self.data_dir.join("skills").join(id))?;
                self.save_skill_runtimes(id, None)?;
            }
            Kind::Mcp => match fs::remove_file(self.mcp_file(id)) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                    return Err(io(&self.mcp_file(id))(e));
                }
                _ => {}
            },
        }
        self.save_installed(&installed)
    }

    /// The suggestions this ecosystem dismissed (catalog or template ids).
    pub fn dismissed(&self) -> Result<BTreeSet<String>> {
        read_json(&self.dismissed_file())
    }

    /// Dismisses (`true`) or restores (`false`) the suggestion `id`. Ids are
    /// slug-shaped but need not be catalog entries: workflow templates are
    /// suggested next to them.
    pub fn dismiss(&self, id: &str, dismissed: bool) -> Result<()> {
        if !is_slug(id) {
            return Err(bad(id, "id must match ^[a-z0-9]+(-[a-z0-9]+)*$"));
        }
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut set = self.dismissed()?;
        let changed = match dismissed {
            true => set.insert(id.to_owned()),
            false => set.remove(id),
        };
        if set.len() > MAX_DISMISSED {
            return Err(Error::InvalidInput(format!(
                "at most {MAX_DISMISSED} dismissed suggestions; restore some first"
            )));
        }
        match changed {
            true => write_json(&self.dismissed_file(), &set),
            false => Ok(()),
        }
    }

    fn dismissed_file(&self) -> PathBuf {
        self.data_dir.join("catalog").join("dismissed.json")
    }

    /// The skill store hosts sync from: `<data>/skills`.
    pub fn skills_dir(&self) -> PathBuf {
        self.data_dir.join("skills")
    }

    /// One entry by id.
    pub fn entry(&self, id: &str) -> Result<&Entry> {
        self.entries
            .iter()
            .find(|e| e.id == id)
            .ok_or_else(|| Error::NotFound(format!("catalog entry `{id}`")))
    }

    /// Every registered MCP server (catalog installs and custom ones), by name.
    pub fn mcp_servers(&self) -> Result<Vec<McpServer>> {
        let dir = self.data_dir.join("mcp_servers");
        let files = match fs::read_dir(&dir) {
            Ok(files) => files,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(io(&dir)(e)),
        };
        let mut servers = Vec::new();
        for file in files {
            let path = file.map_err(io(&dir))?.path();
            if path.extension().is_none_or(|ext| ext != "json") {
                continue;
            }
            let raw = fs::read(&path).map_err(io(&path))?;
            let server: McpServer = serde_json::from_slice(&raw)
                .map_err(|e| Error::InvalidInput(format!("{}: {e}", path.display())))?;
            servers.push(server);
        }
        servers.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(servers)
    }

    /// The registered MCP servers `workspace` has: the catalog installs among
    /// `installs` (the workspace's) and its custom servers.
    pub fn mcp_servers_in(
        &self,
        workspace: WorkspaceId,
        installs: &[String],
    ) -> Result<Vec<McpServer>> {
        let installed = self.installed()?;
        let mut servers = self.mcp_servers()?;
        servers.retain(|s| match installed.contains_key(&s.name) {
            true => installs.contains(&s.name),
            false => s.workspace_id.unwrap_or(WorkspaceId::MAIN) == workspace,
        });
        Ok(servers)
    }

    /// True when the MCP server `name` is a catalog install (its file is
    /// shared by the workspaces that installed it), not a custom server.
    pub fn is_catalog_install(&self, name: &str) -> Result<bool> {
        Ok(self.installed()?.contains_key(name))
    }

    /// One registered MCP server by name.
    pub fn mcp_server(&self, name: &str) -> Result<McpServer> {
        self.mcp_servers()?
            .into_iter()
            .find(|s| s.name == name)
            .ok_or_else(|| Error::NotFound(format!("mcp server `{name}` is not registered")))
    }

    /// Saves a custom MCP server without connecting; the name must be free.
    /// [`Catalog::register_mcp_server`] tests it first.
    pub fn add_mcp_server(&self, server: &McpServer) -> Result<()> {
        server.validate()?;
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let path = self.mcp_file(&server.name);
        if path.exists() {
            return Err(taken(&server.name));
        }
        let json =
            serde_json::to_vec_pretty(server).map_err(|e| Error::InvalidInput(e.to_string()))?;
        write(&path, &json)
    }

    /// Registers a custom MCP server after connecting once to list its tools;
    /// `secrets` are the values of [`McpServer::secret_names`].
    pub async fn register_mcp_server(
        &self,
        server: &McpServer,
        secrets: Vec<(String, String)>,
    ) -> Result<Vec<McpTool>> {
        server.validate()?;
        if self.mcp_server(&server.name).is_ok() {
            return Err(taken(&server.name));
        }
        let tools = self.mcp_tools(server, secrets).await?;
        self.add_mcp_server(server)?;
        Ok(tools)
    }

    /// Connects to `server` and lists its tools.
    pub async fn mcp_tools(
        &self,
        server: &McpServer,
        secrets: Vec<(String, String)>,
    ) -> Result<Vec<McpTool>> {
        self.mcp_connect(server, secrets).await?.tools().await
    }

    /// Unregisters an MCP server; a catalog install is uninstalled.
    pub fn remove_mcp_server(&self, name: &str) -> Result<()> {
        let server = self.mcp_server(name)?;
        if self.installed()?.contains_key(&server.name) {
            return self.uninstall(&server.name);
        }
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let path = self.mcp_file(&server.name);
        fs::remove_file(&path).map_err(io(&path))
    }

    /// Connects to `server` (data dir `<data>/mcp/<name>`); `secrets` are the
    /// values of [`McpServer::secret_names`].
    pub async fn mcp_connect(
        &self,
        server: &McpServer,
        secrets: Vec<(String, String)>,
    ) -> Result<McpClient> {
        let dir = self.data_dir.join("mcp").join(&server.name);
        McpClient::connect(server, &dir, secrets).await
    }

    fn mcp_file(&self, id: &str) -> PathBuf {
        self.data_dir.join("mcp_servers").join(format!("{id}.json"))
    }

    fn installed_file(&self) -> PathBuf {
        self.data_dir.join("catalog").join("installed.json")
    }

    fn installed(&self) -> Result<BTreeMap<String, String>> {
        read_json(&self.installed_file())
    }

    fn save_installed(&self, installed: &BTreeMap<String, String>) -> Result<()> {
        write_json(&self.installed_file(), installed)
    }

    fn skill_runtimes(&self) -> Result<BTreeMap<String, Vec<String>>> {
        read_json(&self.skills_dir().join(SKILL_RUNTIMES_FILE))
    }

    /// Limits skill `id` to `runtimes`, or (`None`) lets every runtime have it.
    fn save_skill_runtimes(&self, id: &str, runtimes: Option<&[String]>) -> Result<()> {
        let mut map = self.skill_runtimes()?;
        match runtimes {
            Some(runtimes) => map.insert(id.to_owned(), runtimes.to_vec()),
            None => map.remove(id),
        };
        write_json(&self.skills_dir().join(SKILL_RUNTIMES_FILE), &map)
    }
}

/// Runtime adapter ids for a skill: 1–16, slug-shaped, each once.
fn check_runtimes(runtimes: &[String]) -> Result<()> {
    let slug = |r: &str| {
        r.len() <= 64
            && r.starts_with(|c: char| c.is_ascii_lowercase())
            && r.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
    };
    let unique: BTreeSet<&String> = runtimes.iter().collect();
    match runtimes {
        [] => Err(Error::InvalidInput(
            "runtimes: pick at least one runtime".into(),
        )),
        _ if runtimes.len() > 16 => Err(Error::InvalidInput("runtimes: at most 16".into())),
        _ if unique.len() != runtimes.len() => {
            Err(Error::InvalidInput("runtimes: each runtime once".into()))
        }
        _ => match runtimes.iter().find(|r| !slug(r)) {
            Some(r) => Err(Error::InvalidInput(format!(
                "runtimes: `{r}` is not a runtime id"
            ))),
            None => Ok(()),
        },
    }
}

/// A JSON file's value; the default when it doesn't exist.
fn read_json<T: DeserializeOwned + Default>(path: &Path) -> Result<T> {
    match fs::read(path) {
        Ok(raw) => serde_json::from_slice(&raw)
            .map_err(|e| Error::InvalidInput(format!("{}: {e}", path.display()))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(e) => Err(io(path)(e)),
    }
}

/// Write-then-rename, so a crash never leaves a half-written file.
fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let tmp = path.with_extension("json.tmp");
    let json = serde_json::to_vec_pretty(value).map_err(|e| Error::InvalidInput(e.to_string()))?;
    write(&tmp, &json)?;
    fs::rename(&tmp, path).map_err(io(path))
}

/// Stack tags of the repo checked out at `repo`, from its top-level files
/// (`Cargo.toml`, `package.json` and its dependencies, `go.mod`, ...).
pub fn detect_stacks(repo: &Path) -> Result<Vec<String>> {
    let names: BTreeSet<String> = fs::read_dir(repo)
        .map_err(io(repo))?
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    let has = |n: &str| names.contains(n);
    let mut stacks = BTreeSet::new();
    let mut add = |tags: &[&str]| stacks.extend(tags.iter().map(|t| t.to_string()));
    for (file, tags) in [
        ("Cargo.toml", &["rust"][..]),
        ("go.mod", &["go"]),
        ("pyproject.toml", &["python"]),
        ("requirements.txt", &["python"]),
        ("Gemfile", &["ruby"]),
        ("pom.xml", &["jvm"]),
        ("build.gradle", &["jvm"]),
        ("build.gradle.kts", &["jvm"]),
        ("pubspec.yaml", &["flutter", "mobile"]),
        ("Package.swift", &["ios", "mobile"]),
        ("Dockerfile", &["docker"]),
    ] {
        if has(file) {
            add(tags);
        }
    }
    if names.iter().any(|n| n.ends_with(".xcodeproj")) {
        add(&["ios", "mobile"]);
    }
    if has("package.json") {
        add(&["node"]);
        for dep in package_deps(&repo.join("package.json")) {
            match dep.as_str() {
                "react" | "next" => add(&["react", "frontend"]),
                "vue" | "nuxt" => add(&["vue", "frontend"]),
                "svelte" | "@sveltejs/kit" => add(&["svelte", "frontend"]),
                "@angular/core" => add(&["angular", "frontend"]),
                "react-native" | "expo" => add(&["react-native", "mobile"]),
                "typescript" => add(&["typescript"]),
                _ => {}
            }
        }
    }
    Ok(stacks.into_iter().collect())
}

/// Dependency names of a `package.json`; unreadable or huge files give none.
fn package_deps(path: &Path) -> Vec<String> {
    const MAX: u64 = 1 << 20;
    let small = fs::metadata(path).is_ok_and(|m| m.len() <= MAX);
    let Some(json) = small
        .then(|| fs::read(path).ok())
        .flatten()
        .and_then(|raw| serde_json::from_slice::<serde_json::Value>(&raw).ok())
    else {
        return Vec::new();
    };
    ["dependencies", "devDependencies"]
        .iter()
        .filter_map(|k| json.get(k)?.as_object())
        .flat_map(|deps| deps.keys().cloned())
        .collect()
}

/// Index checks: ids are skill-name shaped, skills have a `SKILL.md`, MCP
/// entries carry a valid server of the same name.
fn check(entry: &Entry) -> Result<()> {
    let id = &entry.id;
    if !is_slug(id) {
        return Err(bad(id, "id must match ^[a-z0-9]+(-[a-z0-9]+)*$"));
    }
    match (entry.kind, &entry.server) {
        (Kind::Skill, None) => match CatalogFiles::get(&format!("skills/{id}/SKILL.md")) {
            Some(file) => check_skill(id, &String::from_utf8_lossy(&file.data)),
            None => Err(bad(id, "a skill needs skills/<id>/SKILL.md")),
        },
        (Kind::Skill, _) => Err(bad(id, "a skill has no server")),
        (Kind::Mcp, Some(server)) if server.name == *id => server.validate(),
        (Kind::Mcp, _) => Err(bad(id, "an mcp entry needs a server named like the id")),
    }
}

/// `^[a-z0-9]+(-[a-z0-9]+)*$`, at most 64 bytes: a skill name.
fn is_slug(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id.split('-').all(|p| {
            !p.is_empty()
                && p.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        })
}

/// Checks a skill's `SKILL.md`: YAML frontmatter between `---` lines with a
/// `name` equal to the skill's folder `name` (Codex and OpenCode require it)
/// and a non-empty `description` (what agents read to pick it).
pub fn check_skill(name: &str, skill_md: &str) -> Result<()> {
    #[derive(Deserialize)]
    struct Front {
        name: Option<String>,
        description: Option<String>,
    }
    if !is_slug(name) {
        return Err(bad(
            name,
            "a skill name must match ^[a-z0-9]+(-[a-z0-9]+)*$",
        ));
    }
    let front = skill_md
        .strip_prefix("---\n")
        .and_then(|rest| rest.split_once("\n---"))
        .map(|(yaml, _)| yaml)
        .ok_or_else(|| bad(name, "SKILL.md must start with `---` frontmatter"))?;
    let front: Front = serde_saphyr::from_str(front)
        .map_err(|e| bad(name, &format!("SKILL.md frontmatter: {e}")))?;
    if front.name.as_deref() != Some(name) {
        return Err(bad(name, "SKILL.md `name:` must equal the folder name"));
    }
    match front.description.as_deref().map(str::trim) {
        Some(d) if !d.is_empty() => Ok(()),
        _ => Err(bad(name, "SKILL.md needs a `description:`")),
    }
}

/// Line diff `old` → `new` (longest common subsequence), each line prefixed
/// with `' '` (both), `'-'` (old only) or `'+'` (new only).
// ponytail: O(lines²) table; past ~4M cells the whole file shows as replaced.
fn line_diff(old: &str, new: &str) -> Vec<String> {
    let (a, b): (Vec<&str>, Vec<&str>) = (old.lines().collect(), new.lines().collect());
    let mark =
        |p: char, lines: &[&str]| lines.iter().map(|l| format!("{p}{l}")).collect::<Vec<_>>();
    if a.len().saturating_mul(b.len()) > 4_000_000 {
        return [mark('-', &a), mark('+', &b)].concat();
    }
    // lcs[i][j]: how many lines a[i..] and b[j..] share in order.
    let mut lcs = vec![vec![0u32; b.len() + 1]; a.len() + 1];
    for i in (0..a.len()).rev() {
        for j in (0..b.len()).rev() {
            lcs[i][j] = match a[i] == b[j] {
                true => lcs[i + 1][j + 1] + 1,
                false => lcs[i + 1][j].max(lcs[i][j + 1]),
            };
        }
    }
    let (mut i, mut j, mut out) = (0, 0, Vec::new());
    while i < a.len() && j < b.len() {
        if a[i] == b[j] {
            out.push(format!(" {}", a[i]));
            (i, j) = (i + 1, j + 1);
        } else if lcs[i + 1][j] >= lcs[i][j + 1] {
            out.push(format!("-{}", a[i]));
            i += 1;
        } else {
            out.push(format!("+{}", b[j]));
            j += 1;
        }
    }
    [out, mark('-', &a[i..]), mark('+', &b[j..])].concat()
}

fn taken(name: &str) -> Error {
    Error::InvalidInput(format!(
        "an MCP server named {name} already exists; pick another name or remove it first"
    ))
}

fn bad(id: &str, why: &str) -> Error {
    Error::InvalidInput(format!("catalog entry `{id}`: {why}"))
}

/// A skill's embedded files, as (path inside the skill, bytes).
fn skill_files(id: &str) -> Vec<(String, Vec<u8>)> {
    let prefix = format!("skills/{id}/");
    CatalogFiles::iter()
        .filter_map(|path| {
            let rel = path.strip_prefix(&prefix)?.to_owned();
            Some((rel, CatalogFiles::get(&path)?.data.into_owned()))
        })
        .collect()
}

/// Relative `/` paths of the files under `root/rel` (none when missing).
fn list_files(root: &Path, rel: &Path, out: &mut Vec<String>) {
    let Ok(entries) = fs::read_dir(root.join(rel)) else {
        return;
    };
    for entry in entries.flatten() {
        let rel = rel.join(entry.file_name());
        match entry.file_type() {
            Ok(kind) if kind.is_dir() => list_files(root, &rel, out),
            Ok(_) => out.push(rel.to_string_lossy().replace('\\', "/")),
            Err(_) => {}
        }
    }
}

fn hosts(server: &McpServer) -> Vec<String> {
    match &server.transport {
        McpTransport::Url { url, .. } => url
            .split_once("://")
            .and_then(|(_, rest)| rest.split(['/', ':']).next())
            .map(|h| vec![h.to_owned()])
            .unwrap_or_default(),
        McpTransport::Command { network, .. } => network.clone(),
    }
}

fn write(path: &Path, data: &[u8]) -> Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(io(dir))?;
    }
    fs::write(path, data).map_err(io(path))
}

fn remove_dir(dir: &Path) -> Result<()> {
    match fs::remove_dir_all(dir) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(io(dir)(e)),
        _ => Ok(()),
    }
}

fn io(path: &Path) -> impl Fn(std::io::Error) -> Error + '_ {
    move |e| Error::Unavailable(format!("{}: {e}", path.display()))
}
