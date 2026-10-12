//! Local dev mode (`sun mod dev`): watch a mod's folder and
//! hot-reload it into a [`ModHost`] on every change.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use sun_core::Result;

use crate::{ModHost, Sandbox, load_manifest};

/// Build output and dependencies: changes there don't trigger a reload.
const IGNORED: [&str; 3] = ["target", "node_modules", ".git"];

/// Watches `dir` (polling every `poll`) and re-adds the mod after any file
/// changes, which stops the old process; the next call starts the new one.
/// With `data_dir` the mod runs sandboxed, as installed mods do. Each reload
/// attempt is reported to `on_reload` (the mod name, or why `mod.yaml` was
/// rejected; the previous version then keeps running). Runs until dropped.
pub async fn watch_dev(
    host: &ModHost,
    dir: &Path,
    data_dir: Option<PathBuf>,
    poll: Duration,
    mut on_reload: impl FnMut(Result<String>),
) {
    let mut seen = None;
    loop {
        let now = fingerprint(dir);
        if seen != Some(now) {
            seen = Some(now);
            on_reload(reload(host, dir, data_dir.as_deref()).await);
        }
        tokio::time::sleep(poll).await;
    }
}

async fn reload(host: &ModHost, dir: &Path, data_dir: Option<&Path>) -> Result<String> {
    let manifest = load_manifest(dir)?;
    let name = manifest.name.clone();
    match data_dir {
        Some(data) => {
            let sandbox = Sandbox::for_mod(&manifest, data, vec![])?;
            host.add_sandboxed(manifest, dir.to_path_buf(), sandbox)
                .await?;
        }
        None => host.add(manifest, dir.to_path_buf()).await?,
    }
    Ok(name)
}

/// Newest modification time and file count under `dir`.
fn fingerprint(dir: &Path) -> (Option<SystemTime>, usize) {
    let mut newest = None;
    let mut count = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_dir() {
                if !IGNORED.iter().any(|i| entry.file_name() == *i) {
                    stack.push(entry.path());
                }
                continue;
            }
            count += 1;
            newest = newest.max(meta.modified().ok());
        }
    }
    (newest, count)
}
