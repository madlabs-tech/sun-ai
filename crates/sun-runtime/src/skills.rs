//! Skills in each CLI's native folder.
//!
//! The host calls [`sync`] before every session: each skill in the ecosystem
//! store (`<store>/<name>/SKILL.md`, filled by `sun-catalog`) is copied into
//! the session worktree's skills folder for that runtime, instead of being
//! pasted into the prompt. Copies Sun writes carry a `.gitignore` of `*`, so
//! they never land in the task's commits, and are refreshed or removed on the
//! next sync. Folders without that marker (e.g. committed in the repo) are
//! never touched, and a skill the CLI already sees under that name is skipped.
//! A skill the store's [`SKILL_RUNTIMES_FILE`] limits to other runtimes is
//! not copied (and an earlier copy is removed).

use std::fs;
use std::path::Path;

use std::collections::BTreeMap;

use sun_core::{Error, Result, SKILL_RUNTIMES_FILE, skill_folders as folders};

/// First line of the `.gitignore` marking a folder Sun copied.
const MARKER: &str = "# copied by sun for this session; not part of the repo\n*\n";

/// The project skills folder of `adapter_id`, relative to the worktree.
pub fn native_dir(adapter_id: &str) -> Option<&'static str> {
    folders(adapter_id).first().copied()
}

/// Makes `<worktree>/<native_dir>` hold exactly the store's skills (plus any
/// the repo brings itself). A runtime without a skills folder is a no-op, as
/// is a missing store.
pub fn sync(store: &Path, worktree: &Path, adapter_id: &str) -> Result<()> {
    let Some((native, others)) = folders(adapter_id).split_first() else {
        return Ok(());
    };
    let target = worktree.join(native);
    let limits: BTreeMap<String, Vec<String>> = match fs::read(store.join(SKILL_RUNTIMES_FILE)) {
        Ok(raw) => serde_json::from_slice(&raw)
            .map_err(|e| Error::InvalidInput(format!("{SKILL_RUNTIMES_FILE}: {e}")))?,
        Err(_) => BTreeMap::new(),
    };
    let wanted: Vec<String> = match fs::read_dir(store) {
        Ok(dir) => dir
            .filter_map(|e| e.ok())
            .filter(|e| e.path().join("SKILL.md").is_file())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| !name.starts_with('.'))
            .filter(|name| {
                limits
                    .get(name)
                    .is_none_or(|r| r.iter().any(|a| a == adapter_id))
            })
            .collect(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(io(store)(e)),
    };
    // Drop stale copies (uninstalled, or about to be refreshed).
    if let Ok(dir) = fs::read_dir(&target) {
        for e in dir.filter_map(|e| e.ok()) {
            if ours(&e.path()) {
                fs::remove_dir_all(e.path()).map_err(io(&e.path()))?;
            }
        }
    }
    for name in wanted {
        let seen = std::iter::once(native)
            .chain(others)
            .any(|f| worktree.join(f).join(&name).exists());
        if !seen {
            let dest = target.join(&name);
            copy_dir(&store.join(&name), &dest)?;
            fs::write(dest.join(".gitignore"), MARKER).map_err(io(&dest))?;
        }
    }
    Ok(())
}

fn ours(dir: &Path) -> bool {
    fs::read_to_string(dir.join(".gitignore")).is_ok_and(|s| s == MARKER)
}

fn copy_dir(from: &Path, to: &Path) -> Result<()> {
    fs::create_dir_all(to).map_err(io(to))?;
    for e in fs::read_dir(from).map_err(io(from))?.filter_map(|e| e.ok()) {
        let (src, dst) = (e.path(), to.join(e.file_name()));
        // ponytail: symlinks are skipped, not followed; the store only holds plain files.
        let kind = e.file_type().map_err(io(&src))?;
        if kind.is_dir() {
            copy_dir(&src, &dst)?;
        } else if kind.is_file() {
            fs::copy(&src, &dst).map_err(io(&src))?;
        }
    }
    Ok(())
}

fn io(path: &Path) -> impl Fn(std::io::Error) -> Error + '_ {
    move |e| Error::Unavailable(format!("{}: {e}", path.display()))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn skill(store: &Path, name: &str) {
        let dir = store.join(name);
        fs::create_dir_all(dir.join("scripts")).unwrap();
        fs::write(dir.join("SKILL.md"), format!("---\nname: {name}\n---\n")).unwrap();
        fs::write(dir.join("scripts/run.sh"), "echo").unwrap();
    }

    #[test]
    fn catalog_repo_dirs_reach_each_runtime_once() {
        use sun_core::repo_skill_dirs as repo_dirs;
        let ids = |ids: &[&str]| ids.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(repo_dirs(None), [".claude/skills", ".agents/skills"]);
        assert_eq!(repo_dirs(Some(&ids(&["opencode"]))), [".opencode/skills"]);
        assert_eq!(
            repo_dirs(Some(&ids(&["opencode", "codex"]))),
            [".agents/skills"]
        );
        assert!(repo_dirs(Some(&ids(&["aider"]))).is_empty());
    }

    #[test]
    fn catalog_skills_sync_into_each_runtime_native_folder() {
        let store = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let (store, work) = (store.path(), work.path());
        skill(store, "tdd");
        skill(store, "frontend-design");
        // The repo brings its own `frontend-design` for Claude: left alone.
        fs::create_dir_all(work.join(".claude/skills/frontend-design")).unwrap();
        fs::write(work.join(".claude/skills/frontend-design/SKILL.md"), "repo").unwrap();

        sync(store, work, "claude-code").unwrap();
        assert!(work.join(".claude/skills/tdd/scripts/run.sh").is_file());
        assert!(ours(&work.join(".claude/skills/tdd")));
        let repo_owned = work.join(".claude/skills/frontend-design/SKILL.md");
        assert_eq!(fs::read_to_string(&repo_owned).unwrap(), "repo");

        sync(store, work, "codex").unwrap();
        assert!(work.join(".agents/skills/tdd/SKILL.md").is_file());
        assert!(
            work.join(".agents/skills/frontend-design/SKILL.md")
                .is_file()
        );

        // OpenCode already sees both through `.claude`/`.agents`: no duplicates.
        sync(store, work, "opencode").unwrap();
        assert!(!work.join(".opencode/skills").exists());

        // Uninstall: the next session drops Sun's copy, never the repo's.
        fs::remove_dir_all(store.join("tdd")).unwrap();
        sync(store, work, "claude-code").unwrap();
        assert!(!work.join(".claude/skills/tdd").exists());
        assert!(repo_owned.is_file());

        // A skill limited to Codex leaves Claude's folder.
        let limits = r#"{"frontend-design": ["claude-code"], "tdd": ["codex"]}"#;
        fs::write(store.join(SKILL_RUNTIMES_FILE), limits).unwrap();
        skill(store, "tdd");
        sync(store, work, "claude-code").unwrap();
        assert!(!work.join(".claude/skills/tdd").exists());
        sync(store, work, "codex").unwrap();
        assert!(work.join(".agents/skills/tdd").is_dir());
        assert!(!work.join(".agents/skills/frontend-design").exists());
        fs::remove_dir_all(store.join("tdd")).unwrap();

        sync(store, work, "fake").unwrap();
        sync(&store.join("missing"), work, "codex").unwrap();
        assert!(!work.join(".agents/skills/tdd").exists());
        assert_eq!(native_dir("opencode"), Some(".opencode/skills"));
    }
}
