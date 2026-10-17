//! Task working directories: a git worktree on the branch the kernel names
//! (a task's `sun/<KEY>-<slug>`) cut from a cached bare clone, or a plain
//! task folder.
//!
//! Layout under the host's data dir: `repos/<escaped url>.git` (bare clones,
//! shared by every task of a repo) and `work/<task_id>` (one directory per task,
//! reused by every session of the task, so a hand-off continues in place).

use std::os::unix::fs::DirBuilderExt;
use std::path::{Component, Path, PathBuf};

use sun_core::host_protocol::{SessionFile, Worktree};
use sun_core::redact::redact;
use sun_core::{Error, Result, TaskId};
use tokio::process::Command;
use tokio::sync::Mutex;

use crate::buffer::{io, write_file};

/// Prepares and checks task directories.
pub struct Worktrees {
    repos: PathBuf,
    work: PathBuf,
    // ponytail: one lock for all git work (clone/fetch/worktree add are not
    // safe to race on one bare repo); per-repo locks if prepare gets hot.
    git_lock: Mutex<()>,
}

impl Worktrees {
    /// Worktrees under `data_dir`.
    pub fn new(data_dir: &Path) -> Result<Self> {
        let (repos, work) = (data_dir.join("repos"), data_dir.join("work"));
        for dir in [&repos, &work] {
            std::fs::create_dir_all(dir).map_err(io(dir))?;
        }
        Ok(Self {
            repos,
            work: work.canonicalize().map_err(io(&work))?,
            git_lock: Mutex::new(()),
        })
    }

    /// Creates the task directory if missing and returns its absolute path.
    /// A multi-repo task's directory holds one worktree per repo at `<name>/`.
    pub async fn prepare(&self, worktree: &Worktree) -> Result<PathBuf> {
        let _guard = self.git_lock.lock().await;
        let path = self.work.join(worktree.task_id().to_string());
        match worktree {
            Worktree::Folder { .. } => {
                std::fs::create_dir_all(&path).map_err(io(&path))?;
            }
            Worktree::Repo {
                url, base, branch, ..
            } => {
                self.add_worktree(url, base, branch, &path).await?;
            }
            Worktree::Repos { repos, .. } => {
                let mut names = std::collections::HashSet::new();
                for repo in repos {
                    validate_name(&repo.name)?;
                    if !names.insert(&repo.name) {
                        return Err(Error::InvalidInput(format!(
                            "repo folder `{}` is used twice",
                            repo.name
                        )));
                    }
                }
                std::fs::create_dir_all(&path).map_err(io(&path))?;
                for repo in repos {
                    let dir = path.join(&repo.name);
                    self.add_worktree(&repo.url, &repo.base, &repo.branch, &dir)
                        .await?;
                }
            }
        }
        Ok(path)
    }

    /// The worktree of `url` at `path` on `branch`, unless `path` exists.
    /// Call with `git_lock` held.
    async fn add_worktree(
        &self,
        url: &str,
        base: &Option<String>,
        branch: &str,
        path: &Path,
    ) -> Result<()> {
        validate_url(url)?;
        validate_ref(branch)?;
        if let Some(base) = base {
            validate_ref(base)?;
        }
        if path.exists() {
            return Ok(());
        }
        let (bare, base) = self.fetch(url, base).await?;
        run(git(&bare).args(["worktree", "prune"])).await?;
        let exists = |name: String| async {
            run(git(&bare)
                .args(["rev-parse", "--verify", "--quiet"])
                .arg(name))
            .await
            .is_ok()
        };
        let mut add = git(&bare);
        add.args(["worktree", "add"]);
        if exists(format!("refs/heads/{branch}")).await {
            // The task's branch survived its worktree: check it out again.
            add.arg(path).arg(branch);
        } else {
            // A run moved here mid-step pushed its branch: continue from it.
            let pushed = format!("refs/remotes/origin/{branch}");
            let start = match exists(pushed.clone()).await {
                true => pushed,
                false => format!("refs/remotes/origin/{base}"),
            };
            add.args(["-b", branch]).arg(path).arg(start);
        }
        run(&mut add).await?;
        Ok(())
    }

