//! File operations in an editor worktree (`fs.*` frames), confined to its
//! root: the trust boundary between a browser's path and the host's disk.
//!
//! Every path from the kernel is relative to the worktree's canonical root.
//! [`lexical`] refuses absolute paths, `..`, and any `.git` part (any case:
//! macOS disks fold it), so the git metadata a host later runs git against
//! can't be edited into a hook. Then the path is canonicalized
//! ([`existing`]) or its parent is ([`entry`]: create, rename, delete and a
//! new file act on the entry itself, never through a final symlink), and
//! the result must still be under the root, so no symlink leads out.
//!
//! A process on the host that swaps a checked directory for a symlink
//! between the check and the use can still escape; only the host's own
//! users (a terminal) can do that, and they already have the host.

use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;
use std::time::UNIX_EPOCH;

use sha2::{Digest, Sha256};
use sun_core::host_protocol::{
    FsEntry, FsFile, FsKind, FsStat, MAX_EDITOR_ENTRIES, MAX_EDITOR_FILE, ReplyBody,
};
use sun_core::{Error, Result};

use crate::worktree::{git, run};

/// Entries shown closed and kept out of the index.
const COLLAPSED: [&str; 2] = [".git", "node_modules"];

// ponytail: one lock for every editor write on the host, so two saves of
// one file can't both pass the hash check; per-file locks if saves get hot.
static WRITES: Mutex<()> = Mutex::new(());

/// `fs.list`: one directory level, directories first, then by name.
pub(crate) fn list(root: &Path, relative: &str) -> Result<(Vec<FsEntry>, bool)> {
    let dir = existing(root, relative)?;
    if !dir.is_dir() {
        return Err(Error::InvalidInput(format!("{relative}: not a directory")));
    }
    let mut entries = Vec::new();
    let mut truncated = false;
    for item in std::fs::read_dir(&dir).map_err(fs_error(relative))? {
        let item = item.map_err(fs_error(relative))?;
        let Ok(name) = item.file_name().into_string() else {
            continue;
        };
        if entries.len() == MAX_EDITOR_ENTRIES {
            truncated = true;
            break;
        }
        let Ok(meta) = std::fs::symlink_metadata(item.path()) else {
            continue; // deleted meanwhile
        };
        let kind = match meta.file_type() {
            t if t.is_symlink() => FsKind::Symlink,
            t if t.is_dir() => FsKind::Dir,
            _ => FsKind::File,
        };
        entries.push(FsEntry {
            size: if kind == FsKind::File { meta.len() } else { 0 },
            collapsed: COLLAPSED.iter().any(|c| c.eq_ignore_ascii_case(&name)),
            name,
            kind,
        });
    }
    entries.sort_by(|a, b| {
        (a.kind != FsKind::Dir)
            .cmp(&(b.kind != FsKind::Dir))
            .then_with(|| a.name.cmp(&b.name))
    });
    Ok((entries, truncated))
}

/// `fs.index`: every tracked or untracked file git doesn't ignore (nested
/// `.gitignore`s and `info/exclude` included), minus `node_modules`, sorted.
pub(crate) async fn index(root: &Path) -> Result<(Vec<String>, bool)> {
    let out = run(git(root).args([
        "ls-files",
        "-z",
        "--cached",
        "--others",
        "--exclude-standard",
    ]))
    .await?;
    let mut paths: Vec<String> = out
        .split('\0')
        .filter(|p| !p.is_empty() && !p.split('/').any(|part| part == "node_modules"))
        // Tracked but deleted on disk: nothing to open.
        .filter(|p| std::fs::symlink_metadata(root.join(p)).is_ok())
        .map(str::to_owned)
        .collect();
    paths.sort();
    paths.dedup();
    let truncated = paths.len() > MAX_EDITOR_ENTRIES;
    paths.truncate(MAX_EDITOR_ENTRIES);
    Ok((paths, truncated))
}

