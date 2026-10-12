//! `sun skill new` and `sun mcp new`: a new skill or MCP server
//! folder from the repo's `templates/skill` and `templates/mcp-server`,
//! embedded in the binary. The template's placeholder name is replaced, and
//! the result is checked the way the catalog checks its own entries.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use sun_catalog::{McpServer, McpTransport, check_skill};

const SKILL_MD: &str = include_str!("../../../templates/skill/SKILL.md");
const MCP_SERVER: &str = include_str!("../../../templates/mcp-server/server.mjs");

/// Writes `<parent>/<name>/SKILL.md` and returns the folder.
pub fn new_skill(name: &str, parent: &Path) -> Result<PathBuf> {
    let text = SKILL_MD.replace("my-skill", name);
    check_skill(name, &text)?;
    let dir = new_dir(parent, name)?;
    fs::write(dir.join("SKILL.md"), text).context("writing SKILL.md")?;
    Ok(dir)
}

/// Writes `<parent>/<name>/server.mjs` (executable) and returns it with the
/// registration `sun plugin mcp add` would create for it.
pub fn new_mcp_server(name: &str, parent: &Path) -> Result<(PathBuf, McpServer)> {
    let parent = std::path::absolute(parent).context("resolving the folder")?;
    let entry = parent.join(name).join("server.mjs");
    let server = McpServer {
        name: name.to_owned(),
        transport: McpTransport::Command {
            command: entry.to_string_lossy().into_owned(),
            args: Vec::new(),
            network: Vec::new(),
            secrets: Vec::new(),
        },
    };
    server.validate()?;
    new_dir(&parent, name)?;
    fs::write(&entry, MCP_SERVER.replace("my-mcp-server", name)).context("writing server.mjs")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&entry, fs::Permissions::from_mode(0o755))?;
    }
    Ok((entry, server))
}

/// Creates `<parent>/<name>`, refusing one that already exists.
fn new_dir(parent: &Path, name: &str) -> Result<PathBuf> {
    let dir = parent.join(name);
    if dir.exists() {
        bail!(
            "{} already exists; pick another name or folder",
            dir.display()
        );
    }
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    Ok(dir)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn skill_new_scaffold_validates() {
        let parent = tempfile::tempdir().unwrap();
        let dir = new_skill("release-notes", parent.path()).unwrap();
        let text = fs::read_to_string(dir.join("SKILL.md")).unwrap();
        check_skill("release-notes", &text).unwrap();
        assert!(text.contains("# release-notes"));
        assert!(new_skill("release-notes", parent.path()).is_err(), "exists");
        assert!(new_skill("Bad Name", parent.path()).is_err());
        assert!(!parent.path().join("Bad Name").exists());
    }

    #[test]
    fn mcp_new_scaffold_validates() {
        let parent = tempfile::tempdir().unwrap();
        let (entry, server) = new_mcp_server("issue-tracker", parent.path()).unwrap();
        server.validate().unwrap();
        let McpTransport::Command { command, .. } = &server.transport else {
            panic!("a command server")
        };
        assert_eq!(Path::new(command), entry);
        assert!(entry.is_absolute());
        let text = fs::read_to_string(&entry).unwrap();
        assert!(text.starts_with("#!/usr/bin/env node\n"));
        assert!(text.contains("name: \"issue-tracker\""));
        assert!(!text.contains("my-mcp-server"));
        assert!(new_mcp_server("issue-tracker", parent.path()).is_err());
        assert!(new_mcp_server("sun", parent.path()).is_err(), "reserved");
    }
}
