//! `sun mcp`: the stdio shim an agent CLI runs as its `sun` MCP server.
//! It dials the kernel's `GET /mcp` WebSocket with the session
//! token from `SUN_MCP_TOKEN` and relays JSON-RPC lines both ways: stdin →
//! kernel, kernel → stdout. The engine writes the config that runs it
//! (`.sun/context/mcp.json`).
//!
//! Also `sun mcp relay` (a session's granted MCP server, through the
//! kernel's `GET /mcp/{server}`), `sun mcp proxy` (the same served locally
//! from the data dir, for debugging) and `sun plugin mcp` (the registry).

use std::path::Path;

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use futures_util::{SinkExt, StreamExt};
use sun_catalog::{Catalog, McpServer, McpTool, McpTransport};
use sun_core::McpGrant;
use sun_store::Store;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::header::AUTHORIZATION;

/// `kernel` (an `http(s)://` or `ws(s)://` base URL) as a WebSocket URL
/// ending in `path`.
pub fn ws_url(kernel: &str, path: &str) -> Result<String> {
    let kernel = kernel.trim_end_matches('/');
    let url = if let Some(rest) = kernel.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = kernel.strip_prefix("http://") {
        format!("ws://{rest}")
    } else if kernel.starts_with("ws://") || kernel.starts_with("wss://") {
        kernel.to_owned()
    } else {
        bail!("--kernel must be an http(s):// URL, got {kernel}");
    };
    Ok(match url.ends_with(path) {
        true => url,
        false => format!("{url}{path}"),
    })
}

/// Relays stdin/stdout to the kernel's `/mcp` (the `sun` server) or, with
/// `grant`, to `/mcp/<grant.server>` (a granted MCP server) until
/// either side closes.
pub async fn shim(kernel: &str, token: &str, grant: Option<&str>) -> Result<()> {
    let path = match grant {
        None => "/mcp".to_owned(),
        Some(grant) => {
            // The kernel only relays servers it granted (slug names).
            let grant: McpGrant =
                serde_json::from_str(grant).context("the grant is not valid JSON")?;
            format!("/mcp/{}", grant.server)
        }
    };
    let base = kernel.trim_end_matches('/').trim_end_matches("/mcp");
    let mut request = ws_url(base, &path)?.into_client_request()?;
    request
        .headers_mut()
        .insert(AUTHORIZATION, format!("Bearer {token}").parse()?);
    let (socket, _) = tokio_tungstenite::connect_async(request)
        .await
        .with_context(|| format!("dialing the kernel's {path} (is the session still running?)"))?;
    let (mut sink, mut stream) = socket.split();
    let up = async move {
        let mut stdin = BufReader::new(tokio::io::stdin()).lines();
        while let Some(line) = stdin.next_line().await? {
            if !line.trim().is_empty() {
                sink.send(Message::Text(line.into())).await?;
            }
        }
        sink.close().await?;
        anyhow::Ok(())
    };
    let down = async move {
        let mut stdout = tokio::io::stdout();
        while let Some(message) = stream.next().await {
            match message? {
                Message::Text(text) => {
                    stdout.write_all(text.as_bytes()).await?;
                    stdout.write_all(b"\n").await?;
                    stdout.flush().await?;
                }
                Message::Close(_) => break,
                _ => {}
            }
        }
        anyhow::Ok(())
    };
    tokio::select! {
        r = up => r,
        r = down => r,
    }
}

/// `sun plugin mcp`: the MCP server registry (`<data>/mcp_servers`, shared
/// with the catalog). Secrets are ecosystem secrets (`sun secret set NAME`).
#[derive(Subcommand)]
pub enum McpServerCommand {
    /// Register a server: validated, connected once, its tools printed.
    Add {
        /// Name, `[a-z][a-z0-9_-]*` (`sun` is reserved).
        name: String,
        /// Streamable HTTP endpoint.
        #[arg(long, conflicts_with_all = ["command", "args", "network", "secrets"], required_unless_present = "command")]
        url: Option<String>,
        /// Ecosystem secret sent as `Authorization: Bearer <value>`.
        #[arg(long, requires = "url")]
        bearer_secret: Option<String>,
        /// A stdio command (bare name or absolute path), run sandboxed.
        #[arg(long)]
        command: Option<String>,
        /// An argument for `--command` (repeat).
        #[arg(long = "arg", allow_hyphen_values = true)]
        args: Vec<String>,
        /// A host (`host` or `host:port`) the command may reach (repeat).
        #[arg(long = "network")]
        network: Vec<String>,
        /// An ecosystem secret passed to the command as an env var (repeat).
        #[arg(long = "secret")]
        secrets: Vec<String>,
    },
    /// Connect to a registered server and print its tools.
    Tools {
        /// Server name.
        name: String,
    },
    /// List registered servers.
    List,
    /// Unregister a server.
    Remove {
        /// Server name.
        name: String,
    },
}

