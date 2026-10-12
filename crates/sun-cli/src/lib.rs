//! # sun-cli — the `sun` binary
//!
//! **Responsibility:** **Dependency injection**: parse the command line (clap)
//! and wire the real implementations (store, engine, hosts, mods, API) behind
//! the `sun-core` traits. No domain logic lives here; `anyhow` is allowed only here.
//!
//! **Contract:**
//! - [`serve::start`] boots the kernel: `Store` → `ModHost` (installed mods,
//!   sandboxed, with their config and granted secrets) → `Engine` (with the
//!   `HostHub` as `HostLink` and the `ModHost` as `ModLink`) → `Queue` →
//!   `sun_api::router` (REST, SSE, `/host`, webhooks, the web UI at `/`).
//!   `--with-host` adds a `sun_host::Host` dialing `ws://<listen>/host`.
//!   Under Sun Cloud (`SUN_CLOUD_URL` + `SUN_CLOUD_TOKEN`) the `HostLink` is
//!   wrapped in [`cloud::CreditsLink`] (per-session gateway keys).
//! - Client commands (`task`, `workflow save`) go through the kernel's REST
//!   API ([`client`]); install-time commands (`plugin`, `secret`) write the
//!   store directly and take effect on the next `sun serve`.
//! - The data dir (`--data-dir`, `$SUN_DATA_DIR`, `~/.sun`) holds `sun.yaml`
//!   ([`config::Config`]), `kernel.json` (token + host secret, 0600), the
//!   store, `host/` and `mods/`.
//! - [`ops`] runs an installed Sun: `upgrade` (checkpoint wait, backup,
//!   binary swap, migrate, restart), `rollback`, `backup`, `doctor`.
//!
//! **May depend on:** every crate.

pub mod client;
pub mod cloud;
pub mod config;
pub mod eval;
pub mod installer;
pub mod knowledge;
pub mod log;
pub mod mail;
pub mod market;
pub mod mcp;
pub mod mods;
pub mod ops;
pub mod registry;
pub mod scaffold;
pub mod serve;
pub mod users;
