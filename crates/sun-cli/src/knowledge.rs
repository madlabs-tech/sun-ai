//! `sun knowledge`: knowledge sources through the running kernel's REST API
//! (`/api/v1/knowledge/**`). `ingest` walks local folders for markdown and
//! text files (uploaded as docs) and xlsx, docx and PDF files (uploaded as
//! base64 files the kernel converts); connectors sync on the kernel.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use clap::Subcommand;
use serde_json::{Map, Value, json};
use sun_context::knowledge::FILE_EXTENSIONS;
use sun_mod_sdk::http::encode;

use crate::client::Client;

/// File extensions `ingest` uploads.
const TEXT_EXTENSIONS: &[&str] = &["md", "markdown", "txt"];
/// Docs per upload request (the API's cap).
const BATCH: usize = 500;
/// Binary files per upload request (the API's body cap is 24 MiB).
const FILE_BATCH: usize = 2;

/// What `sun knowledge` can do.
#[derive(Subcommand)]
pub enum KnowledgeCommand {
    /// Add a source: `upload`, `repo` (`--set path=/abs/checkout`), or a connector
    /// (`gdrive`, `confluence`, `jira`, `slack`; `--set export_webhook=https://…`).
    Add {
        /// Source kind.
        kind: String,
        /// Display name.
        name: String,
        /// Connector settings, e.g. `--set space=ENG`, `--set max_age_days=90`.
        #[arg(long = "set", value_name = "KEY=VALUE")]
        settings: Vec<String>,
    },
    /// List sources with doc counts and their last sync.
    List,
    /// Remove a source from the workspace; it is deleted with everything indexed
    /// from it once no workspace has it.
    Rm {
        /// Source id.
        source: String,
    },
    /// Upload markdown, text, xlsx, docx and PDF files (folders are walked).
    Ingest {
        /// Source id.
        source: String,
        /// Files or folders.
        #[arg(required = true)]
        paths: Vec<PathBuf>,
    },
    /// Sync a connector source now.
    Reindex {
        /// Source id.
        source: String,
    },
    /// Search the index; hits come with citations.
    Search {
        /// Free text.
        query: String,
        /// Only these source ids.
        #[arg(long = "source")]
        sources: Vec<String>,
    },
}

/// Runs `command` against the kernel; `workspace` (id or name) scopes add, list,
/// rm and search (default Main; `list` without it shows every source).
pub fn run(client: &Client, command: KnowledgeCommand, workspace: Option<&str>) -> Result<()> {
    let workspace = workspace.map(|b| client.workspace_id(b)).transpose()?;
    // `?workspace=` for rm and search; absent means Main on the kernel.
    let workspace_query = workspace
        .as_ref()
        .map_or_else(String::new, |b| format!("workspace={b}"));
    match command {
        KnowledgeCommand::Add {
            kind,
            name,
            settings,
        } => {
            let source = client.post(
                "/knowledge/sources",
                &json!({
                    "kind": kind,
                    "name": name,
                    "settings": parse_settings(&settings)?,
                    "workspace_id": workspace,
                }),
            )?;
            println!("{}  {kind}  {name}", text(&source["id"]));
        }
        KnowledgeCommand::List => {
            for s in client
                .get(&format!("/knowledge/sources?{workspace_query}"))?
                .as_array()
                .into_iter()
                .flatten()
            {
                let error = s["last_sync"]["error"].as_str().unwrap_or_default();
                println!(
                    "{}  {:<10} {:<24} {} docs  {}",
                    text(&s["id"]),
                    text(&s["kind"]),
                    text(&s["name"]),
                    s["docs"],
                    if error.is_empty() {
                        text(&s["last_indexed_at"])
                    } else {
                        error
                    },
                );
            }
        }
        KnowledgeCommand::Rm { source } => {
            client.delete(&format!("/knowledge/sources/{source}?{workspace_query}"))?;
            println!("removed {source} (deleted once no workspace has it)");
        }
        KnowledgeCommand::Ingest { source, paths } => {
            let mut files = Vec::new();
            for path in &paths {
                walk(path, &mut files)?;
            }
            if files.is_empty() {
                bail!("no .md, .markdown, .txt, .xlsx, .docx or .pdf files under the given paths");
            }
            let (mut docs, mut binaries) = (Vec::new(), Vec::new());
            for (path, external_id) in &files {
                if has_extension(path, FILE_EXTENSIONS) {
                    let bytes = std::fs::read(path)
                        .with_context(|| format!("reading {}", path.display()))?;
                    binaries.push(json!({
                        "external_id": external_id,
                        "title": path.file_stem().map_or_else(
                            || external_id.clone(),
                            |n| n.to_string_lossy().into_owned(),
                        ),
                        "version": format!("{:x}", fnv(&bytes)),
                        "content_base64": BASE64.encode(&bytes),
                    }));
                    continue;
                }
                let markdown = std::fs::read_to_string(path)
                    .with_context(|| format!("reading {}", path.display()))?;
                docs.push(json!({
                    "external_id": external_id,
                    "title": title_of(&markdown, path),
                    "version": format!("{:x}", fnv(markdown.as_bytes())),
                    "content": { "markdown": markdown },
                }));
            }
            for batch in docs.chunks(BATCH) {
                let report = client.post(
                    &format!("/knowledge/sources/{source}/docs"),
                    &json!({ "docs": batch }),
                )?;
                print_report(&report);
            }
            for batch in binaries.chunks(FILE_BATCH) {
                let report = client.post(
                    &format!("/knowledge/sources/{source}/docs"),
                    &json!({ "files": batch }),
                )?;
                print_report(&report);
            }
        }
        KnowledgeCommand::Reindex { source } => {
            print_report(
                &client.post(&format!("/knowledge/sources/{source}/reindex"), &json!({}))?,
            );
        }
        KnowledgeCommand::Search { query, sources } => {
            let mut path = format!("/knowledge/search?q={}&{workspace_query}", encode(&query));
            if !sources.is_empty() {
                path += &format!("&sources={}", encode(&sources.join(",")));
            }
            for hit in client.get(&path)?.as_array().into_iter().flatten() {
                println!(
                    "{}\n  {}\n",
                    text(&hit["citation"]),
                    first_line(text(&hit["text"]))
                );
            }
        }
    }
    Ok(())
}

