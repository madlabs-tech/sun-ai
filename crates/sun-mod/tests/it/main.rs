//! Integration tests for `sun-mod`: one binary, so one link for every file.
//! `tests/plugins.rs` stays its own binary: it and `mod_host` each install the
//! process-global tracing subscriber, and a process takes only one.

mod dev;
mod mcp;
mod mod_host;
mod sandbox;

/// Runs `path --version` until exec stops failing with ETXTBSY. Every
/// executable the tests copy or write takes `--version` harmlessly: the mods
/// and `sun-echo-mcp` exit on stdin EOF, `sun-mod-sandbox` on a bad policy.
///
/// ponytail: rust-lang/rust#114554. A fork in another test thread can inherit
/// our write fd of the fresh file until its exec closes it (O_CLOEXEC), so an
/// exec in that window fails with "Text file busy". Retrying here keeps the
/// real exec out of the window; drop this once std closes the race.
fn warm(path: &std::path::Path) {
    for _ in 0..50 {
        match std::process::Command::new(path).arg("--version").output() {
            Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy => {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            _ => return,
        }
    }
}
