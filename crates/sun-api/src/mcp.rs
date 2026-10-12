//! Kernel side of the `sun` MCP server: `GET /mcp`, a WebSocket
//! the `sun mcp` stdio shim dials with the session's token
//! (`Authorization: Bearer`). Each text message is one JSON-RPC line of the
//! MCP stdio transport; the socket is bridged to the session's
//! [`sun_context::SunMcp`], which only sees its own run and step.
//!
//! `GET /mcp/{server}` is the **relay** for a session's granted MCP servers,
//! dialed by `sun mcp relay` on any host with the same token: the
//! kernel resolves the session's grant for `server`, connects with the
//! ecosystem secrets and serves the filtering proxy over the socket. Hosts
//! never see the server's secrets or the kernel's store.

use std::sync::Arc;

use axum::Router;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use futures_util::{SinkExt, StreamExt};
use sun_core::Error;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream};

use crate::Api;
use crate::error::ApiError;

/// Largest JSON-RPC message either way (tool results include notes and
/// knowledge snippets).
const MAX_MESSAGE: usize = 4 * 1024 * 1024;

pub(crate) fn router(api: Api) -> Router {
    Router::new()
        .route("/mcp", get(upgrade))
        .route("/mcp/{server}", get(relay))
        .with_state(api)
}

fn bearer(headers: &HeaderMap) -> &str {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or_default()
}

async fn upgrade(State(api): State<Api>, headers: HeaderMap, ws: WebSocketUpgrade) -> Response {
    let Some(sessions) = api.mcp.clone() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let server = match sessions.open(bearer(&headers), Arc::new(api.engine.clone())) {
        Ok(server) => server,
        Err(_) => return StatusCode::UNAUTHORIZED.into_response(),
    };
    ws.max_message_size(MAX_MESSAGE)
        .on_upgrade(move |socket| bridge(socket, move |io| server.serve_on(io)))
}

async fn relay(
    State(api): State<Api>,
    Path(server): Path<String>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    let (Some(sessions), Some(catalog)) = (api.mcp.clone(), api.catalog.clone()) else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let Some((run_id, step_key)) = sessions.scope(bearer(&headers)) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let connected = async {
        let grant = api
            .engine
            .session_mcp_grant(run_id, &step_key, &server)
            .await?;
        // A registered MCP server, else an installed plugin serving
        // `mcp_tools`: it runs in its own sandbox with its own
        // secrets, so the kernel only relays.
        let registered = match (catalog.mcp_server(&server), api.mods.clone()) {
            (Err(Error::NotFound(_)), Some(mods)) => {
                return Ok((sun_catalog::McpClient::for_mod(mods, &server), grant));
            }
            (registered, _) => registered?,
        };
        let secrets = api.engine.mcp_secrets(&registered.secret_names()).await?;
        let client = catalog.mcp_connect(&registered, secrets).await?;
        Ok::<_, Error>((client, grant))
    };
    let (client, grant) = match connected.await {
        Ok(connected) => connected,
        Err(e) => return ApiError::from(e).into_response(),
    };
    ws.max_message_size(MAX_MESSAGE).on_upgrade(move |socket| {
        bridge(socket, move |io| async move {
            let (read, write) = tokio::io::split(io);
            sun_catalog::serve_mcp_proxy(client, grant, read, write).await
        })
    })
}

/// Pumps WebSocket text messages to the server `serve` runs on its end of a
/// pipe as lines, and its lines back, until either side hangs up.
async fn bridge<F, T>(socket: WebSocket, serve: impl FnOnce(DuplexStream) -> F)
where
    F: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let (server_io, ours) = tokio::io::duplex(64 * 1024);
    let serving = tokio::spawn(serve(server_io));
    let (read, mut write) = tokio::io::split(ours);
    let (mut sink, mut stream) = socket.split();
    let inbound = async move {
        while let Some(Ok(message)) = stream.next().await {
            match message {
                Message::Text(text) => {
                    let line = format!("{}\n", text.as_str().trim_end());
                    if write.write_all(line.as_bytes()).await.is_err() {
                        break;
                    }
                }
                Message::Close(_) => break,
                _ => {}
            }
        }
    };
    let outbound = async move {
        let mut lines = BufReader::new(read).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if sink.send(Message::Text(line.into())).await.is_err() {
                break;
            }
        }
        let _ = sink.close().await;
    };
    tokio::select! {
        () = inbound => {}
        () = outbound => {}
    }
    serving.abort();
}