/// `fs.read`: the file's text, or why there is none (too large, binary).
pub(crate) fn read(root: &Path, relative: &str) -> Result<FsFile> {
    let path = existing(root, relative)?;
    let file = std::fs::File::open(&path).map_err(fs_error(relative))?;
    let meta = file.metadata().map_err(fs_error(relative))?;
    if !meta.is_file() {
        return Err(Error::InvalidInput(format!("{relative}: not a file")));
    }
    let mut bytes = Vec::new();
    file.take(MAX_EDITOR_FILE + 1)
        .read_to_end(&mut bytes)
        .map_err(fs_error(relative))?;
    let mtime_ms = mtime_ms(&meta);
    if bytes.len() as u64 > MAX_EDITOR_FILE {
        return Ok(FsFile {
            size: meta.len().max(bytes.len() as u64),
            mtime_ms,
            hash: None,
            too_large: true,
            binary: false,
            content: None,
        });
    }
    let hash = Some(hex(&Sha256::digest(&bytes)));
    let size = bytes.len() as u64;
    let content = match bytes.contains(&0) {
        true => None,
        false => String::from_utf8(bytes).ok(),
    };
    Ok(FsFile {
        size,
        mtime_ms,
        hash,
        too_large: false,
        binary: content.is_none(),
        content,
    })
}

/// `fs.write`: replaces the file atomically (a temp file renamed over it,
/// keeping its mode), unless `expected_hash` is stale: then
/// [`ReplyBody::Conflict`] and nothing is written. A symlink inside the
/// root is written through to its target.
pub(crate) fn write(
    root: &Path,
    relative: &str,
    content: &str,
    expected_hash: Option<&str>,
) -> Result<ReplyBody> {
    if content.len() as u64 > MAX_EDITOR_FILE {
        return Err(Error::InvalidInput(format!(
            "{relative}: larger than {MAX_EDITOR_FILE} bytes"
        )));
    }
    let _guard = WRITES.lock().unwrap_or_else(|e| e.into_inner());
    let mut path = entry(root, relative)?;
    if std::fs::symlink_metadata(&path).is_ok() {
        path = existing(root, relative)?;
        if !path.is_file() {
            return Err(Error::InvalidInput(format!("{relative}: not a file")));
        }
    }
    let current = match path.exists() {
        true => Some(stat(&path, relative)?),
        false => None,
    };
    if let Some(expected) = expected_hash
        && current.as_ref().map(|c| c.hash.as_str()) != Some(expected)
    {
        return Ok(ReplyBody::Conflict { current });
    }
    let parent = path.parent().unwrap_or(root);
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let nanos = std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    let temp = parent.join(format!(".{name}.sun-{}-{nanos}.tmp", std::process::id()));
    let written = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        file.write_all(content.as_bytes())?;
        if current.is_some() {
            file.set_permissions(std::fs::metadata(&path)?.permissions())?;
        }
        file.sync_all()?;
        std::fs::rename(&temp, &path)
    })();
    if let Err(e) = written {
        let _ = std::fs::remove_file(&temp);
        return Err(fs_error(relative)(e));
    }
    Ok(ReplyBody::Stat {
        stat: stat(&path, relative)?,
    })
}

/// `fs.create`: an empty file or a directory; never replaces anything.
pub(crate) fn create(root: &Path, relative: &str, kind: FsKind) -> Result<()> {
    let path = entry(root, relative)?;
    match kind {
        FsKind::File => std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map(drop),
        FsKind::Dir => std::fs::create_dir(&path),
        FsKind::Symlink => {
            return Err(Error::InvalidInput("the editor makes no symlinks".into()));
        }
    }
    .map_err(fs_error(relative))
}

/// `fs.rename`: moves `from` to `to`, which must not exist.
pub(crate) fn rename(root: &Path, from: &str, to: &str) -> Result<()> {
    let (source, target) = (entry(root, from)?, entry(root, to)?);
    std::fs::symlink_metadata(&source).map_err(fs_error(from))?;
    if std::fs::symlink_metadata(&target).is_ok() {
        return Err(Error::InvalidInput(format!("{to}: already exists")));
    }
    std::fs::rename(&source, &target).map_err(fs_error(from))
}

/// `fs.delete`: a file, a symlink itself, or a directory and its contents.
pub(crate) fn delete(root: &Path, relative: &str) -> Result<()> {
    let path = entry(root, relative)?;
    let meta = std::fs::symlink_metadata(&path).map_err(fs_error(relative))?;
    match meta.is_dir() {
        true => std::fs::remove_dir_all(&path),
        false => std::fs::remove_file(&path),
    }
    .map_err(fs_error(relative))
}

/// The plain parts of `relative`: no root, prefix, `..` or `.git`.
fn lexical(relative: &str) -> Result<Vec<&std::ffi::OsStr>> {
    let mut parts = Vec::new();
    for part in Path::new(relative).components() {
        match part {
            Component::CurDir => {}
            Component::Normal(name) if !name.eq_ignore_ascii_case(".git") => parts.push(name),
            _ => {
                return Err(Error::InvalidInput(format!(
                    "{relative}: must be a path inside the repo, outside .git"
                )));
            }
        }
    }
    Ok(parts)
}

