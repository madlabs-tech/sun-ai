//! # sun-core — the shared contract
//!
//! **Responsibility:** the vocabulary every other crate speaks. Ids, domain
//! types (one per table), the run/step/task **state machines**,
//! [`SunEvent`], errors, the boundary traits ([`RuntimeAdapter`], [`HostLink`],
//! [`ModLink`], [`EngineHandle`]), the one shared secret filter ([`redact`])
//! and plans per workspace ([`entitlement`]).
//!
//! **Contract:**
//! - **No I/O.** Pure types and pure functions only; nothing here opens a file,
//!   socket, process or database.
//! - Higher crates depend on the traits here, never on each other's concrete
//!   types (dependency inversion). `sun-cli` wires the real impls.
//! - Wire/DB names are snake_case. Status enums store [`StateMachine::as_str`]
//!   and parse with `FromStr`.
//! - Traits are deliberately minimal; later phases extend them by adding
//!   methods with documented contracts, not by bypassing them.
//!
//! **May depend on:** nothing but std, serde, serde_json, ulid, chrono,
//! thiserror, async-trait, futures-core and ed25519-dalek (enforced by
//! `scripts/check-deps.sh`).
#![warn(missing_docs)]

/// Defines a fieldless enum whose variants map 1:1 to snake_case strings.
///
/// Generates `ALL`, `as_str`, `Display`, `FromStr` and serde (de)serialization,
/// all from the same string literal, so DB columns, JSON and code never drift.
macro_rules! string_enum {
    (
        $(#[$meta:meta])*
        $vis:vis enum $name:ident {
            $( $(#[$vmeta:meta])* $variant:ident => $text:literal ),+ $(,)?
        }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
        $vis enum $name {
            $( $(#[$vmeta])* #[serde(rename = $text)] $variant ),+
        }

        impl $name {
            /// Every variant, in declaration order.
            pub const ALL: &'static [Self] = &[$(Self::$variant),+];

            /// The snake_case name used in JSON and DB columns.
            pub fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $text),+
                }
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl std::str::FromStr for $name {
            type Err = $crate::Error;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                match s {
                    $($text => Ok(Self::$variant),)+
                    other => Err($crate::Error::InvalidInput(format!(
                        "unknown {} `{}`",
                        stringify!($name),
                        other
                    ))),
                }
            }
        }
    };
}

pub mod entitlement;
mod error;
mod event;
pub mod host_protocol;
mod id;
mod link;
mod mcp_grant;
pub mod model;
pub mod procfs;
pub mod redact;
mod runtime;
pub mod sso;
mod status;

pub use error::{Error, Result};
pub use event::{LimitKind, Role, SunEvent, Todo, TodoStatus};
pub use id::*;
pub use link::{
    AvailableMod, EngineHandle, HostEvent, HostLink, LeaseRequest, ListingFilter, Mailer,
    Marketplace, ModCall, ModEvent, ModInstaller, ModLink, StepBrief, exec_env_action,
    runtime_action,
};
pub use mcp_grant::{McpGrant, render_mcp_config};
pub use runtime::{
    BYO_KEY_ENV, BoxStream, ByoKey, ByoKeyKind, ContainerSpec, EXEC_SHIMS_ENV, EffortCatalog,
    EffortLevel, Envelope, ExecEnvSpec, ExtensionEnvSpec, HostCtx, ModelEffort, PERMISSION_MODES,
    Readiness, RuntimeAdapter, SKILL_RUNTIMES_FILE, SessionHandle, SessionSpec, Steer, StopMode,
    Transcript, VmSpec, repo_skill_dirs, skill_folders,
};
pub use status::{RunStatus, StateMachine, StepStatus, TaskStatus};

/// UTC timestamp used everywhere.
pub type Timestamp = chrono::DateTime<chrono::Utc>;
