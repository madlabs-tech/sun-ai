//! Secrets at rest: the [`MasterKey`] and the [`SecretRepo`].
//!
//! Values are sealed with ChaCha20-Poly1305 under one master key, with the
//! row's `(scope, name)` as associated data. The key comes from
//! `$SUN_MASTER_KEY` (64 hex chars) or `<data_dir>/master.key` (mode 0600,
//! created on first open). Plaintext only leaves this module through
//! [`SecretRepo::secret_value`] and [`SecretRepo::known_secrets`]; the latter
//! feeds [`sun_core::redact`] — the store itself redacts every event it appends.

use std::io::{ErrorKind, Write as _};
use std::path::Path;

use async_trait::async_trait;
use chacha20poly1305::aead::{Aead, Generate, Payload};
use chacha20poly1305::{ChaCha20Poly1305, KeyInit, Nonce};
use sqlx::Row;
use sqlx::sqlite::SqliteRow;
use sun_core::model::{Secret, SecretScope};
use sun_core::redact::MIN_SECRET_LEN;
use sun_core::{SecretId, Timestamp};

use crate::repo::{parse_ts, ts};
use crate::{Result, StoreError, Tx};

/// Env var holding the master key as 64 hex chars; wins over the key file.
pub const MASTER_KEY_ENV: &str = "SUN_MASTER_KEY";
/// Key file name inside the data dir.
pub const MASTER_KEY_FILE: &str = "master.key";
/// Largest secret value accepted.
pub const MAX_SECRET_BYTES: usize = 64 * 1024;

const KEY_BYTES: usize = 32;
const NONCE_BYTES: usize = 12;

/// The key that seals secret values. `Debug` never prints it.
#[derive(Clone)]
pub struct MasterKey([u8; KEY_BYTES]);

impl std::fmt::Debug for MasterKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MasterKey([REDACTED])")
    }
}

impl MasterKey {
    /// `$SUN_MASTER_KEY` if set, else `<data_dir>/master.key`, which is
    /// created (0600, fresh random key) when missing. A key file that group or
    /// others can access is refused.
    pub fn load(data_dir: &Path) -> Result<Self> {
        match std::env::var(MASTER_KEY_ENV) {
            Ok(hex) => Self::from_hex(&hex),
            Err(std::env::VarError::NotPresent) => Self::load_file(&data_dir.join(MASTER_KEY_FILE)),
            Err(e) => Err(crypto(format!("{MASTER_KEY_ENV}: {e}"))),
        }
    }

    /// Parses 64 hex chars (surrounding whitespace ignored).
    pub fn from_hex(text: &str) -> Result<Self> {
        let text = text.trim();
        let bad = || crypto(format!("master key must be {} hex chars", KEY_BYTES * 2));
        if text.len() != KEY_BYTES * 2 {
            return Err(bad());
        }
        let mut key = [0u8; KEY_BYTES];
        for (byte, pair) in key.iter_mut().zip(text.as_bytes().chunks(2)) {
            let pair = std::str::from_utf8(pair).map_err(|_| bad())?;
            *byte = u8::from_str_radix(pair, 16).map_err(|_| bad())?;
        }
        Ok(Self(key))
    }

    /// A fresh random key from the OS RNG.
    pub fn generate() -> Result<Self> {
        Ok(Self(
            <[u8; KEY_BYTES]>::try_generate().map_err(|e| crypto(format!("os rng: {e}")))?,
        ))
    }

    fn load_file(path: &Path) -> Result<Self> {
        let key = Self::generate()?;
        match key.write_new(path) {
            Ok(()) => return Ok(key),
            Err(StoreError::Io(e)) if e.kind() == ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(path)?.permissions().mode();
            if mode & 0o077 != 0 {
                return Err(crypto(format!(
                    "{} has mode {:o}; run `chmod 600` on it",
                    path.display(),
                    mode & 0o777
                )));
            }
        }
        Self::from_hex(&std::fs::read_to_string(path)?)
    }

    /// Writes the key as hex to a new file at `path` with mode 0600.
    pub(crate) fn write_new(&self, path: &Path) -> Result<()> {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let mut file = options.open(path)?;
        file.write_all(format!("{}\n", self.to_hex()).as_bytes())?;
        file.sync_all()?;
        Ok(())
    }

    /// The key as 64 lowercase hex chars, for a key file or `$SUN_MASTER_KEY`.
    /// This is the secret itself: never log it.
    pub fn to_hex(&self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }

