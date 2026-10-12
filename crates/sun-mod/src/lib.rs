//! # sun-mod — mod host
//!
//! **Responsibility:** the **Plugin** host: manifests, JSON-RPC 2.0 over stdio,
//! lifecycle, permissions and the OS sandbox for third-party mods.
//!
//! **Contract:**
//! - Implements `sun_core::ModLink` ([`ModHost`]); every mod message is
//!   validated at the boundary, and a mod that breaks the protocol is killed.
//! - Mods only get the permissions their manifest declares and the user granted.
//!   The kind rules are enforced at install; [`ModHost::add_sandboxed`] runs a
//!   mod inside the OS [`sandbox`]; [`install_package`] verifies a package's
//!   ed25519 signature before it is copied in.
//! - MCP servers plug in as connectors ([`McpServer`]), and so do plugins
//!   with `mcp_tools` ([`McpClient::for_mod`]); agents reach them only
//!   through [`serve_mcp_proxy`], which exposes the tools a task's
//!   [`McpGrant`] enables (read-only by default).
//! - Wire format and lifecycle: this crate. Manifest and frame
//!   types are owned by `sun-mod-sdk`.
//!
//! **May depend on:** `sun-core`, `sun-mod-sdk`.

mod dev;
mod host;
mod install;
mod manifest;
mod mcp;
mod process;
pub mod sandbox;

pub use dev::watch_dev;
pub use host::{HostConfig, ModHost};
pub use install::{
    SIGNATURE_FILE, install_package, pack_package, package_digest, permission_list, public_key,
    sign_package, sign_yank, unpack_package, verify_signature, verify_yank,
};
pub use manifest::{MANIFEST_FILE, check_manifest, load_manifest, parse_manifest};
pub use mcp::{McpClient, McpServer, McpTool, McpTransport, serve_mcp_proxy};
pub use sandbox::Sandbox;
pub use sun_core::{McpGrant, render_mcp_config};
pub use sun_mod_sdk::{API_VERSION, Manifest, ModKind};
