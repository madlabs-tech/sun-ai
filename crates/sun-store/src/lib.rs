//! # sun-store — persistence
//!
//! **Responsibility:** the only crate that touches the database and the blob
//! directory. SQLite only: migrations, backup
//! (`VACUUM INTO`), the append-only event log (the ledger), the step journal,
//! the transactional outbox, FTS5 search and content-addressed zstd blobs.
//!
//! **Contract:**
//! - Exposes **Repository** traits over the domain types in `sun_core::model`.
//! - Every state change is one transaction: step/run state + `event` rows +
//!   `outbox` rows commit together. No transaction is held across an
//!   `.await` on I/O; writes are serialized (SQLite has one writer).
//! - Status columns store `as_str()` of the `sun_core` enums and parse with `FromStr`.
//! - Applied migrations are never edited; a backup is taken before migrating.
//!
//! **May depend on:** `sun-core`.
//!
//! ## Using it
//!
//! ```no_run
//! # async fn demo(run_id: sun_core::RunId, step_id: sun_core::StepId,
//! #               step_key: sun_core::StepKey, item: sun_core::model::OutboxItem)
//! #               -> sun_store::Result<()> {
//! use sun_core::StepStatus;
//! use sun_store::{EventLog, NewEvent, OutboxRepo, RunRepo, Store};
//!
//! // Opens (or creates) `<data_dir>/sun.db` in WAL mode, backs it up to
//! // `<data_dir>/backups/` if a migration is pending, then migrates.
//! let store = Store::open("/var/lib/sun").await?;
//!
//! // One unit of work: state + events + outbox, committed together.
//! let mut tx = store.begin().await?;
//! let previous = tx.set_step_status(step_id, StepStatus::Verifying).await?;
//! tx.append_event(NewEvent::new(
//!     run_id,
//!     Some(step_key),
//!     "step_status",
//!     serde_json::json!({ "from": previous.as_str(), "to": "verifying" }),
//! ))
//! .await?;
//! tx.enqueue(&item).await?; // delivered by the outbox worker after commit
//! tx.commit().await?; // dropped without commit = rolled back
//! # Ok(()) }
//! ```
//!
//! - **Write transactions** ([`Store::begin`]) run `BEGIN IMMEDIATE` on a
//!   single-connection pool, so writers queue in-process instead of failing
//!   with `SQLITE_BUSY`. Do all non-DB work (host calls, blob puts, rendering)
//!   **before** `begin`, keep the `Tx` to DB calls only, then commit.
//! - **Reads** ([`Store::read`]) come from a separate read-only pool and see
//!   one consistent snapshot for the life of that `Tx`; they never block writers.
//! - **Statuses** only change through `set_*_status`, which loads the current
//!   value and applies `StateMachine::transition`: an illegal move returns
//!   [`sun_core::Error::InvalidTransition`] and writes nothing.
//! - **Event log** (**Event sourcing** + **Observer**): `append_event` assigns a
//!   gapless ascending `seq`. Committed events are broadcast to
//!   [`Store::subscribe`]. The live-timeline recipe is: subscribe first, then
//!   page `events_after(run, cursor, n)` until empty, then consume the
//!   receiver, skipping `seq <= cursor`. On `RecvError::Lagged`, page from the
//!   cursor again — the database is the truth, the channel only a wake-up.
//! - **Host events** use `append_host_event(session_id, host_seq, ..)`; a
//!   redelivered pair returns `None` (exactly-once). Ack what was stored.
//! - **Outbox**: `enqueue` is idempotent on `idem_key` ([`sun_core::IdemKey`]).
//! - **Blobs** ([`Store::blobs`]): sha256 of the uncompressed bytes, zstd on
//!   disk. Put them before the `Tx`, store only the hash.
//! - **Knowledge** ([`KnowledgeRepo`]): sources, doc versions and chunks;
//!   `chunk_fts` (unicode61) and `chunk_fts_trigram` follow `chunk` by trigger;
//!   opt-in int8 vectors in `chunk_vec` are scored with sqlite-vec, which
//!   `Store::open` registers (the crate's one `unsafe` block).
//! - **Export / import** ([`Store::export`], [`Store::import`]): one archive
//!   file (SQLite) with every table, every blob and the data dir's config
//!   files ([`ARCHIVED_PATHS`]: sun.yaml, mods, skills, MCP servers, catalog
//!   installs); secrets stay sealed, the master key
//!   is included only on request, or secrets are dropped and listed for
//!   re-entry ([`ArchiveSecrets`]).
//! - **Secrets** ([`SecretRepo`]): values are sealed with
//!   ChaCha20-Poly1305 under the [`MasterKey`] and scoped to an ecosystem, a
//!   mod or a workspace; [`sun_core::model::Secret`] is metadata only. `known_secrets()` is
//!   the `known_secrets` list for [`sun_core::redact::redact`] — pass it to
//!   everything that emits text (runtime driver, hand-off, context packs,
//!   logs, knowledge chunks). `append_event` already applies it to payloads.
//! - **Members and audit** ([`MemberRepo`], [`MembershipRepo`],
//!   [`InvitationRepo`], [`AuditRepo`]): members and their deployment owner
//!   flag, their role in each workspace (a workspace keeps at least one
//!   owner), invitations into a workspace; `audit` is append-only (a trigger blocks
//!   `UPDATE`/`DELETE`). Write the audit row in the same `Tx` as the change.
//!   [`LoginRepo`]: sign-in sessions and one-time login links, stored as
//!   `sha256(token)` only.
//! - **Editor grants** ([`EditorGrantRepo`]): the editor capabilities given
//!   in a workspace; owners' implicit ones are never stored.
//! - **Hosts** ([`HostRepo`]): enrolled hosts with their raw capabilities
//!   report, and `host_runtime`, one row per `(host, tool)` projected from
//!   that report on every `hello` / `capabilities` write (a tool gone from a
//!   later report keeps its row with `removed_at`; back again, same row).
//! - **Git** ([`RepoRepo`], [`VcsRepo`], [`PullRequestRepo`]): a workspace's
//!   repos, its git-host connections, and the pull requests mirrored from
//!   them with their task links; `upsert_pull_request` drops stale deliveries.
//! - New tables: add a new `migrations/<UTC yyyymmddHHMMSS>_<name>.sql`;
//!   never edit an applied one (sqlx checks the checksum and refuses to start).
//!   A migration must sort after every table it alters, indexes or triggers
//!   on; a test checks that.