/// `relative` canonicalized (symlinks followed), which must exist and be
/// under `root` and outside `.git`.
fn existing(root: &Path, relative: &str) -> Result<PathBuf> {
    let joined = lexical(relative)?
        .into_iter()
        .fold(root.to_path_buf(), |p, part| p.join(part));
    let canonical = joined.canonicalize().map_err(fs_error(relative))?;
    let inside = canonical
        .strip_prefix(root)
        .map_err(|_| outside(relative))?;
    if inside
        .components()
        .any(|c| c.as_os_str().eq_ignore_ascii_case(".git"))
    {
        return Err(outside(relative));
    }
    Ok(canonical)
}

/// The entry `relative` names itself (its final part not followed): its
/// parent canonicalized under `root`, joined with its name. Never the root.
fn entry(root: &Path, relative: &str) -> Result<PathBuf> {
    let mut parts = lexical(relative)?;
    let Some(name) = parts.pop() else {
        return Err(Error::InvalidInput("the repo root itself".into()));
    };
    let parent: PathBuf = parts.iter().collect();
    let parent = existing(root, &parent.to_string_lossy())?;
    if !parent.is_dir() {
        return Err(Error::InvalidInput(format!(
            "{relative}: parent is not a directory"
        )));
    }
    Ok(parent.join(name))
}

fn stat(path: &Path, relative: &str) -> Result<FsStat> {
    let mut file = std::fs::File::open(path).map_err(fs_error(relative))?;
    let meta = file.metadata().map_err(fs_error(relative))?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher).map_err(fs_error(relative))?;
    Ok(FsStat {
        size: meta.len(),
        mtime_ms: mtime_ms(&meta),
        hash: hex(&hasher.finalize()),
    })
}

fn mtime_ms(meta: &std::fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_millis() as u64)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn outside(relative: &str) -> Error {
    Error::InvalidInput(format!("{relative}: leads outside the repo"))
}