    fn cipher(&self) -> ChaCha20Poly1305 {
        ChaCha20Poly1305::new(&self.0.into())
    }

    /// Seals `value` with ChaCha20-Poly1305 under a fresh nonce, bound to
    /// `aad` (associated data naming where the value belongs, so a sealed
    /// value can't be moved elsewhere). Returns `nonce ‖ ciphertext`.
    pub fn seal(&self, aad: &[u8], value: &[u8]) -> Result<Vec<u8>> {
        let nonce = Nonce::try_generate().map_err(|e| crypto(format!("os rng: {e}")))?;
        let sealed = self
            .cipher()
            .encrypt(&nonce, Payload { msg: value, aad })
            .map_err(|_| crypto("encryption failed".into()))?;
        Ok([nonce.as_slice(), &sealed].concat())
    }

    /// Opens what [`Self::seal`] returned for the same `aad`. A wrong key,
    /// another `aad` or a tampered value is a [`StoreError::Crypto`] that
    /// names neither the key nor the value.
    pub fn open(&self, aad: &[u8], sealed: &[u8]) -> Result<String> {
        // Never name the value or key here: this error may be logged.
        let wrong =
            || crypto("a stored secret does not decrypt: wrong master key or tampered row".into());
        let (nonce, sealed) = sealed.split_at_checked(NONCE_BYTES).ok_or_else(wrong)?;
        let nonce = Nonce::try_from(nonce).map_err(|_| wrong())?;
        let plain = self
            .cipher()
            .decrypt(&nonce, Payload { msg: sealed, aad })
            .map_err(|_| wrong())?;
        String::from_utf8(plain).map_err(|_| wrong())
    }
}

fn crypto(message: String) -> StoreError {
    StoreError::Crypto(message)
}

/// `kernel_setting` key of the canary: a fixed value sealed under the master
/// key at first open, so a wrong key is caught even before any secret or
/// 2-step factor exists.
pub(crate) const CANARY_KEY: &str = "master_key_canary";
const CANARY_AAD: &[u8] = b"master_key_canary";
const CANARY_VALUE: &[u8] = b"sun";

impl Tx {
    /// Proves the `Tx`'s key is this data dir's: opens the sealed canary, or
    /// writes one when there is none yet (a data dir from before the canary,
    /// or a fresh one). Another key is a [`StoreError::Crypto`]; commit the
    /// `Tx` afterwards so a new canary sticks.
    pub(crate) async fn check_master_key_canary(&mut self) -> Result<()> {
        match crate::SettingRepo::setting(self, CANARY_KEY).await? {
            Some(sealed) => {
                let sealed: Vec<u8> = serde_json::from_value(sealed)?;
                self.key().open(CANARY_AAD, &sealed).map(drop).map_err(|_| {
                    crypto(format!(
                        "{MASTER_KEY_ENV} / {MASTER_KEY_FILE} is not the key this data dir was \
                         first opened with: use the original key (an HA standby needs the \
                         primary's {MASTER_KEY_ENV})"
                    ))
                })
            }
            None => {
                let sealed = self.key().seal(CANARY_AAD, CANARY_VALUE)?;
                crate::SettingRepo::put_setting(self, CANARY_KEY, &serde_json::to_value(sealed)?)
                    .await
            }
        }
    }
}

fn aad(scope: &SecretScope, name: &str) -> Vec<u8> {
    format!("{}\0{}\0{name}", scope.kind(), scope.id()).into_bytes()
}

/// Data step of the migration that drops `secret_to_reseal`
/// ([`crate::DATA_STEPS`]): re-seals the ecosystem secrets it lists, rows
/// the workspace-rename migration moved from the scope's old name
/// `workspace` to `ecosystem`, whose sealed bytes still carry `workspace` in
/// the AAD. Only listed rows are touched, so a per-workspace secret never is.
pub(crate) async fn reseal_old_ecosystem_secrets(
    conn: &mut sqlx::SqliteConnection,
    key: &MasterKey,
) -> Result<()> {
    let rows: Vec<(String, String, String, Vec<u8>)> = sqlx::query_as(
        "SELECT s.id, s.scope_id, s.name, s.sealed
         FROM secret_to_reseal r JOIN secret s ON s.id = r.id",
    )
    .fetch_all(&mut *conn)
    .await?;
    for (id, scope_id, name, sealed) in rows {
        let old = format!("workspace\0{scope_id}\0{name}").into_bytes();
        let value = key.open(&old, &sealed)?;
        let scope = SecretScope::from_parts("ecosystem", &scope_id)?;
        let sealed = key.seal(&aad(&scope, &name), value.as_bytes())?;
        sqlx::query("UPDATE secret SET sealed = $1 WHERE id = $2")
            .bind(sealed)
            .bind(&id)
            .execute(&mut *conn)
            .await?;
    }
    Ok(())
}