mod agent;
mod artifact;
mod blob;
mod correction;
mod credential;
mod editor_grant;
mod error;
mod eval;
mod event_query;
mod export;
mod factor;
mod host;
mod knowledge;
mod member;
mod membership;
mod memory;
mod mod_install;
mod note;
mod pin;
mod pointer;
mod project;
mod pull_request;
mod quality;
mod repo;
mod secret;
mod session;
mod setting;
mod vcs;
mod workspace;

use std::path::Path;

use sqlx::migrate::Migrator;
use sqlx::pool::PoolOptions;
use sqlx::sqlite::SqliteConnectOptions;
use sqlx::{ConnectOptions, Executor, Sqlite, SqlitePool};
use sun_core::model::{EventRecord, TaskActivity};
use tokio::sync::broadcast;

pub use agent::AgentRepo;
pub use artifact::{ArtifactRepo, RepoRepo};
pub use blob::BlobStore;
pub use correction::CorrectionRepo;
pub use credential::CredentialRepo;
pub use editor_grant::EditorGrantRepo;
pub use error::{Result, StoreError};
pub use eval::EvalRepo;
pub use event_query::{COMPACTED, EventQuery, KEPT_KINDS};
pub use export::{ARCHIVED_PATHS, ArchiveSecrets, SECRETS_TO_REENTER, SecretToReenter};
pub use factor::{FactorRepo, TotpSecret};
pub use host::HostRepo;
pub use knowledge::{
    ChunkHit, ChunkText, DocChunk, DocSummary, FtsIndex, KnowledgeRepo, KnowledgeSource, NewDoc,
    SourceSummary,
};
pub use member::{AuditRepo, LoginRepo, MAX_OTP_ATTEMPTS, MemberRepo};
pub use membership::{InvitationRepo, MembershipRepo};
pub use memory::{MemoryPr, MemoryRepo};
pub use mod_install::ModInstallRepo;
pub use note::NoteRepo;
pub use pin::PinRepo;
pub use pointer::PointerRepo;
pub use project::ProjectRepo;
pub use pull_request::PullRequestRepo;
pub use quality::QualityRepo;
use repo::Channels;
pub use repo::{
    ContextRepo, EventLog, NewEvent, OutboxRepo, RunRepo, StepPayer, TaskRepo, Tx, Usage,
    WorkflowRepo,
};
pub use secret::{MASTER_KEY_ENV, MASTER_KEY_FILE, MAX_SECRET_BYTES, MasterKey, SecretRepo};
pub use session::{RuntimeRepo, SessionRepo};
pub use setting::{SettingRepo, TaskContext};
pub use vcs::VcsRepo;
pub use workspace::{WorkspaceItem, WorkspaceRepo};

