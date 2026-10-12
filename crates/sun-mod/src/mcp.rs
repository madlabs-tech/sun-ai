//! MCP connectors: an existing MCP server plugged in
//! by URL (streamable HTTP) or command (stdio), without writing a mod.
//!
//! - [`McpServer`] is the registration. Credentials are secret *names*; the
//!   values come from the secret store when connecting.
//! - [`McpClient`] connects (rmcp, **Adapter** over both transports) and lists
//!   tools. A command server always runs inside the [`Sandbox`]: it is
//!   third-party code.
//! - [`McpGrant`] (owned by `sun-core`) is the per-task (or per-workflow)
//!   enablement. Least privilege: no grant, no server; a grant without a
//!   tool list enables only tools the server marks read-only.
//! - [`serve_mcp_proxy`] is what an agent session actually talks to: a
//!   filtering **Proxy** that shows only the granted tools and refuses the
//!   rest. `sun_core::render_mcp_config` points the runtime's MCP config at
//!   it, so no secret or upstream URL lands in the session's config file.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
    ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
    ToolAnnotations,
};
use rmcp::service::{RequestContext, RunningService};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::{ErrorData, RoleClient, RoleServer, ServerHandler, ServiceExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sun_core::{Error, McpGrant, ModCall, ModLink, Result};
use sun_mod_sdk::{
    API_VERSION, Entry, Manifest, McpCallParams, McpToolSpec, McpTools, ModKind, Permissions,
    Provides, mcp_action,
};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::process::Child;

use crate::sandbox::Sandbox;
use crate::sandbox::proxy::{self, Proxy};

/// How long connecting (handshake) or listing tools may take.
const TIMEOUT: Duration = Duration::from_secs(30);

/// A registered MCP server. Validate with [`McpServer::validate`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpServer {
    /// Unique name, `[a-z][a-z0-9_-]{0,63}`; `sun` is reserved.
    pub name: String,
    /// Where it lives.
    pub transport: McpTransport,
}

/// How to reach an MCP server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum McpTransport {
    /// Streamable HTTP at `url` (`http://` or `https://`).
    Url {
        /// Endpoint URL.
        url: String,
        /// Secret whose value is sent as `Authorization: Bearer <value>`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bearer_secret: Option<String>,
    },
    /// A stdio command, run sandboxed. A bare name is looked up on `PATH`.
    Command {
        /// Executable: a bare name (`npx`) or an absolute path.
        command: String,
        /// Arguments.
        #[serde(default)]
        args: Vec<String>,
        /// Hosts it may reach through the sandbox proxy: `host` or `host:port`.
        #[serde(default)]
        network: Vec<String>,
        /// Secrets passed to it as env vars of the same name.
        #[serde(default)]
        secrets: Vec<String>,
    },
}

impl McpServer {
    /// Checks the name, the URL or command, and every permission entry.
    pub fn validate(&self) -> Result<()> {
        let manifest = self.manifest();
        manifest
            .validate()
            .map_err(|e| Error::InvalidInput(format!("mcp server: {}", e.0)))?;
        if self.name == "sun" {
            return Err(Error::InvalidInput(
                "mcp server name `sun` is reserved for Sun's own tools".into(),
            ));
        }
        match &self.transport {
            McpTransport::Url { url, .. } => {
                let ok = ["http://", "https://"]
                    .iter()
                    .any(|scheme| url.len() > scheme.len() && url.starts_with(scheme))
                    && !url.contains(char::is_whitespace);
                if !ok {
                    return Err(Error::InvalidInput(format!(
                        "mcp server {}: url must be http:// or https://",
                        self.name
                    )));
                }
            }
            McpTransport::Command {
                command, network, ..
            } => {
                if command.contains('/') && !Path::new(command).is_absolute() {
                    return Err(Error::InvalidInput(format!(
                        "mcp server {}: command must be a bare name or an absolute path",
                        self.name
                    )));
                }
                for host in network {
                    crate::sandbox::check_host(host)?;
                }
            }
        }
        Ok(())
    }

    /// Secret names this server needs from the secret store.
    pub fn secret_names(&self) -> Vec<String> {
        self.manifest().permissions.secrets
    }