    /// Removes the task's directory and its preview checkout, then prunes
    /// the clones' worktree records. The task branches stay in the clones.
    /// Idempotent.
    pub async fn remove(&self, task_id: TaskId) -> Result<()> {
        let _guard = self.git_lock.lock().await;
        for dir in [task_id.to_string(), format!("{task_id}.base")] {
            let path = self.work.join(dir);
            match std::fs::remove_dir_all(&path) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(io(&path)(e)),
                _ => {}
            }
        }
        let clones = std::fs::read_dir(&self.repos).map_err(io(&self.repos))?;
        for clone in clones.flatten() {
            if clone.path().extension().is_some_and(|e| e == "git") {
                run(git(&clone.path()).args(["worktree", "prune"])).await?;
            }
        }
        Ok(())
    }

    /// The `before` checkout of a repo task for previews: a detached
    /// worktree of the base branch at `work/<task_id>.base`. Created once.
    // ponytail: never refreshed after creation; re-fetch when previews of
    // long-lived tasks must track a moving base.
    pub async fn prepare_base(&self, worktree: &Worktree) -> Result<PathBuf> {
        let Worktree::Repo {
            task_id, url, base, ..
        } = worktree
        else {
            return Err(Error::InvalidInput(
                "only a single-repo task has a base branch to preview".into(),
            ));
        };
        validate_url(url)?;
        if let Some(base) = base {
            validate_ref(base)?;
        }
        let path = self.work.join(format!("{task_id}.base"));
        let _guard = self.git_lock.lock().await;
        if path.exists() {
            return Ok(path);
        }
        let (bare, base) = self.fetch(url, base).await?;
        run(git(&bare).args(["worktree", "prune"])).await?;
        run(git(&bare)
            .args(["worktree", "add", "--detach"])
            .arg(&path)
            .arg(format!("refs/remotes/origin/{base}")))
        .await?;
        Ok(path)
    }

    /// Clones `url` once (bare), fetches it, and resolves `base` (default:
    /// the remote's default branch). Call with `git_lock` held.
    async fn fetch(&self, url: &str, base: &Option<String>) -> Result<(PathBuf, String)> {
        let bare = self.repos.join(format!("{}.git", escape(url)));
        if !bare.exists() {
            let partial = bare.with_extension("partial");
            let _ = std::fs::remove_dir_all(&partial);
            run(git(&self.repos)
                .args(["clone", "--bare", "--", url])
                .arg(&partial))
            .await?;
            std::fs::rename(&partial, &bare).map_err(io(&bare))?;
            exclude_sun_dir(&bare)?;
        }
        run(git(&bare).args(["fetch", "origin", "+refs/heads/*:refs/remotes/origin/*"])).await?;
        let base = match base {
            Some(base) => base.clone(),
            None => run(git(&bare).args(["symbolic-ref", "--short", "HEAD"]))
                .await?
                .trim()
                .to_owned(),
        };
        Ok((bare, base))
    }

    /// Bytes on disk per task folder (`<task_id>` plus its `.base` preview
    /// checkout), from one `du -sk` over `work/`.
    pub async fn disk_use(&self) -> Vec<(TaskId, u64)> {
        let Ok(entries) = std::fs::read_dir(&self.work) else {
            return Vec::new();
        };
        let dirs: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
        if dirs.is_empty() {
            return Vec::new();
        }
        let Ok(output) = Command::new("du").arg("-sk").args(&dirs).output().await else {
            return Vec::new();
        };
        let mut tasks: std::collections::BTreeMap<String, u64> = Default::default();
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            let Some((kb, path)) = line.split_once('\t') else {
                continue;
            };
            let name = Path::new(path).file_name().and_then(|n| n.to_str());
            let task = name.map(|n| n.trim_end_matches(".base").to_owned());
            if let (Ok(kb), Some(task)) = (kb.trim().parse::<u64>(), task) {
                *tasks.entry(task).or_default() += kb * 1024;
            }
        }
        tasks
            .into_iter()
            .filter_map(|(task, bytes)| Some((task.parse().ok()?, bytes)))
            .collect()
    }

    /// Checks that `path` is a task directory under `work/`, or a folder
    /// directly in one (a multi-repo task's worktree), and returns it
    /// canonicalized (trust boundary: the kernel must not point a session or
    /// command anywhere else on the host).
    pub fn check(&self, path: &Path) -> Result<PathBuf> {
        let canonical = path
            .canonicalize()
            .map_err(|e| Error::InvalidInput(format!("{}: {e}", path.display())))?;
        let work = Some(self.work.as_path());
        if canonical.parent() != work && canonical.parent().and_then(Path::parent) != work {
            return Err(Error::InvalidInput(format!(
                "{} is not a prepared worktree",
                path.display()
            )));
        }
        Ok(canonical)
    }
}