static MIGRATOR: Migrator = sqlx::migrate!();

/// Events kept for slow subscribers before they see `Lagged` and must re-page.
const EVENT_CHANNEL: usize = 1024;
/// Read-only connections; readers never block the single writer (WAL).
const READERS: u32 = 4;

/// Handle to the data dir: the database, the event broadcast and the blob
/// store. Cheap to clone; every clone shares the same pools.
#[derive(Debug, Clone)]
pub struct Store {
    writer: SqlitePool,
    reader: SqlitePool,
    channels: Channels,
    blobs: BlobStore,
    key: MasterKey,
    data_dir: std::path::PathBuf,
}

impl Store {
    /// Opens `<data_dir>/sun.db` and applies all migrations.
    ///
    /// Creates the dir and DB if needed, in WAL mode, and backs the DB up to
    /// `<data_dir>/backups/` first when a migration is pending on an existing
    /// DB. Blobs live in `<data_dir>/blobs/`. Loads the secrets [`MasterKey`]
    /// (`$SUN_MASTER_KEY`, else `<data_dir>/master.key`, created on first
    /// open) and fails with [`StoreError::Crypto`] if it does not open the
    /// stored secrets or the canary sealed at the data dir's first open.
    pub async fn open(data_dir: impl AsRef<Path>) -> Result<Self> {
        let data_dir = data_dir.as_ref();
        std::fs::create_dir_all(data_dir)?;
        let key = MasterKey::load(data_dir)?;
        let blobs = BlobStore::new(data_dir.join("blobs"));
        let (writer, reader) = open_sqlite(data_dir, &key, &blobs).await?;
        // Fail fast on a wrong key: every event append decrypts the secrets,
        // and the canary catches it even with none stored (a standby started
        // with another `SUN_MASTER_KEY`).
        let mut tx = Tx::new(writer.begin().await?, None, key.clone());
        tx.known_secrets().await?;
        tx.check_master_key_canary().await?;
        tx.commit().await?;
        Ok(Self {
            writer,
            reader,
            channels: Channels {
                events: broadcast::channel(EVENT_CHANNEL).0,
                activity: broadcast::channel(EVENT_CHANNEL).0,
            },
            blobs,
            key,
            data_dir: data_dir.to_path_buf(),
        })
    }

