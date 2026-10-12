//! The store's error type.

/// Shorthand for `std::result::Result<T, StoreError>`.
pub type Result<T, E = StoreError> = std::result::Result<T, E>;

/// Everything that can go wrong in `sun-store`.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// Validation, an illegal state transition, or a missing row
    /// ([`sun_core::Error::NotFound`]). Also raised when a stored value no
    /// longer parses (unknown status, bad id).
    #[error(transparent)]
    Core(#[from] sun_core::Error),

    /// The database refused or failed the statement.
    #[error("database: {0}")]
    Db(#[from] sqlx::Error),

    /// A migration failed (or an applied one was edited).
    #[error("migration: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),

    /// A `*_json` column did not (de)serialize.
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),

    /// Data-dir file I/O failed.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    /// A blob's content no longer matches its hash.
    #[error("blob {0} is corrupt")]
    CorruptBlob(String),

    /// Master key missing, malformed or unsafe, or a secret does not decrypt.
    /// The message never contains a key or a secret value.
    #[error("secret store: {0}")]
    Crypto(String),
}

impl StoreError {
    /// `NotFound` for a missing row.
    pub(crate) fn not_found(what: &str, id: impl std::fmt::Display) -> Self {
        Self::Core(sun_core::Error::NotFound(format!("{what} {id}")))
    }
}

/// Maps to the boundary error for the `sun-core` traits: domain errors pass
/// through, infrastructure failures become [`sun_core::Error::Unavailable`].
impl From<StoreError> for sun_core::Error {
    fn from(error: StoreError) -> Self {
        match error {
            StoreError::Core(error) => error,
            other => sun_core::Error::Unavailable(format!("store: {other}")),
        }
    }
}
