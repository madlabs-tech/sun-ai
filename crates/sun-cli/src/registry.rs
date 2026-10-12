//! The mod registry: every plugin and extension `sun` can install by name,
//! embedded from `catalog/mods.json` (registering one = a PR to that file),
//! and the download of a listed version's GitHub Release asset for this
//! platform.

use std::sync::LazyLock;

use serde::Deserialize;
use sun_core::model::ModKind;
use sun_core::{AvailableMod, Error, Result};

use crate::installer::{MAX_PACKAGE, http};

/// One mod of `catalog/mods.json`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    /// Mod name; also the asset name `mod-<name>-<target>.tar.gz`.
    pub name: String,
    /// Plugin or extension.
    pub kind: ModKind,
    /// The pinned version an install gets (release tag `v<version>`).
    pub version: String,
    /// What it does, one sentence.
    pub description: String,
    /// Its GitHub repo, `https://github.com/<owner>/<repo>`.
    pub repo: String,
    /// `official` (Madlabs) or `community` (not reviewed by Madlabs).
    pub tier: String,
    /// What it asks for (`kind:value`, as `sun_mod::permission_list`).
    pub permissions: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    mods: Vec<Entry>,
}

static ENTRIES: LazyLock<Vec<Entry>> = LazyLock::new(|| {
    match serde_json::from_str::<File>(include_str!("../../../catalog/mods.json")) {
        Ok(file) => file.mods,
        Err(e) => {
            tracing::error!("catalog/mods.json: {e}");
            Vec::new()
        }
    }
});

/// Every listed mod.
pub fn entries() -> &'static [Entry] {
    &ENTRIES
}

/// The listed mod called `name`.
pub fn find(name: &str) -> Option<&'static Entry> {
    entries().iter().find(|e| e.name == name)
}

/// How `GET /mods` lists `entry`.
pub fn listed(entry: &Entry) -> AvailableMod {
    AvailableMod {
        name: entry.name.clone(),
        kind: entry.kind,
        version: entry.version.clone(),
        permissions: entry.permissions.clone(),
        signed: false,
        description: entry.description.clone(),
        repo: entry.repo.clone(),
        tier: entry.tier.clone(),
    }
}

/// The release target this `sun` was built for (what cargo-dist names assets).
pub const TARGET: &str = if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
    "aarch64-apple-darwin"
} else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
    "x86_64-unknown-linux-musl"
} else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
    "aarch64-unknown-linux-musl"
} else {
    "unsupported"
};

/// The release asset of `entry`'s version for [`TARGET`].
pub fn asset_url(entry: &Entry) -> String {
    format!(
        "{}/releases/download/v{}/mod-{}-{TARGET}.tar.gz",
        entry.repo, entry.version, entry.name
    )
}

/// Downloads [`asset_url`] (a listed mod with no asset for this platform is
/// `Unavailable`).
pub fn download(entry: &Entry) -> Result<Vec<u8>> {
    let (name, version, repo) = (&entry.name, &entry.version, &entry.repo);
    let url = asset_url(entry);
    let mut response = http()
        .get(&url)
        .call()
        .map_err(|e| Error::Unavailable(format!("{name} {version}: cannot reach {url}: {e}")))?;
    let status = response.status().as_u16();
    if !response.status().is_success() {
        return Err(Error::Unavailable(format!(
            "{name} {version}: no release asset for {TARGET} at {url} (HTTP {status}). Ask the maintainers for a build at {repo}/releases, or build it yourself and run `sun plugin add <folder>`."
        )));
    }
    response
        .body_mut()
        .with_config()
        .limit(MAX_PACKAGE)
        .read_to_vec()
        .map_err(|e| Error::Unavailable(format!("downloading {name} {version}: {e}")))
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_embedded_registry_is_well_formed() {
        let entries = super::entries();
        assert!(!entries.is_empty());
        let mut names: Vec<_> = entries.iter().map(|e| e.name.as_str()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), entries.len(), "duplicate names");
        for e in entries {
            assert!(
                ["official", "community"].contains(&e.tier.as_str()),
                "{e:?}"
            );
            assert!(e.repo.starts_with("https://github.com/"), "{e:?}");
            assert!(e.permissions.iter().all(|p| p.contains(':')), "{e:?}");
        }
        let github = super::find("github").unwrap();
        assert!(super::asset_url(github).ends_with(&format!(
            "/sun-plugin-github/releases/download/v{}/mod-github-{}.tar.gz",
            github.version,
            super::TARGET
        )));
    }
}
