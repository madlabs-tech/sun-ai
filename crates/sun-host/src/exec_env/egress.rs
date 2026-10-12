//! Host-side egress for a `vm` with a network allowlist.
//!
//! The VM's only NIC is QEMU user networking with `restrict=on`: no guest
//! packet is routed to the host or the outside. Its one exit is a `guestfwd`
//! from [`GUEST_PROXY`] to this CONNECT proxy on the host's loopback, which
//! tunnels only to the allowlisted host names (any port) and resolves them on
//! the host. Both live outside the VM, so the agent's root inside can flush
//! any firewall it likes and still reach nothing else.
//!
//! The QEMU argv comes from Lima, so [`qemu_wrapper`] is a script Lima runs
//! as `qemu-system-<arch>` (`QEMU_SYSTEM_<ARCH>`): it adds `restrict=on` and
//! the `guestfwd` to the user netdev and refuses any other network backend.
// ponytail: same idea as sun-mod's per-mod proxy, but sun-host may not depend
// on sun-mod and this one allows any port; share it if a third copy
// shows up.

use std::net::{IpAddr, SocketAddr};
use std::path::Path;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

use super::sh_quote;

/// Where the guest reaches the proxy: an unused address in Lima's slirp
/// network (`192.168.5.0/24`: gateway `.2`, DNS `.3`, DHCP from `.15`).
pub(super) const GUEST_PROXY: &str = "192.168.5.4:3128";

const MAX_HEAD: usize = 8 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// A running proxy; dropping it stops it (open tunnels included).
pub(super) struct Egress {
    pub(super) port: u16,
    task: JoinHandle<()>,
}

impl Drop for Egress {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Starts the proxy on `127.0.0.1:<ephemeral>` for `hosts`. With
/// `public_only` (marketplace hosts) a name resolving only to loopback,
/// private or link-local addresses is refused, so the buyer's allowlist
/// can't reach the seller's LAN.
pub(super) async fn start(hosts: &[String], public_only: bool) -> std::io::Result<Egress> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
    let port = listener.local_addr()?.port();
    let hosts: Vec<String> = hosts.iter().map(|h| h.to_ascii_lowercase()).collect();
    let task = tokio::spawn(async move {
        let mut tunnels = tokio::task::JoinSet::new();
        while let Ok((client, _)) = listener.accept().await {
            let hosts = hosts.clone();
            tunnels.spawn(async move {
                let _ = serve(client, &hosts, public_only).await;
            });
            while tunnels.try_join_next().is_some() {}
        }
    });
    Ok(Egress { port, task })
}

async fn serve(mut client: TcpStream, hosts: &[String], public_only: bool) -> std::io::Result<()> {
    let Some(head) = read_head(&mut client).await? else {
        return reply(&mut client, "400 Bad Request").await;
    };
    let first = head.lines().next().unwrap_or_default();
    let target = match first.split(' ').collect::<Vec<_>>()[..] {
        ["CONNECT", target, version] if version.starts_with("HTTP/1.") => target,
        _ => return reply(&mut client, "405 Method Not Allowed").await,
    };
    let parsed = target
        .rsplit_once(':')
        .and_then(|(host, port)| Some((host, port.parse::<u16>().ok()?)))
        .filter(|(_, port)| *port != 0);
    let Some((host, port)) = parsed else {
        return reply(&mut client, "400 Bad Request").await;
    };
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    if !hosts.contains(&host) {
        return reply(&mut client, "403 Forbidden").await;
    }
    let addrs: Vec<SocketAddr> = match tokio::net::lookup_host((host.as_str(), port)).await {
        Ok(addrs) => addrs.filter(|a| !public_only || public(a.ip())).collect(),
        Err(_) => Vec::new(),
    };
    if addrs.is_empty() {
        return reply(&mut client, "502 Bad Gateway").await;
    }
    let upstream = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(&addrs[..]));
    let Ok(Ok(mut upstream)) = upstream.await else {
        return reply(&mut client, "502 Bad Gateway").await;
    };
    client
        .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
        .await?;
    tokio::io::copy_bidirectional(&mut client, &mut upstream).await?;
    Ok(())
}

/// `false` for loopback, private, link-local, CGNAT and unspecified addresses.
fn public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            !(v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || (a == 100 && b & 0xc0 == 64))
        }
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => public(IpAddr::V4(v4)),
            None => {
                !(v6.is_loopback()
                    || v6.is_unspecified()
                    || v6.is_unique_local()
                    || v6.is_unicast_link_local())
            }
        },
    }
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

/// The `guestfwd` QEMU adds: each guest connection to [`GUEST_PROXY`] runs
/// `nc 127.0.0.1 <port>` on the host (a `tcp:` chardev would carry only one
/// connection). `nc` must not contain a comma (QEMU's option separator).
pub(super) fn guestfwd(nc: &Path, port: u16) -> Option<String> {
    let nc = nc.to_str().filter(|s| !s.contains([',', '\'', '"']))?;
    Some(format!(
        "guestfwd=tcp:{GUEST_PROXY}-cmd:{nc} 127.0.0.1 {port}"
    ))
}

/// The `QEMU_SYSTEM_<ARCH>` script: appends `restrict=on,<guestfwd>` to
/// every `-netdev user,...` and refuses (exit 1) any other `-netdev`, any
/// `-nic`/`-net` and a netdev that already forwards, so a Lima change can
/// only fail closed. Argv without a netdev (Lima's `-version` probes)
/// passes through to `qemu`.
pub(super) fn qemu_wrapper(qemu: &str, guestfwd: &str) -> String {
    format!(
        "#!/bin/sh\n\
         # sun: the VM's only exit is the host egress proxy\n\
         n=$#; next=\n\
         refuse() {{ echo \"sun: refusing qemu network option: $1\" >&2; exit 1; }}\n\
         for a in \"$@\"; do\n\
         \x20 if [ -n \"$next\" ]; then\n\
         \x20   case \"$a\" in\n\
         \x20     *guestfwd=*|*restrict=*) refuse \"$a\" ;;\n\
         \x20     user,*) a=\"$a,restrict=on,{guestfwd}\" ;;\n\
         \x20     *) refuse \"$a\" ;;\n\
         \x20   esac\n\
         \x20   next=\n\
         \x20 else\n\
         \x20   case \"$a\" in\n\
         \x20     -netdev|--netdev) next=1 ;;\n\
         \x20     -nic|--nic|-net|--net|-netdev=*|--netdev=*) refuse \"$a\" ;;\n\
         \x20   esac\n\
         \x20 fi\n\
         \x20 set -- \"$@\" \"$a\"\n\
         done\n\
         shift \"$n\"\n\
         exec {} \"$@\"\n",
        sh_quote(qemu)
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn public_addresses_only() {
        for ip in [
            "127.0.0.1",
            "10.1.2.3",
            "192.168.5.2",
            "169.254.1.1",
            "100.64.0.1",
            "::1",
            "fd00::1",
            "fe80::1",
            "::ffff:10.0.0.1",
            "0.0.0.0",
        ] {
            assert!(!public(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["1.1.1.1", "140.82.112.3", "2606:4700::1111"] {
            assert!(public(ip.parse().unwrap()), "{ip}");
        }
        assert_eq!(guestfwd(Path::new("/a,b/nc"), 1), None);
    }
}
