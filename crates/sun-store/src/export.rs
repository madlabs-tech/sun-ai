//! Ecosystem export / import: one archive file holding the
//! whole ecosystem — every table (workflows, history, settings, mod installs,
//! sealed secrets), the blobs, and the data dir's config files (sun.yaml,
//! installed mods and their data, skills, MCP servers, catalog installs) — so
//! an ecosystem moves between self-host and Sun Cloud.
//!
//! The archive is itself a SQLite file: a `VACUUM INTO` snapshot of `sun.db`
//! plus three extra tables, `sun_export_meta` (format, optional master key),
//! `sun_export_blob` (hash → the blob's zstd bytes) and `sun_export_file`
//! (data-dir path → mode + zstd bytes). Secrets stay sealed; the master key is
//! only written into the archive when the caller asks for it; with
//! [`ArchiveSecrets::Dropped`] no sealed value leaves at all, and the names
//! are kept for re-entry ([`SecretRepo::secrets_to_reenter`]).

use serde::{Deserialize, Serialize};
use sqlx::pool::PoolOptions;
use sqlx::sqlite::SqliteConnectOptions;
use sqlx::{ConnectOptions, Executor, Sqlite, SqlitePool};
use std::fs;
use std::path::{Component, Path, PathBuf};
use sun_core::BlobHash;
use sun_core::model::SecretScope;

use crate::{BlobStore, MASTER_KEY_ENV, MASTER_KEY_FILE, MasterKey, Result, SecretRepo, Store, Tx};

/// Archive format version; bumped on any incompatible layout change.
/// Format 1 archives (no `sun_export_file`) still import.
const FORMAT: &str = "2";
const FORMATS: [&str; 2] = ["1", "2"];
const DB_FILE: &str = "sun.db";
/// Largest data-dir file carried in an archive.
const MAX_FILE: u64 = 64 << 20;

/// What an ecosystem is beside its database and blobs: the data-dir files and
/// folders an export carries (missing ones are skipped). Not carried: the
/// ecosystem's own identity and keys (`master.key`, `license`, `kernel.json`,
/// `host/`, `publisher.key`) and backups.
pub const ARCHIVED_PATHS: [&str; 7] = [
    "sun.yaml",
    "mods",
    "mod-data",
    "skills",
    "mcp_servers",
    "mcp",
    "catalog",
];

/// `kernel_setting` key listing the secrets an import still needs re-entered.
pub const SECRETS_TO_REENTER: &str = "secrets_to_reenter";

/// How [`Store::export`] carries secrets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveSecrets {
    /// Sealed values; importing needs the source master key.
    Sealed,
    /// Sealed values plus the master key: the archive alone opens them.
    WithKey,
    /// No values at all, only their names, to re-enter after import.
    Dropped,
}

/// A secret an imported ecosystem had, whose value was not exported.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretToReenter {
    /// Its scope.
    pub scope: SecretScope,
    /// Its name.
    pub name: String,
}

impl Store {
    /// Writes the whole ecosystem to a new archive file at `archive` (mode
    /// 0600; it must not exist). See [`ArchiveSecrets`]; with `WithKey` the
    /// archive alone opens every secret, so guard it like the key file. Safe
    /// while in use: the DB is a consistent snapshot, and blobs are put before
    /// the rows that cite them.
    pub async fn export(&self, archive: &Path, secrets: ArchiveSecrets) -> Result<()> {
        create_private(archive)?;
        let written = self.export_into(archive, secrets).await;
        if written.is_err() {
            let _ = fs::remove_file(archive);
        }
        written
    }