fn text(value: &Value) -> &str {
    value.as_str().unwrap_or("-")
}

fn first_line(text: &str) -> &str {
    text.lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or_default()
}

fn print_report(report: &Value) {
    println!(
        "{} indexed, {} unchanged, {} retired",
        report["indexed"], report["unchanged"], report["retired"]
    );
    for skipped in report["skipped"].as_array().into_iter().flatten() {
        println!("  not indexed: {}", text(skipped));
    }
}

/// `key=value` pairs; whole numbers become numbers (`max_age_days`).
fn parse_settings(pairs: &[String]) -> Result<Value> {
    let mut map = Map::new();
    for pair in pairs {
        let Some((key, value)) = pair.split_once('=') else {
            bail!("--set takes KEY=VALUE, got `{pair}`");
        };
        let value = value
            .parse::<u64>()
            .map_or_else(|_| json!(value), |n| json!(n));
        map.insert(key.trim().to_owned(), value);
    }
    Ok(Value::Object(map))
}

/// Collects `(file, external id)`: a file as given, a folder's text files
/// with ids relative to it. Hidden entries are skipped.
fn walk(path: &Path, out: &mut Vec<(PathBuf, String)>) -> Result<()> {
    if path.is_file() {
        out.push((path.to_owned(), path.display().to_string()));
        return Ok(());
    }
    let mut stack = vec![path.to_owned()];
    while let Some(dir) = stack.pop() {
        let mut entries: Vec<_> = std::fs::read_dir(&dir)
            .with_context(|| format!("reading {}", dir.display()))?
            .collect::<std::io::Result<_>>()?;
        entries.sort_by_key(std::fs::DirEntry::path);
        for entry in entries {
            let file = entry.path();
            if entry.file_name().to_string_lossy().starts_with('.') {
                continue;
            }
            if entry.file_type()?.is_dir() {
                stack.push(file);
            } else if has_extension(&file, TEXT_EXTENSIONS) || has_extension(&file, FILE_EXTENSIONS)
            {
                let id = file
                    .strip_prefix(path)
                    .unwrap_or(&file)
                    .display()
                    .to_string();
                out.push((file, id));
            }
        }
    }
    Ok(())
}

fn has_extension(path: &Path, extensions: &[&str]) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| extensions.contains(&e.to_lowercase().as_str()))
}

/// The first `# heading`, else the file name.
fn title_of(markdown: &str, path: &Path) -> String {
    markdown
        .lines()
        .find_map(|l| l.strip_prefix("# "))
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| {
            path.file_name()
                .map_or_else(|| "untitled".into(), |n| n.to_string_lossy().into_owned())
        })
}

/// FNV-1a: a short content version label (the kernel hashes for real).
fn fnv(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3)
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn ingest_walks_text_files_and_parses_settings() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("docs/.git")).unwrap();
        std::fs::write(dir.path().join("docs/a.md"), "# Alpha\nx").unwrap();
        std::fs::write(dir.path().join("docs/b.png"), "x").unwrap();
        std::fs::write(dir.path().join("docs/.git/c.md"), "x").unwrap();
        let mut files = Vec::new();
        walk(dir.path(), &mut files).unwrap();
        let ids: Vec<&str> = files.iter().map(|(_, id)| id.as_str()).collect();
        assert_eq!(ids, ["docs/a.md"]);
        assert_eq!(title_of("# Alpha\nx", Path::new("a.md")), "Alpha");
        assert_eq!(title_of("x", Path::new("dir/b.md")), "b.md");
        assert_eq!(
            parse_settings(&["space=ENG".into(), "max_age_days=90".into()]).unwrap(),
            json!({"space": "ENG", "max_age_days": 90})
        );
        assert!(parse_settings(&["nope".into()]).is_err());
    }
}
