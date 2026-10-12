//! Loads and checks `mod.yaml`. The shape and kind rules are owned
//! by `sun-mod-sdk`; this adds YAML parsing and the API compatibility check.

use std::path::Path;

use semver::{Version, VersionReq};
use sun_core::{Error, Result};
use sun_mod_sdk::{API_VERSION, Manifest};

/// File name of a mod's manifest inside its folder.
pub const MANIFEST_FILE: &str = "mod.yaml";

/// Reads `dir/mod.yaml` and checks it.
pub fn load_manifest(dir: &Path) -> Result<Manifest> {
    let path = dir.join(MANIFEST_FILE);
    let text = std::fs::read_to_string(&path)
        .map_err(|e| Error::InvalidInput(format!("{}: {e}", path.display())))?;
    parse_manifest(&text)
}

/// Parses `mod.yaml` text and checks it ([`check_manifest`]).
pub fn parse_manifest(yaml: &str) -> Result<Manifest> {
    let manifest: Manifest = serde_saphyr::from_str(yaml)
        .map_err(|e| Error::InvalidInput(format!("invalid mod.yaml: {e}")))?;
    check_manifest(&manifest)?;
    Ok(manifest)
}

/// Kind rules, semver `version`, and an `api` range that includes the API
/// this Sun speaks. The error names both so the user knows what would work.
pub fn check_manifest(m: &Manifest) -> Result<()> {
    m.validate()
        .map_err(|e| Error::InvalidInput(e.to_string()))?;
    Version::parse(&m.version).map_err(|e| {
        Error::InvalidInput(format!("mod {}: version {:?}: {e}", m.name, m.version))
    })?;
    let range = api_range(&m.api)
        .map_err(|e| Error::InvalidInput(format!("mod {}: api {:?}: {e}", m.name, m.api)))?;
    let ours = Version::parse(API_VERSION).map_err(|e| Error::Protocol(e.to_string()))?;
    if !range.matches(&ours) {
        return Err(Error::InvalidInput(format!(
            "mod {} supports mod API {}, but this Sun speaks {API_VERSION}: \
             install a version of {} whose api range includes {API_VERSION}",
            m.name, m.api, m.name
        )));
    }
    Ok(())
}

/// Accepts the space-separated form (`>=1.0 <2.0`) as well as semver's
/// comma form (`>=1.0, <2.0`).
fn api_range(text: &str) -> std::result::Result<VersionReq, semver::Error> {
    let mut parts: Vec<String> = Vec::new();
    let mut operator = String::new();
    for token in text.split([' ', ',']).filter(|t| !t.is_empty()) {
        if token.chars().all(|c| "<>=~^".contains(c)) {
            operator.push_str(token); // `>= 1.0`: glue the operator to its version
        } else {
            parts.push(format!("{operator}{token}"));
            operator.clear();
        }
    }
    VersionReq::parse(&parts.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The kernel's copy of the reserved action names (sun-core, which
    /// sun-runtime and sun-host use) matches the SDK's, which mods use.
    #[test]
    fn kernel_and_sdk_agree_on_reserved_actions() {
        assert_eq!(
            sun_core::runtime_action::ALL,
            sun_mod_sdk::runtime_action::ALL
        );
        assert_eq!(
            sun_core::exec_env_action::ALL,
            sun_mod_sdk::exec_env_action::ALL
        );
    }

    const GITHUB: &str = r#"
name: github
kind: plugin                  # plugin | extension
version: 0.3.1
api: ">=1.0 <2.0"
entry: { command: "./github-mod" }
provides:
  actions: [open_pr, comment, ci_status]
  triggers: [issue_labeled, pr_review]
  mcp_tools: true
permissions:
  network: [api.github.com]
  secrets: [GITHUB_TOKEN]
"#;

    #[test]
    fn parses_the_trd_example() {
        let m = parse_manifest(GITHUB).unwrap();
        assert_eq!(m.name, "github");
        assert_eq!(m.provides.triggers, ["issue_labeled", "pr_review"]);
        assert!(m.is_callable("open_pr"));
    }

    #[test]
    fn rejects_kind_violations_unknown_fields_and_bad_versions() {
        let ext = GITHUB.replace("kind: plugin", "kind: extension");
        assert!(
            parse_manifest(&ext)
                .unwrap_err()
                .to_string()
                .contains("plugin-only")
        );

        let devices = GITHUB.replace("  secrets:", "  devices: [kvm]\n  secrets:");
        let err = parse_manifest(&devices).unwrap_err().to_string();
        assert!(err.contains("devices"), "{err}");

        let unknown = format!("{GITHUB}color: red\n");
        assert!(parse_manifest(&unknown).is_err());

        let version = GITHUB.replace("version: 0.3.1", "version: three");
        assert!(parse_manifest(&version).is_err());
    }

    #[test]
    fn rejects_an_incompatible_api_and_names_the_versions() {
        let future = GITHUB.replace(">=1.0 <2.0", ">= 2.0, <3.0");
        let err = parse_manifest(&future).unwrap_err().to_string();
        assert!(
            err.contains(">= 2.0, <3.0") && err.contains(API_VERSION),
            "{err}"
        );
        assert!(api_range("^1").unwrap().matches(&Version::new(1, 0, 0)));
    }
}