    /// The equivalent plugin manifest, so a command server gets the same name
    /// rules and [`Sandbox`] policy as any mod.
    fn manifest(&self) -> Manifest {
        let (entry, permissions) = match &self.transport {
            McpTransport::Url { bearer_secret, .. } => (
                Entry {
                    command: "-".into(),
                    args: Vec::new(),
                },
                Permissions {
                    secrets: bearer_secret.iter().cloned().collect(),
                    ..Permissions::default()
                },
            ),
            McpTransport::Command {
                command,
                args,
                network,
                secrets,
            } => (
                Entry {
                    command: command.clone(),
                    args: args.clone(),
                },
                Permissions {
                    network: network.clone(),
                    secrets: secrets.clone(),
                    ..Permissions::default()
                },
            ),
        };
        Manifest {
            name: self.name.clone(),
            kind: ModKind::Plugin,
            version: "0.0.0".into(),
            api: format!("={API_VERSION}"),
            entry,
            provides: Provides {
                mcp_tools: true,
                ..Provides::default()
            },
            permissions,
        }
    }
}

/// One tool an MCP server offers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpTool {
    /// Tool name, as the server calls it.
    pub name: String,
    /// What it does (may be empty).
    pub description: String,
    /// The server marks it read-only (`readOnlyHint: true`). Unmarked tools
    /// are not read-only (the MCP default).
    pub read_only: bool,
}

impl From<&Tool> for McpTool {
    fn from(tool: &Tool) -> Self {
        Self {
            name: tool.name.to_string(),
            description: tool.description.as_deref().unwrap_or_default().to_string(),
            read_only: tool
                .annotations
                .as_ref()
                .and_then(|a| a.read_only_hint)
                .unwrap_or(false),
        }
    }
}

/// A connected MCP server, or a mod serving MCP tools (**Adapter** over
/// both). Dropping it closes the connection and kills a command server.
pub struct McpClient {
    upstream: Upstream,
}

enum Upstream {
    Server {
        service: Box<RunningService<RoleClient, ()>>,
        _child: Option<Child>,
        /// A command server's only way out; stops with it.
        _proxy: Option<Proxy>,
    },
    /// A plugin with `provides.mcp_tools`: its tools are the
    /// reserved `call` actions [`mcp_action`], so it runs in its own sandbox
    /// and can emit trigger events too.
    Mod {
        link: Arc<dyn ModLink>,
        name: String,
    },
}