/// An I/O error, named by the repo-relative path (never the host's).
fn fs_error(relative: &str) -> impl Fn(std::io::Error) -> Error + '_ {
    move |e| match e.kind() {
        std::io::ErrorKind::NotFound => Error::NotFound(relative.to_owned()),
        std::io::ErrorKind::AlreadyExists => {
            Error::InvalidInput(format!("{relative}: already exists"))
        }
        _ => Error::InvalidInput(format!("{relative}: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    /// A canonical root with `src/main.rs`, a `.git` file and an `outside`
    /// dir next to it holding `secret`.
    fn setup() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let (root, outside) = (base.join("root"), base.join("outside"));
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(root.join(".git"), "gitdir: /x\n").unwrap();
        std::fs::write(outside.join("secret"), "s3cret").unwrap();
        (dir, root, outside)
    }

    fn invalid<T: std::fmt::Debug>(result: Result<T>) -> bool {
        matches!(result, Err(Error::InvalidInput(_)))
    }

    #[test]
    fn traversal_absolute_and_git_paths_are_refused_everywhere() {
        let (_dir, root, outside) = setup();
        let secret = outside.join("secret");
        let absolute = secret.to_str().unwrap();
        for bad in [
            "../outside/secret",
            "src/../../outside/secret",
            "src/../..",
            "..",
            absolute,
            "/etc/passwd",
            ".git",
            ".GIT",
            "src/.git/config",
            ".Git/hooks/pre-commit",
        ] {
            assert!(invalid(read(&root, bad)), "read {bad}");
            assert!(invalid(list(&root, bad)), "list {bad}");
            assert!(invalid(write(&root, bad, "x", None)), "write {bad}");
            assert!(invalid(create(&root, bad, FsKind::File)), "create {bad}");
            assert!(invalid(delete(&root, bad)), "delete {bad}");
            assert!(
                invalid(rename(&root, "src/main.rs", bad)),
                "rename to {bad}"
            );
            assert!(invalid(rename(&root, bad, "moved")), "rename from {bad}");
        }
        assert_eq!(std::fs::read_to_string(&secret).unwrap(), "s3cret");
        assert!(root.join("src/main.rs").is_file());
        // `.` parts and a trailing slash are fine.
        assert!(read(&root, "./src/main.rs").unwrap().content.is_some());
        assert!(!list(&root, "src/").unwrap().0.is_empty());
        // The root itself can't be deleted or renamed.
        assert!(invalid(delete(&root, "")));
        assert!(invalid(delete(&root, ".")));
        assert!(invalid(rename(&root, "", "x")));
    }

    #[test]
    fn symlinks_out_of_the_root_are_refused_and_inside_ones_followed() {
        let (_dir, root, outside) = setup();
        symlink(&outside, root.join("out_dir")).unwrap();
        symlink(outside.join("secret"), root.join("out_file")).unwrap();
        symlink(root.join("src"), root.join("in_dir")).unwrap();
        symlink(root.join(".git"), root.join("git_link")).unwrap();
        symlink("main.rs", root.join("src/in_file")).unwrap();

        assert!(invalid(read(&root, "out_file")));
        assert!(invalid(read(&root, "out_dir/secret")));
        assert!(invalid(list(&root, "out_dir")));
        assert!(invalid(read(&root, "git_link")), "a link into .git");
        assert!(invalid(write(&root, "out_file", "pwned", None)));
        assert!(invalid(write(&root, "out_dir/new", "pwned", None)));
        assert!(invalid(create(&root, "out_dir/new", FsKind::File)));
        assert!(invalid(rename(&root, "src/main.rs", "out_dir/main.rs")));
        assert!(invalid(delete(&root, "out_dir/secret")));
        assert_eq!(
            std::fs::read_to_string(outside.join("secret")).unwrap(),
            "s3cret"
        );
        assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 1);

        // Inside links work, and deleting a link leaves its target.
        assert!(read(&root, "in_dir/main.rs").unwrap().content.is_some());
        let stat = read(&root, "src/in_file").unwrap();
        assert_eq!(stat.content.as_deref(), Some("fn main() {}\n"));
        let saved = write(
            &root,
            "src/in_file",
            "fn main() { }\n",
            stat.hash.as_deref(),
        );
        assert!(matches!(saved, Ok(ReplyBody::Stat { .. })), "{saved:?}");
        assert!(root.join("src/in_file").is_symlink(), "the link stays");
        assert_eq!(
            std::fs::read_to_string(root.join("src/main.rs")).unwrap(),
            "fn main() { }\n"
        );
        delete(&root, "out_dir").unwrap();
        delete(&root, "out_file").unwrap();
        assert!(outside.join("secret").is_file(), "link gone, target kept");

        let (entries, truncated) = list(&root, "").unwrap();
        assert!(!truncated);
        let names: Vec<_> = entries.iter().map(|e| (e.name.as_str(), e.kind)).collect();
        assert_eq!(
            names,
            [
                ("src", FsKind::Dir),
                (".git", FsKind::File),
                ("git_link", FsKind::Symlink),
                ("in_dir", FsKind::Symlink),
            ]
        );
        assert!(entries[1].collapsed && !entries[0].collapsed);
    }

    #[test]
    fn a_stale_hash_is_a_conflict_and_writes_nothing() {
        let (_dir, root, _) = setup();
        let original = read(&root, "src/main.rs").unwrap();
        let hash = original.hash.clone().unwrap();
        let ReplyBody::Stat { stat } = write(&root, "src/main.rs", "v2", Some(&hash)).unwrap()
        else {
            panic!()
        };
        assert_eq!(stat.size, 2);
        assert_ne!(stat.hash, hash);
        // Someone else's edit happened in between: the old hash is stale.
        let stale = write(&root, "src/main.rs", "mine", Some(&hash)).unwrap();
        assert_eq!(
            stale,
            ReplyBody::Conflict {
                current: Some(stat.clone())
            }
        );
        assert_eq!(
            std::fs::read_to_string(root.join("src/main.rs")).unwrap(),
            "v2"
        );
        // Deleted meanwhile: a conflict with nothing current.
        std::fs::remove_file(root.join("src/main.rs")).unwrap();
        let gone = write(&root, "src/main.rs", "mine", Some(&stat.hash)).unwrap();
        assert_eq!(gone, ReplyBody::Conflict { current: None });
        // No precondition: overwrite or create.
        assert!(matches!(
            write(&root, "src/main.rs", "forced", None),
            Ok(ReplyBody::Stat { .. })
        ));
        assert_eq!(
            std::fs::read_to_string(root.join("src/main.rs")).unwrap(),
            "forced"
        );
        let leftovers = std::fs::read_dir(root.join("src")).unwrap().count();
        assert_eq!(leftovers, 1, "no temp files left");
    }

    #[test]
    fn writes_keep_the_file_mode() {
        use std::os::unix::fs::PermissionsExt;
        let (_dir, root, _) = setup();
        let script = root.join("run.sh");
        std::fs::write(&script, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        write(&root, "run.sh", "#!/bin/sh\necho hi\n", None).unwrap();
        let mode = std::fs::metadata(&script).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755);
    }

    #[test]
    fn files_over_two_mib_are_read_without_content_and_never_written() {
        let (_dir, root, _) = setup();
        let cap = MAX_EDITOR_FILE as usize;
        std::fs::write(root.join("exact.txt"), "a".repeat(cap)).unwrap();
        std::fs::write(root.join("big.txt"), "a".repeat(cap + 1)).unwrap();
        let exact = read(&root, "exact.txt").unwrap();
        assert!(!exact.too_large && exact.content.is_some_and(|c| c.len() == cap));
        let big = read(&root, "big.txt").unwrap();
        assert!(big.too_large && big.content.is_none() && big.hash.is_none());
        assert_eq!(big.size, cap as u64 + 1);
        assert!(invalid(write(&root, "new.txt", &"a".repeat(cap + 1), None)));
        assert!(!root.join("new.txt").exists());
        assert!(write(&root, "new.txt", &"a".repeat(cap), None).is_ok());
    }

    #[test]
    fn binary_files_have_no_content() {
        let (_dir, root, _) = setup();
        std::fs::write(root.join("nul.bin"), b"PK\x03\x04\x00\x00").unwrap();
        std::fs::write(root.join("latin1.txt"), b"caf\xe9").unwrap();
        std::fs::write(root.join("utf8.txt"), "café ☀").unwrap();
        for name in ["nul.bin", "latin1.txt"] {
            let file = read(&root, name).unwrap();
            assert!(
                file.binary && file.content.is_none() && file.hash.is_some(),
                "{name}"
            );
        }
        let text = read(&root, "utf8.txt").unwrap();
        assert!(!text.binary);
        assert_eq!(text.content.as_deref(), Some("café ☀"));
        assert!(invalid(read(&root, "src")), "a directory isn't a file");
    }

    #[test]
    fn create_rename_and_delete_never_replace_anything() {
        let (_dir, root, _) = setup();
        create(&root, "src/lib", FsKind::Dir).unwrap();
        create(&root, "src/lib/mod.rs", FsKind::File).unwrap();
        assert!(invalid(create(&root, "src/main.rs", FsKind::File)));
        assert!(invalid(create(&root, "src/lib", FsKind::Dir)));
        assert!(invalid(create(&root, "src/x", FsKind::Symlink)));
        assert!(matches!(
            create(&root, "missing/x.rs", FsKind::File),
            Err(Error::NotFound(_))
        ));
        assert!(invalid(rename(&root, "src/lib/mod.rs", "src/main.rs")));
        assert!(matches!(
            rename(&root, "src/nope.rs", "src/b.rs"),
            Err(Error::NotFound(_))
        ));
        rename(&root, "src/lib", "lib").unwrap();
        assert!(root.join("lib/mod.rs").is_file());
        delete(&root, "lib").unwrap();
        assert!(!root.join("lib").exists());
        assert!(matches!(delete(&root, "lib"), Err(Error::NotFound(_))));
    }

    #[tokio::test]
    async fn index_lists_what_git_does_not_ignore() {
        let (_dir, root, _) = setup();
        std::fs::remove_file(root.join(".git")).unwrap();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(&root)
                .args(["-c", "user.name=t", "-c", "user.email=t@t"])
                .args(args)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        git(&["init", "-q"]);
        std::fs::write(root.join(".gitignore"), "target/\n*.log\n").unwrap();
        std::fs::create_dir_all(root.join("target")).unwrap();
        std::fs::write(root.join("target/out"), "").unwrap();
        std::fs::write(root.join("debug.log"), "").unwrap();
        std::fs::create_dir_all(root.join("node_modules/x")).unwrap();
        std::fs::write(root.join("node_modules/x/i.js"), "").unwrap();
        std::fs::write(root.join("tracked.md"), "").unwrap();
        git(&["add", "tracked.md", "src"]);
        git(&["commit", "-q", "-m", "init"]);
        std::fs::write(root.join("new file.txt"), "").unwrap();
        let (paths, truncated) = index(&root).await.unwrap();
        assert!(!truncated);
        assert_eq!(
            paths,
            [".gitignore", "new file.txt", "src/main.rs", "tracked.md"]
        );
        std::fs::remove_file(root.join("tracked.md")).unwrap();
        let (paths, _) = index(&root).await.unwrap();
        assert!(!paths.iter().any(|p| p == "tracked.md"), "deleted on disk");
    }
}
