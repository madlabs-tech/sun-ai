//! # sun-runtime — agent CLI adapters
//!
//! **Responsibility:** one **Adapter** per agent CLI (Claude Code, Codex,
//! OpenCode), each implementing `sun_core::RuntimeAdapter`.
//!
//! **Contract:**
//! - Transport per CLI, best first: ACP, then structured headless JSON, then PTY
//!   (chosen from the recorded fixtures in `fixtures/`).
//! - Every CLI output is normalized to `sun_core::SunEvent`; limits map to
//!   `SunEvent::LimitHit`. Adapters are tested by replaying recorded fixtures.
//!
//! **Shared pieces** every adapter reuses: [`process`] (JSONL process driver
//! and fixture replay), [`limit`] (limit regex list and reset-time parser) and
//! [`envelope`] (instruction templates). [`contract`] checks every adapter's
//! events against the same contract. [`byo_key`] is the BYO-key runtime
//! source OpenCode renders into its provider config; [`credits`] is the env
//! that points a CLI at the Sun gateway. [`fake`] is the scripted
//! test runtime (binary `sun-fake-runtime`). [`skills`] copies
//! installed skills into each CLI's native skills folder per session.
//! [`mod_adapter`] forwards to a runtime an extension serves (mod API 1.2).
//!
//! **May depend on:** `sun-core`.

pub mod byo_key;
pub mod claude;
pub mod codex;
pub mod contract;
pub mod credits;
pub mod envelope;
pub mod fake;
pub mod limit;
pub mod mcp;
pub mod mod_adapter;
pub mod opencode;
pub mod process;
pub mod skills;

pub use claude::ClaudeCode;
pub use codex::Codex;
pub use fake::FakeRuntime;
pub use mod_adapter::ModRuntimeAdapter;
pub use opencode::OpenCode;