impl McpClient {
    /// Connects to `server`. `secrets` are the values of
    /// [`McpServer::secret_names`]; a command server runs sandboxed with
    /// `data_dir` as its `HOME` and working dir.
    pub async fn connect(
        server: &McpServer,
        data_dir: &Path,
        secrets: Vec<(String, String)>,
    ) -> Result<Self> {
        server.validate()?;
        let name = &server.name;
        let failed = |e: String| Error::Unavailable(format!("mcp server {name}: {e}"));
        let connect = async {
            match &server.transport {
                McpTransport::Url { url, bearer_secret } => {
                    let mut config = StreamableHttpClientTransportConfig::with_uri(url.as_str());
                    if let Some(secret) = bearer_secret {
                        let value = secrets.iter().find(|(k, _)| k == secret).ok_or_else(|| {
                            Error::InvalidInput(format!(
                                "mcp server {name}: secret {secret} missing"
                            ))
                        })?;
                        config = config.auth_header(value.1.clone());
                    }
                    // reqwest needs a process-wide rustls provider; a second
                    // install is a harmless no-op error.
                    let _ = rustls::crypto::ring::default_provider().install_default();
                    let transport = StreamableHttpClientTransport::from_config(config);
                    let service = ().serve(transport).await.map_err(|e| failed(e.to_string()))?;
                    Ok(Self::server(service, None, None))
                }
                McpTransport::Command { command, args, .. } => {
                    let mut sandbox = Sandbox::for_mod(&server.manifest(), data_dir, secrets)?;
                    // Launchers (npx, uvx) start interpreters: any program,
                    // still in the file and network jail.
                    sandbox.processes = None;
                    let proxy = proxy::start(sandbox.network.clone())
                        .map_err(|e| failed(format!("proxy: {e}")))?;
                    let program = PathBuf::from(command);
                    // An absolute command needs its own folder readable.
                    let read_dir = match program.parent() {
                        Some(dir) if program.is_absolute() => {
                            check_not_broad(name, dir)?;
                            dir.to_path_buf()
                        }
                        _ => data_dir.to_path_buf(),
                    };
                    let mut cmd = sandbox.command(program, args, &read_dir, proxy.port)?;
                    cmd.current_dir(data_dir)
                        .stdin(Stdio::piped())
                        .stdout(Stdio::piped())
                        // ponytail: server logs go to our stderr unredacted, as for mods.
                        .stderr(Stdio::inherit())
                        .kill_on_drop(true);
                    if let Some(path) = std::env::var_os("PATH") {
                        cmd.env("PATH", path);
                    }
                    let mut child = cmd
                        .spawn()
                        .map_err(|e| failed(format!("cannot start: {e}")))?;
                    let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take())
                    else {
                        return Err(failed("no stdio".into()));
                    };
                    let service =
                        ().serve((stdout, stdin))
                            .await
                            .map_err(|e| failed(e.to_string()))?;
                    Ok(Self::server(service, Some(child), Some(proxy)))
                }
            }
        };
        tokio::time::timeout(TIMEOUT, connect)
            .await
            .map_err(|_| failed(format!("no handshake within {TIMEOUT:?}")))?
    }

    fn server(
        service: RunningService<RoleClient, ()>,
        child: Option<Child>,
        proxy: Option<Proxy>,
    ) -> Self {
        Self {
            upstream: Upstream::Server {
                service: Box::new(service),
                _child: child,
                _proxy: proxy,
            },
        }
    }

    /// The MCP tools of installed mod `name` (a plugin with
    /// `provides.mcp_tools`), reached through `link`. Nothing is called yet;
    /// a mod without `mcp_tools` fails on the first listing.
    pub fn for_mod(link: Arc<dyn ModLink>, name: &str) -> Self {
        Self {
            upstream: Upstream::Mod {
                link,
                name: name.to_owned(),
            },
        }
    }

    /// Every tool the server offers.
    pub async fn tools(&self) -> Result<Vec<McpTool>> {
        Ok(self.raw_tools().await?.iter().map(McpTool::from).collect())
    }

    async fn raw_tools(&self) -> Result<Vec<Tool>> {
        let (link, name) = match &self.upstream {
            Upstream::Server { service, .. } => {
                let listed = tokio::time::timeout(TIMEOUT, service.list_all_tools()).await;
                return match listed {
                    Ok(Ok(tools)) => Ok(tools),
                    Ok(Err(e)) => Err(Error::Unavailable(format!("mcp tools/list: {e}"))),
                    Err(_) => Err(Error::Unavailable(format!(
                        "mcp tools/list: no answer within {TIMEOUT:?}"
                    ))),
                };
            }
            Upstream::Mod { link, name } => (link, name),
        };
        let listed = link
            .call(mod_call(name, mcp_action::TOOLS, Value::Null))
            .await?;
        let listed: McpTools = serde_json::from_value(listed)
            .map_err(|e| Error::Protocol(format!("mod {name}: mcp_tools: {e}")))?;
        listed
            .tools
            .into_iter()
            .map(|t| mod_tool(name, t))
            .collect()
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        server: &str,
    ) -> std::result::Result<CallToolResponse, ErrorData> {
        let (link, name) = match &self.upstream {
            Upstream::Server { service, .. } => {
                // ponytail: no per-call timeout (tools may run long); the agent's own
                // timeout applies. Add one when a hung server stalls a session.
                return service
                    .call_tool(request)
                    .await
                    .map(Into::into)
                    .map_err(|e| ErrorData::internal_error(format!("{server}: {e}"), None));
            }
            Upstream::Mod { link, name } => (link, name),
        };
        let params = McpCallParams {
            name: request.name.to_string(),
            arguments: request.arguments.map(Value::Object).unwrap_or_default(),
        };
        let params = serde_json::to_value(params)
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        let result = match link.call(mod_call(name, mcp_action::CALL, params)).await {
            Ok(Value::String(text)) => CallToolResult::success(vec![ContentBlock::text(text)]),
            Ok(other) => CallToolResult::success(vec![ContentBlock::text(other.to_string())]),
            // A tool failure is the agent's to read, not a protocol error.
            Err(e) => CallToolResult::error(vec![ContentBlock::text(e.to_string())]),
        };
        Ok(result.into())
    }
}

fn mod_call(name: &str, action: &str, params: Value) -> ModCall {
    ModCall {
        mod_name: name.to_owned(),
        method: action.to_owned(),
        params,
        idem_key: None,
    }
}

