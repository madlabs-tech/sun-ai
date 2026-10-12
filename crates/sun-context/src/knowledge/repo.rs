//! Repo markdown: every `.md` / `.markdown` file under a local
//! checkout, as docs keyed by their path. Blocking; run it on the blocking pool.

use std::path::Path;
use std::process::Command;

use sun_core::Error;

use super::{DocContent, MAX_CONTENT_BYTES, SourceDoc};

/// Most files one repo source indexes.
const MAX_FILES: usize = 5000;
/// Folders never walked (besides dot-folders such as `.git`).
const SKIPPED_DIRS: &[&str] = &["node_modules", "target", "vendor", "dist", "build"];

/// The markdown files under `root` (no symlinks, no dot-folders), each
/// versioned by the checkout's commit and dated by its mtime. Files over the
/// size cap are left out and named in the second list.
pub fn repo_docs(root: &Path) -> Result<(Vec<SourceDoc>, Vec<String>), Error> {
    if !root.is_absolute() || !root.is_dir() {
        return Err(Error::InvalidInput(format!(
            "{} is not a folder on the kernel's machine",
            root.display()
        )));
    }
    let version = commit(root).unwrap_or_else(|| "working copy".into());
    let (mut docs, mut skipped) = (Vec::new(), Vec::new());
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = std::fs::read_dir(&dir)
            .map_err(|e| Error::Unavailable(format!("{}: {e}", dir.display())))?;
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let name = entry.file_name().to_string_lossy().into_owned();
            let path = entry.path();
            if kind.is_dir() {
                if !name.starts_with('.') && !SKIPPED_DIRS.contains(&name.as_str()) {
                    stack.push(path);
                }
                continue;
            }
            let markdown = Path::new(&name).extension().is_some_and(|e| {
                e.eq_ignore_ascii_case("md") || e.eq_ignore_ascii_case("markdown")
            });
            if !kind.is_file() || !markdown {
                continue;
            }
            let relative = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            let Ok(meta) = entry.metadata() else { continue };
            if meta.len() > MAX_CONTENT_BYTES as u64 {
                skipped.push(format!("{relative}: larger than {MAX_CONTENT_BYTES} bytes"));
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                skipped.push(format!("{relative}: not UTF-8 text"));
                continue;
            };
            if docs.len() == MAX_FILES {
                return Err(Error::InvalidInput(format!(
                    "{} has more than {MAX_FILES} markdown files; point the source at a sub-folder",
                    root.display()
                )));
            }
            docs.push(SourceDoc {
                external_id: relative.clone(),
                title: relative,
                version: version.clone(),
                updated_at: meta.modified().ok().map(Into::into),
                url: None,
                acl: Vec::new(),
                content: DocContent::Markdown(text),
            });
        }
    }
    docs.sort_by(|a, b| a.external_id.cmp(&b.external_id));
    Ok((docs, skipped))
}

/// The checkout's short commit, when `root` is in a git repo.
fn commit(root: &Path) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--short=12", "HEAD"])
        .output()
        .ok()?;
    let sha = String::from_utf8(out.stdout).ok()?.trim().to_owned();
    (out.status.success() && !sha.is_empty()).then_some(sha)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_docs_walks_markdown_only() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for (path, text) in [
            ("README.md", "# Readme"),
            ("docs/adr/0001.markdown", "# ADR"),
            ("src/main.rs", "fn main() {}"),
            (".git/notes.md", "# no"),
            ("node_modules/x/README.md", "# no"),
        ] {
            let file = root.join(path);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file, text).unwrap();
        }
        let (docs, skipped) = repo_docs(root).unwrap();
        let ids: Vec<&str> = docs.iter().map(|d| d.external_id.as_str()).collect();
        assert_eq!(ids, ["README.md", "docs/adr/0001.markdown"]);
        assert!(skipped.is_empty());
        assert_eq!(docs[0].version, "working copy");
        assert!(docs[0].updated_at.is_some());
        assert!(repo_docs(Path::new("relative/path")).is_err());
    }
}