    /// Starts a write transaction (`BEGIN IMMEDIATE` on the single writer
    /// connection). Waits while another write `Tx` is open. See [`Tx`].
    pub async fn begin(&self) -> Result<Tx> {
        let writer = self.writer.clone();
        let inner =
            uncancellable(async move { Ok(writer.begin_with("BEGIN IMMEDIATE").await?) }).await?;
        Ok(Tx::new(
            inner,
            Some(self.channels.clone()),
            self.key.clone(),
        ))
    }

    /// Starts a read-only snapshot transaction. Writes through it fail.
    pub async fn read(&self) -> Result<Tx> {
        let reader = self.reader.clone();
        let inner = uncancellable(async move { Ok(reader.begin().await?) }).await?;
        Ok(Tx::new(inner, None, self.key.clone()))
    }

    /// Receives every event committed from now on (**Observer**).
    pub fn subscribe(&self) -> broadcast::Receiver<EventRecord> {
        self.channels.events.subscribe()
    }

    /// Receives every task activity entry committed from now on
    /// (**Observer**). As with events: subscribe first, then page
    /// [`TaskRepo::activity_of`], skipping what was already seen.
    pub fn subscribe_activity(&self) -> broadcast::Receiver<TaskActivity> {
        self.channels.activity.subscribe()
    }

    /// The content-addressed blob store.
    pub fn blobs(&self) -> &BlobStore {
        &self.blobs
    }

    /// Writes a consistent copy of the database to `path` (`VACUUM INTO`).
    /// `path` must not exist. Safe while the store is in use.
    pub async fn backup(&self, path: &Path) -> Result<()> {
        vacuum_into(&self.writer, path).await
    }
}

/// The SQLite pools: one writer (writes queue in-process instead of failing
/// with `SQLITE_BUSY`) and [`READERS`] read-only connections.
async fn open_sqlite(
    data_dir: &Path,
    key: &MasterKey,
    blobs: &BlobStore,
) -> Result<(SqlitePool, SqlitePool)> {
    knowledge::register_sqlite_vec();
    let file = SqliteConnectOptions::new().filename(data_dir.join("sun.db"));
    // The pragmas the URL cannot hold run per connection.
    let writer = PoolOptions::<Sqlite>::new()
        .max_connections(1)
        .after_connect(|conn, _| {
            Box::pin(async move {
                conn.execute(
                    "PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL; \
                     PRAGMA foreign_keys = ON; PRAGMA busy_timeout = 5000;",
                )
                .await
                .map(drop)
            })
        })
        .connect(file.clone().create_if_missing(true).to_url_lossy().as_str())
        .await?;
    if migration_pending(&writer).await? {
        let name = format!(
            "pre-migrate-{}.db",
            chrono::Utc::now().format("%Y%m%dT%H%M%S%.3fZ")
        );
        let dir = data_dir.join("backups");
        std::fs::create_dir_all(&dir)?;
        vacuum_into(&writer, &dir.join(name)).await?;
    }
    migrate(&writer, key, blobs, data_dir).await?;
    let reader = PoolOptions::<Sqlite>::new()
        .max_connections(READERS)
        .after_connect(|conn, _| {
            Box::pin(async move { conn.execute("PRAGMA busy_timeout = 5000;").await.map(drop) })
        })
        .connect(file.read_only(true).to_url_lossy().as_str())
        .await?;
    Ok((writer, reader))
}

/// What a data step gets: the migration's transaction, the master key, the
/// blob store and the data dir.
type DataStep = for<'a> fn(
    &'a mut sqlx::SqliteConnection,
    &'a MasterKey,
    &'a BlobStore,
    &'a Path,
) -> std::pin::Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>>;

