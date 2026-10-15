//! # sun-host — runtime host daemon
//!
//! **Responsibility:** runs agent sessions on a machine: worktrees, exec
//! environments, processes and resource accounting, plus device verify on
//! simulators and emulators, and design-canvas dev servers on loopback
//! (`preview.*`). Dials out to the kernel
//! over WebSocket; the wire format is `sun_core::host_protocol`.
//!
//! **Contract:**
//! - Host events carry `(session_id, seq)` and are buffered on disk until the
//!   kernel acks them.
//! - Drives CLIs only through `sun_core::RuntimeAdapter` impls from `sun-runtime`.
//!
//! **May depend on:** `sun-core`, `sun-runtime`.

//!
//! **Use:** `enroll` once (`sun host join <token>`), then
//! `Host::open(HostConfig::new(data_dir), adapters)?.run(Credentials::load(data_dir)?)`.
//! On-disk layout under the data dir: `host.json` (credentials), `buffer/`
//! (unacked events), `repos/` (bare clones), `work/<task_id>` (task dirs),
//! `tmp/<session_id>` (each session's `TMPDIR`), `skills/` (installed skills,
//! copied into each session's native skills folder).

mod buffer;
mod connection;
mod device;
mod exec_env;
mod host;
mod preview;
mod process;
mod worktree;

pub use connection::{Credentials, HostStatus, enroll, leave, status};
pub use device::DeviceConfig;
pub use exec_env::ModEnvs;
pub use host::{Host, HostConfig};
