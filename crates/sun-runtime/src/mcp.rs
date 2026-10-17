//! The session's MCP config (`SessionSpec.mcp_config`), rendered into each
//! CLI's native form. The file is Claude Code's format, which Claude reads
//! as is: `{"mcpServers": {"<name>": {"command", "args", "env"}}}`.
//! Codex gets `-c mcp_servers.<name>.…` overrides, OpenCode ACP
//! `session/new` `mcpServers` entries.

use std::collections::BTreeMap;
use std::path::Path;

use serde_json::{Value, json};
use sun_core::{Error, Result, SessionSpec};

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

/// Puts the session MCP servers' env (the `SUN_MCP_TOKEN`) in `spec.env`
/// for Codex, which forwards it to them by name ([`McpEntry::codex_args`]),
/// so no value is ever on argv, where every local user can read it.
/// Idempotent; a no-op without an MCP config or for other adapters (Claude
/// Code reads the file, OpenCode gets ACP entries). The host calls it before
/// an exec env is prepared, so the vars reach a container or VM too.
pub fn native_env(spec: &mut SessionSpec, adapter: &str) -> Result<()> {
    let Some(path) = spec
        .mcp_config
        .clone()
        .filter(|_| adapter == crate::codex::ID)
    else {
        return Ok(());
    };
    for server in read_mcp_config(&path)? {
        for (name, value) in server.env {
            match spec.env.get(&name) {
                Some(have) if *have != value => {
                    return Err(Error::InvalidInput(format!(
                        "mcp config {}: env {name} of server {:?} clashes with another value",
                        path.display(),
                        server.name
                    )));
                }
                Some(_) => {}
                None => {
                    spec.env.insert(name, value);
                }
            }
        }
    }
    Ok(())
}

impl McpEntry {
    /// Codex `-c key=value` overrides (values are TOML; JSON strings and
    /// string arrays are valid TOML). Env goes by name only (`env_vars`):
    /// Codex forwards those from its own env, which [`native_env`] fills.
    pub fn codex_args(&self) -> Vec<String> {
        let key = format!("mcp_servers.{}", self.name);
        let mut out = vec![
            "-c".into(),
            format!("{key}.command={}", json!(self.command)),
            "-c".into(),
            format!("{key}.args={}", json!(self.args)),
        ];
        if !self.env.is_empty() {
            let names: Vec<&String> = self.env.keys().collect();
            out.push("-c".into());
            out.push(format!("{key}.env_vars={}", json!(names)));
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
                r#"mcp_servers.sun.env_vars=["A"]"#,
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

    #[test]
    fn native_env_lifts_mcp_env_for_codex_only() {
        let dir = std::env::temp_dir().join(format!("sun-mcp-env-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("mcp.json");
        let write = |docs: &str| {
            let config = json!({"mcpServers": {
                "sun": {"command": "sun", "env": {"SUN_MCP_TOKEN": "t"}},
                "docs": {"command": "sun", "env": {"SUN_MCP_TOKEN": docs}},
            }});
            std::fs::write(&path, config.to_string()).unwrap();
        };
        write("t");
        let mut spec: SessionSpec = serde_json::from_value(json!({
            "session_id": sun_core::SessionId::generate(),
            "run_id": sun_core::RunId::generate(),
            "step_key": "implement",
            "worktree": dir,
            "envelope": "",
            "mcp_config": path,
            "env": {},
        }))
        .unwrap();
        native_env(&mut spec, "claude-code").unwrap();
        assert!(spec.env.is_empty(), "Claude Code reads the file itself");
        native_env(&mut spec, "codex").unwrap();
        native_env(&mut spec, "codex").unwrap();
        assert_eq!(spec.env["SUN_MCP_TOKEN"], "t");
        write("other");
        assert!(
            native_env(&mut spec, "codex").is_err(),
            "one name can't forward two values"
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