/// `secret` (**Repository**). Values are sealed on write and opened on read;
/// [`Secret`] carries metadata only.
#[async_trait]
pub trait SecretRepo {
    /// Seals and stores `value` as `(scope, name)`, replacing any earlier
    /// value (the id is kept). Values shorter than
    /// [`MIN_SECRET_LEN`] are refused: `redact` could not hide them.
    async fn set_secret(&mut self, scope: &SecretScope, name: &str, value: &str) -> Result<Secret>;
    /// The plaintext value, or `NotFound`. Audit the read when a user asks.
    async fn secret_value(&mut self, scope: &SecretScope, name: &str) -> Result<String>;
    /// Metadata of one scope's secrets (all scopes for `None`), by scope then name.
    async fn list_secrets(&mut self, scope: Option<&SecretScope>) -> Result<Vec<Secret>>;
    /// Deletes `(scope, name)`; `false` when it did not exist.
    async fn delete_secret(&mut self, scope: &SecretScope, name: &str) -> Result<bool>;
    /// Every stored value in plaintext (but [`Secret::PLAIN`] ones): the
    /// `known_secrets` argument of [`sun_core::redact::redact`] for anything
    /// leaving the kernel as text.
    async fn known_secrets(&mut self) -> Result<Vec<String>>;
    /// Secrets an import without values ([`crate::ArchiveSecrets::Dropped`])
    /// left to re-enter, minus those set since.
    async fn secrets_to_reenter(&mut self) -> Result<Vec<crate::SecretToReenter>>;
}

const SECRET_COLUMNS: &str = "id, scope_kind, scope_id, name, updated_at";

fn secret_from(row: &SqliteRow) -> Result<Secret> {
    Ok(Secret {
        id: row.try_get::<String, _>("id")?.parse()?,
        scope: SecretScope::from_parts(row.try_get("scope_kind")?, row.try_get("scope_id")?)?,
        name: row.try_get("name")?,
        updated_at: parse_ts(row.try_get("updated_at")?)?,
    })
}

#[async_trait]
impl SecretRepo for Tx {
    async fn set_secret(&mut self, scope: &SecretScope, name: &str, value: &str) -> Result<Secret> {
        Secret::validate_name(name)?;
        // Never echo the value in an error.
        let plain = Secret::is_plain(name);
        let min = if plain { 1 } else { MIN_SECRET_LEN };
        if !(min..=MAX_SECRET_BYTES).contains(&value.len()) {
            return Err(sun_core::Error::InvalidInput(format!(
                "secret `{name}` must be {min} to {MAX_SECRET_BYTES} bytes"
            ))
            .into());
        }
        if let SecretScope::Mod(mod_name) = scope
            && mod_name.trim().is_empty()
        {
            return Err(
                sun_core::Error::InvalidInput("secret scope: empty mod name".into()).into(),
            );
        }
        // Redacted from logs and mod output from now on, not just after a reboot.
        if !plain {
            sun_core::redact::remember_secret(value);
        }
        let sealed = self.key().seal(&aad(scope, name), value.as_bytes())?;
        let now: Timestamp = chrono::Utc::now();
        let row = sqlx::query(&format!(
            "INSERT INTO secret ({SECRET_COLUMNS}, sealed) VALUES ($1, $2, $3, $4, $5, $6)
             ON CONFLICT (scope_kind, scope_id, name)
             DO UPDATE SET sealed = excluded.sealed, updated_at = excluded.updated_at
             RETURNING {SECRET_COLUMNS}"
        ))
        .bind(SecretId::generate().to_string())
        .bind(scope.kind())
        .bind(scope.id())
        .bind(name)
        .bind(ts(&now))
        .bind(sealed)
        .fetch_one(self.conn())
        .await?;
        secret_from(&row)
    }

