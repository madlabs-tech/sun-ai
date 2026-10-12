# Deployment

The ways to run Sun. Back to the [overview](README.md).

Every shape runs the same `sun` binary. Only where the kernel and the hosts live changes.

## 1. One box

```text
  ┌───────────────── your machine or a VPS ─────────────────┐
  │  sun serve --with-host   (one process)                  │
  │                                                         │
  │    kernel ◄── ws://<listen>/host (loopback) ──► host    │
  │      │                                           │      │
  │      ▼                                           ▼      │
  │   data dir                                  agent CLIs  │
  │   sun.db · blobs · sun.yaml                 Claude Code │
  │   kernel.json · host/                       Codex · …   │
  └─────────────────────────────────────────────────────────┘
       browser ──► http://127.0.0.1:7700
```

- The default, and what the installer sets up. `scripts/install.sh` writes a systemd unit on Linux (`packaging/systemd/sun.service`) or a launchd agent on macOS (`packaging/launchd/dev.sun.kernel.plist`), both running `sun serve --with-host`.
- The installer's data dir is `/var/lib/sun` on Linux and `~/Library/Application Support/sun` on macOS. The install URL and release signing key are **placeholders**; no public release exists yet, so build from source (see the README "Quick start").
- **Development:** `bash scripts/dev.sh` builds the UI and the kernel, runs `sun serve --with-host` on port 7700 with data in `.dev/sun`, plus the vite dev server on port 5173. Ctrl-C stops both.

## 2. Kernel on a server, hosts on dev machines

```text
   browsers · sun CLI
          │ HTTPS
          ▼
  ┌──────────────────┐         ┌────────────────────────────┐
  │ TLS proxy (your  │──HTTP──►│ sun serve (kernel + store) │
  │ own; not shipped)│         │ listen: 0.0.0.0:7700       │
  └──────────────────┘         └────────────────────────────┘
          ▲
          │ wss://sun.example.com/host   (each host dials out, seq/ack)
          │
          ├────────────────────┬────────────────────┐
          │                    │                    │
  ┌───────┴────────┐   ┌───────┴────────┐   ┌───────┴────────┐
  │ laptop         │   │ build box      │   │ Mac behind NAT │
  │ sun host run   │   │ sun host run   │   │ sun host run   │
  │ + agent CLIs   │   │ + agent CLIs   │   │ + agent CLIs   │
  └────────────────┘   └────────────────┘   └────────────────┘
```

- Run `sun serve` on the server, with `listen` and `public_url` set in `sun.yaml`.
- On each dev machine: copy a token from the UI's "Add host" (single use), run `sun host join <token> --kernel https://sun.example.com`, then `sun host run`. The host keeps its credentials in `<data dir>/host/host.json` and reconnects on its own.
- Hosts dial out, so they need no open port. The kernel needs one port that hosts and browsers can reach.
- `sun serve` speaks plain HTTP. For `https://` and `wss://`, put your own TLS-terminating proxy in front; Sun does not ship one.
- The queue places each run on the least busy online host that has the runtime installed and logged in.
- Known limit: extension runtimes and exec envs reach only the in-process host (`--with-host`), not a remote one.

## 3. High availability (active-passive)

```text
        node A: primary                            node B: standby
  ┌───────────────────────────────┐       ┌───────────────────────────────┐
  │ scripts/ha.sh start           │       │ scripts/ha.sh start           │
  │   litestream replicate        │       │   waits while A's heartbeat   │
  │     └─ sun serve --with-host  │       │   is fresh, then restores     │
  │   file sync loop + heartbeat  │       │   and runs as primary         │
  └───────┬───────────────┬───────┘       └───────────────┬───────────────┘
          │ sun.db        │ blobs, sun.yaml, mods, …      │ reads the heartbeat;
          ▼               ▼                               │ restores both to start
  ┌────────────────┐ ┌──────────────────────┐             │
  │ SUN_DB_REPLICA │ │ SUN_FILES_REPLICA    │◄────────────┤
  │ (Litestream)   │ │ (a dir or rclone)    │             │
  └───────▲────────┘ └──────────────────────┘             │
          └───────────────────────────────────────────────┘
```

- Litestream replicates `sun.db`; `scripts/ha.sh` copies the files beside it and keeps one primary at a time. Each node needs `litestream` (plus `rclone` for a remote files replica), `packaging/litestream.yml` as `/etc/litestream.yml` (or `LITESTREAM_CONFIG`), and the same `SUN_MASTER_KEY`.
- **Recommended: manual restore.** Stop the primary (`ha.sh` exits 0 on SIGTERM after a last file sync), then run `ha.sh start` on the other node. It restores the latest database and files, and refuses to start on an empty database when a backup should exist.
- **Automatic takeover is experimental**, verified only by `scripts/ha-chaos.sh` on one machine. A standby takes over when the primary's heartbeat stops. Exit code 3 means another node took over; start it again to rejoin as a standby.
- `sun doctor` on the primary checks that the replica has caught up and the file sync is fresh.
- Full setup, env vars and service notes: README, "Configuration" → "High availability".

## Sun Cloud

Sun Cloud (a private repo, closed source) runs one kernel per tenant from its control plane. It is optional, and real cloud hosting is **unproven**. See the [overview](README.md).
