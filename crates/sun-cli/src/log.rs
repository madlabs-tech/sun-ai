//! The kernel's logger: `tracing` events (ours and the libraries') to stderr,
//! each line passed through `sun_core::redact::redact_known` (every stored
//! secret value, kept current by the store) first. `SUN_LOG` sets
//! the filter, e.g. `info,sqlx=warn` (default `info`).

use std::io::Write;

use tracing_subscriber::filter::Targets;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// Starts the logger; a second call is a no-op.
pub fn init() {
    let filter = std::env::var("SUN_LOG")
        .ok()
        .and_then(|f| f.parse::<Targets>().ok())
        .unwrap_or_else(|| Targets::new().with_default(tracing::Level::INFO));
    let _ = tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(Redacting)
                .with_ansi(false)
                .with_target(false),
        )
        .with(filter)
        .try_init();
}

/// `line` with every known secret value replaced.
fn redact_line(line: &[u8]) -> String {
    sun_core::redact::redact_known(&String::from_utf8_lossy(line))
}

/// Makes one [`Line`] per event.
struct Redacting;

impl<'a> MakeWriter<'a> for Redacting {
    type Writer = Line;

    fn make_writer(&'a self) -> Line {
        Line(Vec::new())
    }
}

/// Buffers one formatted event; redacts and prints it when dropped, so a
/// secret split across `write` calls is still caught.
struct Line(Vec<u8>);

impl Write for Line {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Drop for Line {
    fn drop(&mut self) {
        let _ = std::io::stderr().write_all(redact_line(&self.0).as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_lines_never_carry_a_stored_secret() {
        sun_core::redact::remember_secret("hunter2-secret");
        let line = redact_line(b"connecting with token=hunter2-secret\n");
        assert!(!line.contains("hunter2-secret"), "{line}");
        assert!(line.starts_with("connecting with token="));
    }
}