/// A mod's tool spec as an MCP tool; the input schema must be an object.
fn mod_tool(mod_name: &str, spec: McpToolSpec) -> Result<Tool> {
    let schema = match spec.input_schema {
        Value::Null => serde_json::Map::from_iter([("type".to_owned(), "object".into())]),
        Value::Object(schema) => schema,
        _ => {
            return Err(Error::Protocol(format!(
                "mod {mod_name}: tool {} input_schema must be an object",
                spec.name
            )));
        }
    };
    Ok(Tool::new(spec.name, spec.description, schema)
        .with_annotations(ToolAnnotations::new().read_only(spec.read_only)))
}

/// Refuses an absolute command sitting directly in `/` or the home dir:
/// its folder becomes readable inside the sandbox.
fn check_not_broad(name: &str, dir: &Path) -> Result<()> {
    let home = std::env::home_dir();
    if dir.parent().is_none() || home.as_deref() == Some(dir) {
        return Err(Error::InvalidInput(format!(
            "mcp server {name}: put the command in its own folder, not {}",
            dir.display()
        )));
    }
    Ok(())
}

/// Serves the tools `grant` enables from `upstream` on `reader`/`writer`
/// (the agent's stdio) until the agent disconnects. The tool list is fixed
/// at start, so a server can't widen the grant by re-marking tools later.
pub async fn serve_mcp_proxy<R, W>(
    upstream: McpClient,
    grant: McpGrant,
    reader: R,
    writer: W,
) -> Result<()>
where
    R: AsyncRead + Send + Unpin + 'static,
    W: AsyncWrite + Send + Unpin + 'static,
{
    let tools = upstream
        .raw_tools()
        .await?
        .into_iter()
        .filter(|tool| {
            let tool = McpTool::from(tool);
            grant.allows(&tool.name, tool.read_only)
        })
        .collect();
    let filter = Filter {
        upstream,
        name: grant.server,
        tools,
    };
    let running = filter
        .serve((reader, writer))
        .await
        .map_err(|e| Error::Protocol(format!("mcp proxy: {e}")))?;
    running
        .waiting()
        .await
        .map_err(|e| Error::Protocol(format!("mcp proxy: {e}")))?;
    Ok(())
}

/// The session-facing side of [`serve_mcp_proxy`].
struct Filter {
    upstream: McpClient,
    name: String,
    tools: Vec<Tool>,
}

impl ServerHandler for Filter {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build()).with_server_info(
            Implementation::new(self.name.clone(), env!("CARGO_PKG_VERSION")),
        )
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> std::result::Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult::with_all_items(self.tools.clone()))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> std::result::Result<CallToolResponse, ErrorData> {
        if !self.tools.iter().any(|tool| tool.name == request.name) {
            return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                "tool {} is not enabled for this task",
                request.name
            ))])
            .into());
        }
        self.upstream.call_tool(request, &self.name).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn server(json: Value) -> Result<McpServer> {
        let server: McpServer =
            serde_json::from_value(json).map_err(|e| Error::InvalidInput(e.to_string()))?;
        server.validate().map(|()| server)
    }

    #[test]
    fn mcp_server_registration_is_validated() {
        let ok = server(
            json!({"name": "docs", "transport": {"type": "url", "url": "https://mcp.example.com/mcp", "bearer_secret": "DOCS_TOKEN"}}),
        );
        assert_eq!(
            ok.map(|s| s.secret_names()).ok(),
            Some(vec!["DOCS_TOKEN".to_string()])
        );
        let ok = server(
            json!({"name": "fs", "transport": {"type": "command", "command": "npx", "args": ["-y", "x"], "network": ["registry.npmjs.org"]}}),
        );
        assert!(ok.is_ok());
        for bad in [
            json!({"name": "sun", "transport": {"type": "url", "url": "https://x"}}),
            json!({"name": "Bad", "transport": {"type": "url", "url": "https://x"}}),
            json!({"name": "x", "transport": {"type": "url", "url": "file:///etc/passwd"}}),
            json!({"name": "x", "transport": {"type": "url", "url": "https://x", "extra": 1}}),
            json!({"name": "x", "transport": {"type": "command", "command": "./rel"}}),
            json!({"name": "x", "transport": {"type": "command", "command": "a", "network": ["*.x.com"]}}),
        ] {
            assert!(server(bad.clone()).is_err(), "{bad}");
        }
    }
}