/// Runs a `sun plugin mcp` command against the data dir's registry and store.
pub async fn server_command(data_dir: &Path, command: McpServerCommand) -> Result<()> {
    let catalog = Catalog::open(data_dir)?;
    match command {
        McpServerCommand::Add {
            name,
            url,
            bearer_secret,
            command,
            args,
            network,
            secrets,
        } => {
            let transport = match (url, command) {
                (Some(url), _) => McpTransport::Url { url, bearer_secret },
                (None, Some(command)) => McpTransport::Command {
                    command,
                    args,
                    network,
                    secrets,
                },
                (None, None) => bail!("give --url or --command"),
            };
            let server = McpServer { name, transport };
            server.validate()?;
            let secrets = secret_values(data_dir, &server).await?;
            let tools = catalog.register_mcp_server(&server, secrets).await?;
            print_tools(&tools);
            println!("registered {}", server.name);
        }
        McpServerCommand::Tools { name } => {
            let server = catalog.mcp_server(&name)?;
            let secrets = secret_values(data_dir, &server).await?;
            print_tools(&catalog.mcp_tools(&server, secrets).await?);
        }
        McpServerCommand::List => {
            for server in catalog.mcp_servers()? {
                let target = match &server.transport {
                    McpTransport::Url { url, .. } => url.clone(),
                    McpTransport::Command { command, args, .. } => {
                        format!("{command} {}", args.join(" "))
                    }
                };
                println!("{:<24} {}", server.name, target.trim_end());
            }
        }
        McpServerCommand::Remove { name } => {
            catalog.remove_mcp_server(&name)?;
            println!("removed {name}");
        }
    }
    Ok(())
}

fn print_tools(tools: &[McpTool]) {
    for tool in tools {
        let marker = if tool.read_only { "read-only" } else { "" };
        println!("{:<32} {marker:<9} {}", tool.name, tool.description);
    }
}

/// The ecosystem secrets `server` needs, from the store.
async fn secret_values(data_dir: &Path, server: &McpServer) -> Result<Vec<(String, String)>> {
    let names = server.secret_names();
    if names.is_empty() {
        return Ok(Vec::new());
    }
    let store = Store::open(data_dir).await?;
    Ok(sun_engine::mcp_secret_values(&store, &names).await?)
}

/// `sun mcp proxy <grant>`: serves the granted tools of a registered MCP
/// server on stdio for one agent session. Refuses a server that
/// is not registered.
// Reads the kernel's store and master.key under `data_dir`: a debugging aid
// on the kernel's machine. Sessions use `sun mcp relay`.
pub async fn proxy(data_dir: &Path, grant: &str) -> Result<()> {
    let grant: McpGrant = serde_json::from_str(grant).context("the grant is not valid JSON")?;
    let catalog = Catalog::open(data_dir)?;
    let server = catalog.mcp_server(&grant.server)?;
    let secrets = secret_values(data_dir, &server).await?;
    let client = catalog.mcp_connect(&server, secrets).await?;
    sun_mod::serve_mcp_proxy(client, grant, tokio::io::stdin(), tokio::io::stdout()).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    #[test]
    fn kernel_urls_become_websocket_urls() {
        let url = |k| super::ws_url(k, "/mcp").unwrap();
        assert_eq!(url("http://127.0.0.1:7700/"), "ws://127.0.0.1:7700/mcp");
        assert_eq!(url("https://sun.acme.com"), "wss://sun.acme.com/mcp");
        assert_eq!(url("ws://h/mcp"), "ws://h/mcp");
        assert!(super::ws_url("ftp://h", "/mcp").is_err());
    }
}