/// `worktree.write`: writes a session's `.sun/` files under `root` (a path
/// [`Worktrees::check`] returned). `.sun/context/` is derived, so it is
/// cleared first; `overwrite: false` files are only created when missing.
/// Paths must be relative and under `.sun/`, and no existing part of a path
/// may be a symlink, so an agent can't redirect a write out of the worktree.
/// Files are written `0600` and the dirs Sun creates `0700`: `mcp.json`
/// holds the session's `SUN_MCP_TOKEN`. Every exec env runs the agent as
/// the worktree's owner, so it still reads them.
pub fn write_files(root: &Path, files: &[SessionFile]) -> Result<()> {
    for file in files {
        let mut parts = Path::new(&file.path).components();
        let under_sun = parts.next() == Some(Component::Normal(".sun".as_ref()));
        if !under_sun
            || parts.clone().next().is_none()
            || !parts.all(|c| matches!(c, Component::Normal(_)))
        {
            return Err(Error::InvalidInput(format!(
                "{}: must be a relative path under .sun/",
                file.path
            )));
        }
        no_symlink(root, Path::new(&file.path))?;
    }
    let context = root.join(".sun/context");
    no_symlink(root, Path::new(".sun/context"))?;
    match std::fs::remove_dir_all(&context) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(io(&context)(e)),
        _ => {}
    }
    for file in files {
        let path = root.join(&file.path);
        if !file.overwrite && path.exists() {
            continue;
        }
        if let Some(parent) = path.parent() {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(parent)
                .map_err(io(parent))?;
        }
        write_file(&path, &file.contents, 0o600)?;
    }
    Ok(())
}

/// `InvalidInput` when any existing prefix of `root/relative` is a symlink.
fn no_symlink(root: &Path, relative: &Path) -> Result<()> {
    let mut path = root.to_path_buf();
    for part in relative.components() {
        path.push(part);
        if std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err(Error::InvalidInput(format!(
                "{}: is a symlink",
                relative.display()
            )));
        }
    }
    Ok(())
}

/// `git -C <dir>`, never prompting for credentials.
fn git(dir: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(std::process::Stdio::null());
    cmd
}