/// Rust data steps by migration version: work SQL can't do (open a sealed
/// secret, read a blob or data-dir file). A step runs in its migration's transaction,
/// before the migration's SQL, so the data it reads is still there and the
/// step, the SQL and the bookkeeping row commit together or not at all. It
/// runs once, when its migration does: never at a later open.
pub(crate) const DATA_STEPS: &[(i64, DataStep)] = &[
    (20261022100000, finish_old_data),
    (20261102090100, copy_agents),
    (20261102090300, move_secrets),
    (20261103090200, seed_catalog_installs),
    (20261104090300, backfill_host_runtimes),
];

/// `20261022100000_drop_logo_blob`: re-seal what `secret_to_reseal` lists
/// and move the logos `workspace.logo_blob` names, before both go.
fn finish_old_data<'a>(
    conn: &'a mut sqlx::SqliteConnection,
    key: &'a MasterKey,
    blobs: &'a BlobStore,
    _: &'a Path,
) -> std::pin::Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
    Box::pin(async move {
        secret::reseal_old_ecosystem_secrets(&mut *conn, key).await?;
        workspace::move_old_logos(conn, blobs).await
    })
}

/// `20261102090100_agent_copies`: see [`agent::copy_agents_into_their_workspaces`].
fn copy_agents<'a>(
    conn: &'a mut sqlx::SqliteConnection,
    _: &'a MasterKey,
    _: &'a BlobStore,
    _: &'a Path,
) -> std::pin::Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
    Box::pin(agent::copy_agents_into_their_workspaces(conn))
}

/// `20261102090300_secret_workspace`: see [`secret::move_ecosystem_secrets_to_main`].
fn move_secrets<'a>(
    conn: &'a mut sqlx::SqliteConnection,
    key: &'a MasterKey,
    _: &'a BlobStore,
    _: &'a Path,
) -> std::pin::Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
    Box::pin(secret::move_ecosystem_secrets_to_main(conn, key))
}

/// `20261103090200_catalog_install_main`: see [`workspace::seed_main_catalog_installs`].
fn seed_catalog_installs<'a>(
    conn: &'a mut sqlx::SqliteConnection,
    _: &'a MasterKey,
    _: &'a BlobStore,
    data_dir: &'a Path,
) -> std::pin::Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
    Box::pin(workspace::seed_main_catalog_installs(conn, data_dir))
}

/// `20261104090300_host_runtime_backfill`: see [`host::backfill_host_runtimes`].
fn backfill_host_runtimes<'a>(
    conn: &'a mut sqlx::SqliteConnection,
    _: &'a MasterKey,
    _: &'a BlobStore,
    _: &'a Path,
) -> std::pin::Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
    Box::pin(host::backfill_host_runtimes(conn))
}

/// Applies the pending migrations in order. sqlx 0.8's SQLite driver runs
/// every migration inside a transaction, where `PRAGMA foreign_keys` is a
/// no-op, so a migration starting `-- no-transaction` (a table rebuild), or
/// one with a data step ([`DATA_STEPS`]), is applied here: its data step,
/// its SQL and its `_sqlx_migrations` row in one transaction, then `PRAGMA
/// foreign_key_check` (a violation rolls it all back) and commit; a
/// `-- no-transaction` one with foreign keys off around it. Then sqlx checks
/// every applied migration's checksum as usual.
async fn migrate(
    pool: &SqlitePool,
    key: &MasterKey,
    blobs: &BlobStore,
    data_dir: &Path,
) -> Result<()> {
    use sqlx::migrate::{Migrate, MigrateError};
    let mut conn = pool.acquire().await?;
    conn.ensure_migrations_table().await?;
    if let Some(version) = conn.dirty_version().await? {
        return Err(MigrateError::Dirty(version).into());
    }
    let applied: Vec<i64> = conn
        .list_applied_migrations()
        .await?
        .iter()
        .map(|m| m.version)
        .collect();
    let pending = MIGRATOR
        .iter()
        .filter(|m| !m.migration_type.is_down_migration() && !applied.contains(&m.version));
    for migration in pending {
        let step = DATA_STEPS
            .iter()
            .find(|(version, _)| *version == migration.version)
            .map(|(_, step)| *step);
        if !migration.no_tx && step.is_none() {
            conn.apply(migration).await?;
            continue;
        }
        if migration.no_tx {
            conn.execute("PRAGMA foreign_keys = OFF").await?;
        }
        let applied = apply_here(&mut conn, migration, step, key, blobs, data_dir).await;
        // Back on even when it failed: the connection goes back to the pool.
        conn.execute("PRAGMA foreign_keys = ON").await?;
        applied?;
    }
    MIGRATOR.run_direct(&mut *conn).await?;
    Ok(())
}

