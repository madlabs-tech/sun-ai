//! Integration tests for `sun-mod`: one binary, so one link for every file.
//! `tests/plugins.rs` stays its own binary: it and `mod_host` each install the
//! process-global tracing subscriber, and a process takes only one.

mod dev;
mod mcp;
mod mod_host;
mod sandbox;
