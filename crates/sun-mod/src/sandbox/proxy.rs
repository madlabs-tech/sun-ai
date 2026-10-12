//! The per-mod CONNECT proxy: the only way out of the sandbox. It listens on
//! loopback and tunnels `CONNECT host:port` only to the mod's declared hosts;
//! everything else gets `403`.

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

/// Longest request head we read before giving up.
const MAX_HEAD: usize = 8 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// A running proxy; dropping it stops it (open tunnels included).
pub(crate) struct Proxy {
    pub(crate) port: u16,
    task: JoinHandle<()>,
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Starts a proxy on `127.0.0.1:<ephemeral>` that allows `allowed` entries
/// (`host` = port 443, or `host:port`). Needs a Tokio runtime.
pub(crate) fn start(allowed: Vec<String>) -> std::io::Result<Proxy> {
    let std_listener = std::net::TcpListener::bind(("127.0.0.1", 0))?;
    std_listener.set_nonblocking(true)?;
    let port = std_listener.local_addr()?.port();
    let listener = TcpListener::from_std(std_listener)?;
    let allowed: Vec<(String, u16)> = allowed
        .iter()
        .filter_map(|entry| split_host_port(entry, 443))
        .map(|(host, port)| (host.to_ascii_lowercase(), port))
        .collect();
    let task = tokio::spawn(async move {
        let mut tunnels = tokio::task::JoinSet::new();
        while let Ok((client, _)) = listener.accept().await {
            let allowed = allowed.clone();
            tunnels.spawn(async move {
                let _ = serve(client, &allowed).await;
            });
            while tunnels.try_join_next().is_some() {}
        }
    });
    Ok(Proxy { port, task })
}

async fn serve(mut client: TcpStream, allowed: &[(String, u16)]) -> std::io::Result<()> {
    let Some(head) = read_head(&mut client).await? else {
        return reply(&mut client, "400 Bad Request").await;
    };
    let target =
        head.lines()
            .next()
            .and_then(|line| match line.split(' ').collect::<Vec<_>>()[..] {
                ["CONNECT", target, version] if version.starts_with("HTTP/1.") => Some(target),
                _ => None,
            });
    let Some(target) = target else {
        return reply(&mut client, "405 Method Not Allowed").await;
    };
    let Some((host, port)) = split_host_port(target, 0).filter(|(_, port)| *port != 0) else {
        return reply(&mut client, "400 Bad Request").await;
    };
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    if !allowed.iter().any(|(h, p)| *h == host && *p == port) {
        return reply(&mut client, "403 Forbidden").await;
    }
    // ponytail: no check that a declared name resolves to a public address
    // (DNS rebinding to loopback/LAN); add an IP filter if marketplace mods need it.
    let upstream = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect((host.as_str(), port)));
    let Ok(Ok(mut upstream)) = upstream.await else {
        return reply(&mut client, "502 Bad Gateway").await;
    };
    client
        .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
        .await?;
    tokio::io::copy_bidirectional(&mut client, &mut upstream).await?;
    Ok(())
}

/// Reads up to the blank line ending the request head; `None` if it is too
/// long or not UTF-8. Clients wait for our `200` before sending more.
async fn read_head(client: &mut TcpStream) -> std::io::Result<Option<String>> {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    while !buf.ends_with(b"\r\n\r\n") {
        if buf.len() >= MAX_HEAD || client.read(&mut byte).await? == 0 {
            return Ok(None);
        }
        buf.push(byte[0]);
    }
    Ok(String::from_utf8(buf).ok())
}

async fn reply(client: &mut TcpStream, status: &str) -> std::io::Result<()> {
    let msg = format!("HTTP/1.1 {status}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
    client.write_all(msg.as_bytes()).await
}

/// Splits `host[:port]` (`[v6]:port` too); a missing port is `default_port`.
pub(crate) fn split_host_port(entry: &str, default_port: u16) -> Option<(&str, u16)> {
    if entry.contains(['/', '@', '*', ' ']) {
        return None;
    }
    if let Some(rest) = entry.strip_prefix('[') {
        let (host, after) = rest.split_once(']')?;
        return match after.strip_prefix(':') {
            Some(port) => Some((host, port.parse().ok()?)),
            None if after.is_empty() => Some((host, default_port)),
            None => None,
        };
    }
    match entry.rsplit_once(':') {
        Some((host, port)) if !host.contains(':') => Some((host, port.parse().ok()?)),
        Some(_) => None, // bare IPv6 needs brackets
        None => Some((entry, default_port)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sandbox_proxy_host_port_parsing() {
        assert_eq!(
            split_host_port("api.github.com", 443),
            Some(("api.github.com", 443))
        );
        assert_eq!(split_host_port("h:8080", 443), Some(("h", 8080)));
        assert_eq!(split_host_port("[::1]:9", 443), Some(("::1", 9)));
        assert_eq!(split_host_port("::1", 443), None);
        assert_eq!(split_host_port("h:x", 443), None);
        assert_eq!(split_host_port("u@h", 443), None);
    }
}