/// Runs a git command and returns its stdout.
async fn run(cmd: &mut Command) -> Result<String> {
    let output = cmd
        .output()
        .await
        .map_err(|e| Error::Unavailable(format!("cannot run git: {e}")))?;
    if !output.status.success() {
        return Err(Error::Unavailable(redact(
            &format!(
                "git failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
            &[],
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Adds `.sun/` to the clone's `info/exclude`, which every worktree of it
/// reads, so context files never land in commits.
fn exclude_sun_dir(bare: &Path) -> Result<()> {
    let path = bare.join("info").join("exclude");
    let mut text = std::fs::read_to_string(&path).unwrap_or_default();
    if !text.lines().any(|l| l.trim() == ".sun/") {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(".sun/\n");
        std::fs::create_dir_all(bare.join("info")).map_err(io(bare))?;
        std::fs::write(&path, text).map_err(io(&path))?;
    }
    Ok(())
}

/// Rejects URLs git could read as an option or a remote helper (`ext::cmd`).
fn validate_url(url: &str) -> Result<()> {
    if url.is_empty()
        || url.starts_with('-')
        || url.contains("::")
        || url.chars().any(char::is_control)
    {
        return Err(Error::InvalidInput(format!("bad repo url `{url}`")));
    }
    Ok(())
}

/// A multi-repo task's folder name: one plain path component.
fn validate_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 100
        && !name.starts_with(['.', '-'])
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b));
    if !ok {
        return Err(Error::InvalidInput(format!("bad repo folder `{name}`")));
    }
    Ok(())
}

fn validate_ref(name: &str) -> Result<()> {
    if name.is_empty()
        || name.starts_with('-')
        || name.contains("..")
        || name
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || "~^:?*[\\".contains(c))
    {
        return Err(Error::InvalidInput(format!("bad branch `{name}`")));
    }
    Ok(())
}

/// A URL as one file name, reversibly: `[A-Za-z0-9.-]` kept, every other byte
/// as `_xx` hex, so two URLs never share a cache directory.
fn escape(url: &str) -> String {
    url.bytes()
        .map(|b| match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'.' | b'-' => (b as char).to_string(),
            _ => format!("_{b:02x}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn disk_use_sums_each_task_folder_and_its_preview_checkout() {
        let dir = tempfile::tempdir().unwrap();
        let worktrees = Worktrees::new(dir.path()).unwrap();
        let task = TaskId::generate();
        for folder in [task.to_string(), format!("{task}.base")] {
            let path = dir.path().join("work").join(folder);
            std::fs::create_dir_all(&path).unwrap();
            std::fs::write(path.join("blob"), vec![7u8; 64 * 1024]).unwrap();
        }
        std::fs::create_dir_all(dir.path().join("work/not-a-task")).unwrap();
        let used = worktrees.disk_use().await;
        assert_eq!(used.len(), 1);
        assert_eq!(used[0].0, task);
        assert!(used[0].1 >= 128 * 1024, "{used:?}");
    }

    #[test]
    fn escape_is_injective_and_validation_rejects_injection() {
        assert_eq!(escape("git@h.com:a/b_c"), "git_40h.com_3aa_2fb_5fc");
        assert_ne!(escape("a/b"), escape("a_b"));
        assert!(validate_url("-uupload-pack=x").is_err());
        assert!(validate_url("ext::sh -c touch% /tmp/pwned").is_err());
        assert!(validate_url("https://github.com/a/b.git").is_ok());
        assert!(validate_ref("main").is_ok());
        assert!(validate_ref("--force").is_err());
        assert!(validate_ref("a..b").is_err());
        assert!(validate_name("api-server_2.x").is_ok());
        for bad in ["", ".sun", "..", "-x", "a/b", "a b"] {
            assert!(validate_name(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn write_files_stays_under_sun_and_refuses_symlinks() {
        let root = std::env::temp_dir().join(format!("sun-write-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(".sun/context/sources")).unwrap();
        std::fs::write(root.join(".sun/context/sources/stale.md"), "old").unwrap();
        std::fs::write(root.join(".sun/progress.md"), "mine").unwrap();
        let file = |path: &str, overwrite| SessionFile {
            path: path.into(),
            contents: "new".into(),
            overwrite,
        };
        write_files(
            &root,
            &[
                file(".sun/context/pack.md", true),
                file(".sun/progress.md", false),
            ],
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join(".sun/context/pack.md")).unwrap(),
            "new"
        );
        assert!(
            !root.join(".sun/context/sources/stale.md").exists(),
            "context is cleared"
        );
        assert_eq!(
            std::fs::read_to_string(root.join(".sun/progress.md")).unwrap(),
            "mine"
        );
        for bad in ["/etc/x", "../x", ".sun/../../x", "src/main.rs", ".sun"] {
            assert!(write_files(&root, &[file(bad, true)]).is_err(), "{bad}");
        }
        // An agent-made symlink can't redirect a write.
        let outside = root.with_extension("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::remove_dir_all(root.join(".sun")).unwrap();
        std::os::unix::fs::symlink(&outside, root.join(".sun")).unwrap();
        assert!(write_files(&root, &[file(".sun/progress.md", true)]).is_err());
        assert!(!outside.join("progress.md").exists());
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn session_files_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // A config left by an older Sun, and an existing agent-readable file.
        std::fs::create_dir_all(root.join(".sun/context")).unwrap();
        std::fs::write(root.join(".sun/context/mcp.json"), "old").unwrap();
        std::fs::write(root.join(".sun/notes.md"), "old").unwrap();
        let loose = std::fs::Permissions::from_mode(0o644);
        std::fs::set_permissions(root.join(".sun/context/mcp.json"), loose.clone()).unwrap();
        std::fs::set_permissions(root.join(".sun/notes.md"), loose).unwrap();
        let config = r#"{"mcpServers":{"sun":{"command":"sun","args":["mcp"],"env":{"SUN_MCP_TOKEN":"t"}}}}"#;
        let file = |path: &str, contents: &str| SessionFile {
            path: path.into(),
            contents: contents.into(),
            overwrite: true,
        };
        write_files(
            root,
            &[
                file(".sun/context/mcp.json", config),
                file(".sun/context/sources/a.md", "a"),
                file(".sun/notes.md", "new"),
            ],
        )
        .unwrap();
        let mcp = root.join(".sun/context/mcp.json");
        assert_eq!(std::fs::read_to_string(&mcp).unwrap(), config);
        assert_eq!(mode(&mcp), 0o600, "the session token is owner-only");
        assert_eq!(mode(&root.join(".sun/context")), 0o700);
        assert_eq!(mode(&root.join(".sun/context/sources")), 0o700);
        assert_eq!(mode(&root.join(".sun/context/sources/a.md")), 0o600);
        assert_eq!(
            mode(&root.join(".sun/notes.md")),
            0o600,
            "existing file tightened"
        );
    }
}
