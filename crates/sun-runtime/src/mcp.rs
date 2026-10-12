//! The session's MCP config (`SessionSpec.mcp_config`), rendered into each
//! CLI's native form. The file is Claude Code's format, which Claude reads
//! as is: `{"mcpServers": {"<name>": {"command", "args", "env"}}}`.
//! Codex gets `-c mcp_servers.<name>.…` overrides, OpenCode ACP
//! `session/new` `mcpServers` entries.

use std::collections::BTreeMap;
use std::path::Path;

use serde_json::{Value, json};
use sun_core::{Error, Result};

/// One stdio MCP server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpEntry {
    /// Server name (`[A-Za-z0-9_-]+`, it becomes a TOML key for Codex).
    pub name: String,
    /// Program to run.
    pub command: String,
    /// Its arguments.
    pub args: Vec<String>,
    /// Extra env vars.
    pub env: BTreeMap<String, String>,
}

/// Reads the MCP config file the session points at.
pub fn read_mcp_config(path: &Path) -> Result<Vec<McpEntry>> {
    let invalid = |e: &str| Error::InvalidInput(format!("mcp config {}: {e}", path.display()));
    let text = std::fs::read_to_string(path).map_err(|e| invalid(&e.to_string()))?;
    let file: Value = serde_json::from_str(&text).map_err(|e| invalid(&e.to_string()))?;
    let bare = |s: &str| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    };
    let strings = |v: &Value| -> Option<Vec<String>> {
        v.as_array()?
            .iter()
            .map(|s| s.as_str().map(str::to_owned))
            .collect()
    };
    let servers = file["mcpServers"]
        .as_object()
        .ok_or_else(|| invalid("no mcpServers object"))?;
    servers
        .iter()
        .map(|(name, server)| {
            let bad = || invalid(&format!("bad server {name:?}"));
            let env = match server.get("env") {
                None => BTreeMap::new(),
                Some(env) => env
                    .as_object()
                    .ok_or_else(bad)?
                    .iter()
                    .map(|(k, v)| Some((k.clone(), v.as_str()?.to_owned())))
                    .collect::<Option<_>>()
                    .ok_or_else(bad)?,
            };
            if !bare(name) || !env.keys().all(|k: &String| bare(k)) {
                return Err(bad());
            }
            Ok(McpEntry {
                name: name.clone(),
                command: server["command"].as_str().ok_or_else(bad)?.to_owned(),
                args: match server.get("args") {
                    None => Vec::new(),
                    Some(args) => strings(args).ok_or_else(bad)?,
                },
                env,
            })
        })
        .collect()
}

impl McpEntry {
    /// Codex `-c key=value` overrides (values are TOML; JSON strings and
    /// string arrays are valid TOML).
    pub fn codex_args(&self) -> Vec<String> {
        let key = format!("mcp_servers.{}", self.name);
        let mut out = vec![
            "-c".into(),
            format!("{key}.command={}", json!(self.command)),
            "-c".into(),
            format!("{key}.args={}", json!(self.args)),
        ];
        for (name, value) in &self.env {
            out.push("-c".into());
            out.push(format!("{key}.env.{name}={}", json!(value)));
        }
        out
    }

    /// An ACP `McpServer` (stdio) for `session/new`.
    pub fn acp(&self) -> Value {
        let env: Vec<Value> = self
            .env
            .iter()
            .map(|(name, value)| json!({"name": name, "value": value}))
            .collect();
        json!({"name": self.name, "command": self.command, "args": self.args, "env": env})
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn renders_codex_and_acp_forms_and_refuses_bad_names() {
        let dir = std::env::temp_dir().join(format!("sun-mcp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("mcp.json");
        std::fs::write(
            &path,
            r#"{"mcpServers":{"sun":{"command":"/bin/sun","args":["mcp","--token","t\"x"],"env":{"A":"1"}}}}"#,
        )
        .unwrap();
        let servers = read_mcp_config(&path).unwrap();
        assert_eq!(
            servers[0].codex_args(),
            [
                "-c",
                r#"mcp_servers.sun.command="/bin/sun""#,
                "-c",
                r#"mcp_servers.sun.args=["mcp","--token","t\"x"]"#,
                "-c",
                r#"mcp_servers.sun.env.A="1""#,
            ]
        );
        assert_eq!(
            servers[0].acp(),
            json!({"name": "sun", "command": "/bin/sun", "args": ["mcp", "--token", "t\"x"],
                   "env": [{"name": "A", "value": "1"}]})
        );
        std::fs::write(&path, r#"{"mcpServers":{"a.b":{"command":"x"}}}"#).unwrap();
        assert!(
            read_mcp_config(&path).is_err(),
            "dotted names would nest TOML keys"
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