    async fn export_into(&self, archive: &Path, secrets: ArchiveSecrets) -> Result<()> {
        self.backup(archive).await?;
        let pool = open_single(archive).await?;
        let mut tx = pool.begin().await?;
        sqlx::query(
            "CREATE TABLE sun_export_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL) STRICT;
             CREATE TABLE sun_export_blob (hash TEXT PRIMARY KEY, zst BLOB NOT NULL) STRICT;
             CREATE TABLE sun_export_file (
                 path TEXT PRIMARY KEY, mode INTEGER NOT NULL, zst BLOB NOT NULL) STRICT;",
        )
        .execute(&mut *tx)
        .await?;
        let mut meta = vec![("format", FORMAT.to_owned())];
        match secrets {
            ArchiveSecrets::Sealed => {}
            ArchiveSecrets::WithKey => meta.push(("master_key", self.key.to_hex())),
            ArchiveSecrets::Dropped => {
                let rows: Vec<(String, String, String)> =
                    sqlx::query_as("SELECT scope_kind, scope_id, name FROM secret")
                        .fetch_all(&mut *tx)
                        .await?;
                let mut names = Vec::with_capacity(rows.len());
                for (kind, id, name) in rows {
                    let scope = SecretScope::from_parts(&kind, &id)?;
                    names.push(SecretToReenter { scope, name });
                }
                sqlx::query(
                    "INSERT INTO kernel_setting (key, value_json) VALUES ($1, $2) \
                     ON CONFLICT (key) DO UPDATE SET value_json = excluded.value_json",
                )
                .bind(SECRETS_TO_REENTER)
                .bind(serde_json::to_string(&names)?)
                .execute(&mut *tx)
                .await?;
                // The canary goes too: the importer's own key owns the copy.
                sqlx::query("DELETE FROM secret; DELETE FROM kernel_setting WHERE key = $1")
                    .bind(crate::secret::CANARY_KEY)
                    .execute(&mut *tx)
                    .await?;
            }
        }
        for (key, value) in meta {
            sqlx::query("INSERT INTO sun_export_meta (key, value) VALUES ($1, $2)")
                .bind(key)
                .bind(value)
                .execute(&mut *tx)
                .await?;
        }
        for (path, mode, file) in data_files(&self.data_dir)? {
            let bytes = fs::read(&file)?;
            sqlx::query("INSERT INTO sun_export_file (path, mode, zst) VALUES ($1, $2, $3)")
                .bind(path)
                .bind(i64::from(mode))
                .bind(zstd::encode_all(bytes.as_slice(), 3)?)
                .execute(&mut *tx)
                .await?;
        }
        // ponytail: one blob in memory at a time on the async thread; move to
        // spawn_blocking if exports of large blob dirs stall the runtime.
        for (hash, path) in blob_files(self.blobs.dir())? {
            sqlx::query("INSERT INTO sun_export_blob (hash, zst) VALUES ($1, $2)")
                .bind(hash.as_str())
                .bind(fs::read(path)?)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        pool.close().await;
        Ok(())
    }

    /// Restores an archive from [`Store::export`] into `data_dir`, which must
    /// not hold a `sun.db` yet, then opens it (migrating an older archive).
    ///
    /// The master key is, in order: `$SUN_MASTER_KEY`, an existing
    /// `<data_dir>/master.key`, else the archive's key (written as a 0600
    /// `master.key`). If the archive holds secrets, the key must open all of
    /// them before anything is written; otherwise import fails with
    /// [`crate::StoreError::Crypto`] and the data dir is left untouched.
    /// Every blob is checked against its hash.
    pub async fn import(archive: &Path, data_dir: impl AsRef<Path>) -> Result<Store> {
        let data_dir = data_dir.as_ref();
        fs::create_dir_all(data_dir)?;
        if data_dir.join(DB_FILE).exists() {
            return Err(sun_core::Error::InvalidInput(format!(
                "{} already holds an ecosystem; import needs an empty data dir",
                data_dir.display()
            ))
            .into());
        }
        let staged = data_dir.join("sun.db.import");
        fs::copy(archive, &staged)?;
        let restored = restore(&staged, data_dir).await;
        if restored.is_err() {
            let _ = fs::remove_file(&staged);
        }
        restored?;
        fs::rename(&staged, data_dir.join(DB_FILE))?;
        Store::open(data_dir).await
    }
}

/// Validates `staged`, extracts its blobs and key into `data_dir`, and strips
/// the export tables so `staged` is a plain `sun.db`.
async fn restore(staged: &Path, data_dir: &Path) -> Result<()> {
    let pool = open_single(staged).await?;
    let meta = |key: &'static str| {
        sqlx::query_scalar::<_, String>("SELECT value FROM sun_export_meta WHERE key = $1")
            .bind(key)
            .fetch_optional(&pool)
    };
    let format = meta("format").await.map_err(|_| not_archive())?;
    if !format.as_deref().is_some_and(|f| FORMATS.contains(&f)) {
        return Err(not_archive());
    }
    let has_files = format.as_deref() != Some("1");
    let archived_key = meta("master_key")
        .await?
        .map(|hex| MasterKey::from_hex(&hex))
        .transpose()?;