/// One migration with its data step in one transaction (see [`migrate`]).
async fn apply_here(
    conn: &mut sqlx::SqliteConnection,
    migration: &sqlx::migrate::Migration,
    step: Option<DataStep>,
    key: &MasterKey,
    blobs: &BlobStore,
    data_dir: &Path,
) -> Result<()> {
    let mut tx = sqlx::Connection::begin(&mut *conn).await?;
    if let Some(step) = step {
        step(&mut tx, key, blobs, data_dir).await?;
    }
    tx.execute(&*migration.sql)
        .await
        .map_err(|e| sqlx::migrate::MigrateError::ExecuteMigration(e, migration.version))?;
    sqlx::query(
        "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time)
         VALUES ($1, $2, TRUE, $3, -1)",
    )
    .bind(migration.version)
    .bind(&*migration.description)
    .bind(&*migration.checksum)
    .execute(&mut *tx)
    .await?;
    let broken: Vec<(String, Option<i64>, String)> =
        sqlx::query_as("SELECT \"table\", rowid, parent FROM pragma_foreign_key_check")
            .fetch_all(&mut *tx)
            .await?;
    if let Some((table, row, parent)) = broken.first() {
        return Err(sun_core::Error::InvalidInput(format!(
            "migration {}: {} row(s) break a foreign key, e.g. {table} row {row:?} -> {parent}; \
             nothing was changed",
            migration.version,
            broken.len()
        ))
        .into());
    }
    tx.commit().await?;
    Ok(())
}

/// True when the DB already has migrations applied and at least one is new.
/// A brand-new DB has nothing to lose, so it gets no backup.
async fn migration_pending(pool: &SqlitePool) -> Result<bool> {
    let tracked = sqlx::query(
        "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = '_sqlx_migrations'",
    )
    .fetch_optional(pool)
    .await?
    .is_some();
    if !tracked {
        return Ok(false);
    }
    let applied: Vec<i64> = sqlx::query_scalar("SELECT version FROM _sqlx_migrations")
        .fetch_all(pool)
        .await?;
    Ok(MIGRATOR.iter().any(|m| !applied.contains(&m.version)))
}

async fn vacuum_into(pool: &SqlitePool, path: &Path) -> Result<()> {
    let text = path.to_str().ok_or_else(|| {
        sun_core::Error::InvalidInput(format!("backup path {} is not UTF-8", path.display()))
    })?;
    sqlx::query("VACUUM INTO $1")
        .bind(text)
        .execute(pool)
        .await?;
    Ok(())
}

/// Runs a `BEGIN` on its own task. A caller cancelled mid-`BEGIN` (an
/// aborted task, a dropped request future) would otherwise drop sqlx's
/// acknowledgement and with it the only handle to the open transaction: the
/// pooled connection then stays inside it, and with one writer every later
/// [`Store::begin`] fails ("begin_with at non-zero transaction depth"). On
/// its own task the `Transaction` is always built, and dropping it rolls back.
async fn uncancellable(
    begin: impl Future<Output = Result<sqlx::Transaction<'static, Sqlite>>> + Send + 'static,
) -> Result<sqlx::Transaction<'static, Sqlite>> {
    tokio::spawn(begin)
        .await
        .map_err(|e| StoreError::Io(std::io::Error::other(format!("begin task: {e}"))))?
}