    async fn secret_value(&mut self, scope: &SecretScope, name: &str) -> Result<String> {
        let sealed: Vec<u8> = sqlx::query_scalar(
            "SELECT sealed FROM secret WHERE scope_kind = $1 AND scope_id = $2 AND name = $3",
        )
        .bind(scope.kind())
        .bind(scope.id())
        .bind(name)
        .fetch_optional(self.conn())
        .await?
        .ok_or_else(|| {
            StoreError::not_found("secret", format!("{}:{}/{name}", scope.kind(), scope.id()))
        })?;
        let value = self.key().open(&aad(scope, name), &sealed)?;
        // A value set by another process (`sun secret set`) is learned here.
        if !Secret::is_plain(name) {
            sun_core::redact::remember_secret(&value);
        }
        Ok(value)
    }

    async fn list_secrets(&mut self, scope: Option<&SecretScope>) -> Result<Vec<Secret>> {
        let order = "ORDER BY scope_kind, scope_id, name";
        let rows = match scope {
            Some(scope) => sqlx::query(&format!(
                "SELECT {SECRET_COLUMNS} FROM secret WHERE scope_kind = $1 AND scope_id = $2 {order}"
            ))
            .bind(scope.kind())
            .bind(scope.id())
            .fetch_all(self.conn())
            .await?,
            None => {
                sqlx::query(&format!("SELECT {SECRET_COLUMNS} FROM secret {order}"))
                    .fetch_all(self.conn())
                    .await?
            }
        };
        rows.iter().map(secret_from).collect()
    }

    async fn delete_secret(&mut self, scope: &SecretScope, name: &str) -> Result<bool> {
        let done =
            sqlx::query("DELETE FROM secret WHERE scope_kind = $1 AND scope_id = $2 AND name = $3")
                .bind(scope.kind())
                .bind(scope.id())
                .bind(name)
                .execute(self.conn())
                .await?;
        Ok(done.rows_affected() > 0)
    }

    async fn known_secrets(&mut self) -> Result<Vec<String>> {
        let rows = sqlx::query("SELECT scope_kind, scope_id, name, sealed FROM secret")
            .fetch_all(self.conn())
            .await?;
        let mut values = Vec::new();
        for row in &rows {
            let name: &str = row.try_get("name")?;
            if Secret::is_plain(name) {
                continue;
            }
            let scope =
                SecretScope::from_parts(row.try_get("scope_kind")?, row.try_get("scope_id")?)?;
            values.push(
                self.key()
                    .open(&aad(&scope, name), row.try_get("sealed")?)?,
            );
        }
        Ok(values)
    }

    async fn secrets_to_reenter(&mut self) -> Result<Vec<crate::SecretToReenter>> {
        let Some(listed) = crate::SettingRepo::setting(self, crate::SECRETS_TO_REENTER).await?
        else {
            return Ok(Vec::new());
        };
        let listed: Vec<crate::SecretToReenter> = serde_json::from_value(listed)?;
        let present = self.list_secrets(None).await?;
        Ok(listed
            .into_iter()
            .filter(|s| {
                !present
                    .iter()
                    .any(|p| p.scope == s.scope && p.name == s.name)
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_master_key_seals_with_aad_and_hides_in_debug() {
        let key = MasterKey::from_hex(&"ab".repeat(32)).unwrap();
        assert_eq!(format!("{key:?}"), "MasterKey([REDACTED])");
        let sealed = key.seal(b"a", b"hunter2hunter2").unwrap();
        assert_eq!(key.open(b"a", &sealed).unwrap(), "hunter2hunter2");
        // Moved to another row (other AAD), tampered, or another key: refused.
        assert!(key.open(b"b", &sealed).is_err());
        let mut tampered = sealed.clone();
        tampered[NONCE_BYTES] ^= 1;
        assert!(key.open(b"a", &tampered).is_err());
        let other = MasterKey::from_hex(&"cd".repeat(32)).unwrap();
        assert!(other.open(b"a", &sealed).is_err());
        // Two seals of one value differ (fresh nonce).
        assert_ne!(sealed, key.seal(b"a", b"hunter2hunter2").unwrap());
    }

    #[test]
    fn secret_master_key_hex_is_validated() {
        for bad in ["", "ab", &"zz".repeat(32), &"ab".repeat(33)] {
            assert!(MasterKey::from_hex(bad).is_err(), "{bad}");
        }
        assert!(MasterKey::from_hex(&format!(" {}\n", "0F".repeat(32))).is_ok());
    }
}