    // Pick the key and prove it opens every sealed secret before writing.
    let key_file = data_dir.join(MASTER_KEY_FILE);
    let has_own_key = std::env::var_os(MASTER_KEY_ENV).is_some() || key_file.exists();
    let key = match (&archived_key, has_own_key) {
        (_, true) => Some(MasterKey::load(data_dir)?),
        (Some(key), false) => Some(key.clone()),
        (None, false) => None,
    };
    let sealed: i64 = sqlx::query_scalar("SELECT count(*) FROM secret")
        .fetch_one(&pool)
        .await?;
    match &key {
        Some(key) => {
            let mut tx = Tx::new(pool.begin().await?, None, key.clone());
            tx.known_secrets().await?;
            tx.check_master_key_canary().await?;
            tx.commit().await?;
        }
        None if sealed > 0 => {
            return Err(crate::StoreError::Crypto(format!(
                "the archive holds {sealed} sealed secrets but no master key: copy the \
                 source {MASTER_KEY_FILE} into {} or set {MASTER_KEY_ENV}, or re-export \
                 with the key",
                data_dir.display()
            )));
        }
        // No key anywhere and nothing sealed: the key `Store::open` generates
        // becomes this copy's, so the source's canary must not outlive it.
        None => {
            sqlx::query("DELETE FROM kernel_setting WHERE key = $1")
                .bind(crate::secret::CANARY_KEY)
                .execute(&pool)
                .await?;
        }
    }

    let blobs = BlobStore::new(data_dir.join("blobs"));
    let hashes: Vec<String> = sqlx::query_scalar("SELECT hash FROM sun_export_blob")
        .fetch_all(&pool)
        .await?;
    for hash in hashes {
        let expected = BlobHash::new(hash)?;
        let zst: Vec<u8> = sqlx::query_scalar("SELECT zst FROM sun_export_blob WHERE hash = $1")
            .bind(expected.as_str())
            .fetch_one(&pool)
            .await?;
        let corrupt = || crate::StoreError::CorruptBlob(expected.as_str().to_owned());
        let bytes = zstd::decode_all(zst.as_slice()).map_err(|_| corrupt())?;
        if blobs.put(bytes).await? != expected {
            return Err(corrupt());
        }
    }

    if has_files {
        // Validate every path before writing any file.
        let files: Vec<(String, i64, Vec<u8>)> =
            sqlx::query_as("SELECT path, mode, zst FROM sun_export_file ORDER BY path")
                .fetch_all(&pool)
                .await?;
        let mut targets = Vec::with_capacity(files.len());
        for (path, mode, zst) in files {
            targets.push((archived_path(&path)?, mode, zst));
        }
        for (relative, mode, zst) in targets {
            let bytes = zstd::decode_all(zst.as_slice())
                .map_err(|_| crate::StoreError::CorruptBlob(relative.display().to_string()))?;
            let target = data_dir.join(&relative);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&target, bytes)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                // Permission bits only: never setuid/setgid/sticky from an archive.
                let mode = u32::try_from(mode & 0o777).unwrap_or(0o600);
                fs::set_permissions(&target, fs::Permissions::from_mode(mode))?;
            }
            #[cfg(not(unix))]
            let _ = mode;
        }
    }
    if let (Some(key), false) = (&archived_key, has_own_key) {
        key.write_new(&key_file)?;
    }
    sqlx::query("DROP TABLE sun_export_meta; DROP TABLE sun_export_blob;")
        .execute(&pool)
        .await?;
    sqlx::query("DROP TABLE IF EXISTS sun_export_file")
        .execute(&pool)
        .await?;
    sqlx::query("VACUUM").execute(&pool).await?;
    pool.close().await;
    Ok(())
}

