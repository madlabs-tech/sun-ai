//! Errors that cross crate boundaries through the `sun-core` traits.

/// Shorthand for `std::result::Result<T, sun_core::Error>`.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Error returned by `sun-core` functions and by every boundary trait.
///
/// Crates keep their own richer error types internally and map to this at the
/// trait boundary. Messages must already be redacted ([`crate::redact`]).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// Input failed validation (bad id, unknown enum value, malformed field).
    #[error("invalid input: {0}")]
    InvalidInput(String),

    /// A state machine refused a transition.
    #[error("invalid {machine} transition: {from} -> {to}")]
    InvalidTransition {
        /// The state machine, e.g. `StepStatus`.
        machine: &'static str,
        /// Current state.
        from: &'static str,
        /// Requested state.
        to: &'static str,
    },

    /// The referenced entity does not exist.
    #[error("not found: {0}")]
    NotFound(String),

    /// The caller is known but not allowed to do this (e.g. an approval by
    /// someone not listed in `who:`).
    #[error("forbidden: {0}")]
    Forbidden(String),

    /// The workspace's plan has no room for one more of something (a seat,
    /// an agent, a host...). The message says which cap and how to get unstuck.
    #[error("{0}")]
    PlanLimit(String),

    /// The other side (host, runtime, mod) is not reachable or not ready.
    /// The caller may retry per its policy.
    #[error("unavailable: {0}")]
    Unavailable(String),

    /// The other side broke its protocol (bad frame, unexpected reply).
    #[error("protocol violation: {0}")]
    Protocol(String),

    /// A third party's HTTP API refused the action with `http_status` (a
    /// mod's `FAILED` reply carrying `data.http_status`). Handled like
    /// [`Error::InvalidInput`] (not retryable); a caller that can fix the
    /// cause (e.g. renew a token after a 401) reads the status, never the text.
    #[error("invalid input: {message}")]
    Refused {
        /// The HTTP status the third party answered, e.g. 401.
        http_status: u16,
        /// What it said (redacted).
        message: String,
    },
}
