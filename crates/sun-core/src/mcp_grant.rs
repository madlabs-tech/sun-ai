//! MCP grants: which registered MCP servers, and which of their
//! tools, a task's agent sessions may use. Plain data here so the engine can
//! render a session's MCP config; `sun-mod` connects and enforces them.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{Error, Result};

/// One MCP server enabled for a task or workflow.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpGrant {
    /// Registered server name.
    pub server: String,
    /// Tools enabled by name. `None` (the default) enables only the tools
    /// the server marks read-only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<String>>,
}

impl McpGrant {
    /// Whether this grant lets an agent see and call tool `name`, which the
    /// server marks `read_only` (`readOnlyHint: true`) or not.
    pub fn allows(&self, name: &str, read_only: bool) -> bool {
        match &self.tools {
            None => read_only,
            Some(names) => names.iter().any(|n| n == name),
        }
    }
}

/// The session's MCP config (`{"mcpServers": {..}}`, the shape Claude Code's
/// `--mcp-config` reads): one stdio entry per grant, each running
/// `command args.. <grant json>`, which must serve the filtering proxy
/// (`sun_mod::serve_mcp_proxy`) on its stdio. Add Sun's own `sun` server to
/// the result separately.
pub fn render_mcp_config(grants: &[McpGrant], command: &str, args: &[&str]) -> Result<Value> {
    let mut servers = BTreeMap::new();
    for grant in grants {
        if servers.contains_key(&grant.server) {
            return Err(Error::InvalidInput(format!(
                "mcp server {} granted twice",
                grant.server
            )));
        }
        let grant_json =
            serde_json::to_string(grant).map_err(|e| Error::InvalidInput(e.to_string()))?;
        let mut argv: Vec<String> = args.iter().map(|a| a.to_string()).collect();
        argv.push(grant_json);
        servers.insert(
            grant.server.clone(),
            json!({ "type": "stdio", "command": command, "args": argv }),
        );
    }
    Ok(json!({ "mcpServers": servers }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_grant_defaults_to_read_only_tools() {
        let grant: McpGrant =
            serde_json::from_value(json!({"server": "docs"})).unwrap_or_else(|e| panic!("{e}"));
        assert!(grant.allows("read", true));
        assert!(!grant.allows("write", false));
        let grant = McpGrant {
            server: "docs".into(),
            tools: Some(vec!["write".into()]),
        };
        assert!(grant.allows("write", false));
        assert!(!grant.allows("read", true));
    }

    #[test]
    fn mcp_config_runs_the_proxy_per_grant() {
        let grants = [McpGrant {
            server: "docs".into(),
            tools: None,
        }];
        let config = render_mcp_config(&grants, "/bin/sun", &["mcp", "proxy"])
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(
            config,
            json!({"mcpServers": {"docs": {"type": "stdio", "command": "/bin/sun",
                "args": ["mcp", "proxy", "{\"server\":\"docs\"}"]}}})
        );
        let twice = [grants[0].clone(), grants[0].clone()];
        assert!(render_mcp_config(&twice, "sun", &[]).is_err());
    }
}