fn not_archive() -> crate::StoreError {
    sun_core::Error::InvalidInput(format!("not a Sun ecosystem archive (format {FORMAT})")).into()
}

/// Creates an empty file only the owner can read (`VACUUM INTO` accepts an
/// empty existing file and keeps its mode).
fn create_private(path: &Path) -> Result<()> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(path)?;
    Ok(())
}

/// One connection, rollback journal: the archive stays a single file.
async fn open_single(path: &Path) -> Result<SqlitePool> {
    let url = SqliteConnectOptions::new().filename(path).to_url_lossy();
    Ok(PoolOptions::<Sqlite>::new()
        .max_connections(1)
        .after_connect(|conn, _| {
            Box::pin(async move {
                conn.execute("PRAGMA journal_mode = DELETE; PRAGMA busy_timeout = 5000;")
                    .await
                    .map(drop)
            })
        })
        .connect(url.as_str())
        .await?)
}

/// Every regular file under [`ARCHIVED_PATHS`] in `data_dir`: (`/`-joined
/// relative path, permission bits, file). Symlinks are skipped, never followed.
fn data_files(data_dir: &Path) -> Result<Vec<(String, u32, PathBuf)>> {
    let mut found = Vec::new();
    let mut pending: Vec<PathBuf> = ARCHIVED_PATHS.iter().map(PathBuf::from).collect();
    while let Some(relative) = pending.pop() {
        let path = data_dir.join(&relative);
        let meta = match fs::symlink_metadata(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            other => other?,
        };
        if meta.is_dir() {
            for entry in fs::read_dir(&path)? {
                pending.push(relative.join(entry?.file_name()));
            }
        } else if meta.is_file() {
            if meta.len() > MAX_FILE {
                return Err(sun_core::Error::InvalidInput(format!(
                    "{} is over {} MiB; move it out of the data dir to export",
                    path.display(),
                    MAX_FILE >> 20
                ))
                .into());
            }
            #[cfg(unix)]
            let mode = std::os::unix::fs::PermissionsExt::mode(&meta.permissions()) & 0o777;
            #[cfg(not(unix))]
            let mode = 0o644;
            let name: Vec<String> = relative
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect();
            found.push((name.join("/"), mode, path));
        }
    }
    Ok(found)
}

/// An archive path as a safe relative path under one of [`ARCHIVED_PATHS`]:
/// no `..`, no root, no empty or `.` parts (archives are untrusted input).
fn archived_path(path: &str) -> Result<PathBuf> {
    let bad = || {
        crate::StoreError::from(sun_core::Error::InvalidInput(format!(
            "the archive holds an unsafe file path `{path}`"
        )))
    };
    let parts: Vec<&str> = path.split('/').collect();
    let safe = parts
        .iter()
        .all(|p| !p.is_empty() && *p != "." && *p != ".." && !p.contains('\\'))
        && ARCHIVED_PATHS.contains(&parts[0]);
    let relative: PathBuf = parts.iter().collect();
    match safe
        && relative
            .components()
            .all(|c| matches!(c, Component::Normal(_)))
    {
        true => Ok(relative),
        false => Err(bad()),
    }
}

/// Every finished blob file `<dir>/ab/<hash>.zst`; temp files are skipped.
fn blob_files(dir: &Path) -> Result<Vec<(BlobHash, PathBuf)>> {
    let mut found = Vec::new();
    let shards = match fs::read_dir(dir) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(found),
        other => other?,
    };
    for shard in shards {
        let shard = shard?.path();
        if !shard.is_dir() {
            continue;
        }
        for file in fs::read_dir(&shard)? {
            let path = file?.path();
            let hash = path
                .file_name()
                .and_then(|name| name.to_str())
                .and_then(|name| name.strip_suffix(".zst"))
                .and_then(|hex| BlobHash::new(hex).ok());
            if let Some(hash) = hash {
                found.push((hash, path));
            }
        }
    }
    Ok(found)
}
