-- M1 tables of TRD §5. Never edit this file once applied; add a new
-- timestamp-prefixed migration instead.
--
-- Conventions: ids are ULID text, timestamps are UTC text (sqlx chrono),
-- `*_json` columns hold serde_json text, status columns hold the sun-core
-- enum's `as_str()`. Big content is a blob hash (ADR-11).

CREATE TABLE workspace (
    id            TEXT PRIMARY KEY,
    name          TEXT NOT NULL,
    settings_json TEXT NOT NULL DEFAULT '{}'
) STRICT;

CREATE TABLE repo (
    id               TEXT PRIMARY KEY,
    url              TEXT NOT NULL,
    default_branch   TEXT NOT NULL,
    gate_config_json TEXT NOT NULL DEFAULT '{}'
) STRICT;

CREATE TABLE workflow (
    id   TEXT PRIMARY KEY,
    name TEXT NOT NULL
) STRICT;

CREATE TABLE workflow_version (
    workflow_id TEXT NOT NULL REFERENCES workflow (id),
    version     INTEGER NOT NULL CHECK (version >= 1),
    source_yaml TEXT NOT NULL,
    schema_hash TEXT NOT NULL,
    PRIMARY KEY (workflow_id, version)
) STRICT;

CREATE TABLE task (
    id            TEXT PRIMARY KEY,
    title         TEXT NOT NULL,
    body          TEXT NOT NULL,
    workflow_id   TEXT NOT NULL REFERENCES workflow (id),
    priority      INTEGER NOT NULL DEFAULT 0,
    status        TEXT NOT NULL,
    repo_ids_json TEXT NOT NULL DEFAULT '[]'
) STRICT;
CREATE INDEX task_by_status ON task (status, priority DESC, id);

CREATE TABLE run (
    id               TEXT PRIMARY KEY,
    task_id          TEXT NOT NULL REFERENCES task (id),
    workflow_id      TEXT NOT NULL,
    workflow_version INTEGER NOT NULL,
    status           TEXT NOT NULL,
    end_reason       TEXT,
    budget_json      TEXT NOT NULL DEFAULT '{}',
    FOREIGN KEY (workflow_id, workflow_version) REFERENCES workflow_version (workflow_id, version)
) STRICT;
CREATE INDEX run_by_status ON run (status);
CREATE INDEX run_by_task ON run (task_id);

CREATE TABLE runtime (
    id      TEXT PRIMARY KEY,
    name    TEXT NOT NULL UNIQUE,
    adapter TEXT NOT NULL,
    source  TEXT NOT NULL
) STRICT;

CREATE TABLE runtime_state (
    runtime_id     TEXT PRIMARY KEY REFERENCES runtime (id),
    status         TEXT NOT NULL,
    cooldown_until TEXT
) STRICT;

CREATE TABLE host (
    id                TEXT PRIMARY KEY,
    name              TEXT NOT NULL,
    capabilities_json TEXT NOT NULL DEFAULT '{}'
) STRICT;

CREATE TABLE session (
    id          TEXT PRIMARY KEY,
    host_id     TEXT NOT NULL REFERENCES host (id),
    worktree    TEXT NOT NULL,
    pid         INTEGER,
    rss_bytes   INTEGER,
    cpu_percent REAL
) STRICT;

CREATE TABLE step (
    id         TEXT PRIMARY KEY,
    run_id     TEXT NOT NULL REFERENCES run (id),
    step_key   TEXT NOT NULL,
    attempt    INTEGER NOT NULL CHECK (attempt >= 1),
    status     TEXT NOT NULL,
    runtime_id TEXT REFERENCES runtime (id),
    session_id TEXT REFERENCES session (id),
    started_at TEXT,
    ended_at   TEXT,
    UNIQUE (run_id, step_key, attempt)
) STRICT;
CREATE INDEX step_by_status ON step (status);

-- The ledger. Append-only; `seq` is assigned here and, with one writer and
-- AUTOINCREMENT rolled back with its transaction, has no gaps.
-- Host events carry (session_id, host_seq): the unique index makes host
-- redelivery after a reconnect a no-op (exactly-once, TRD §7).
CREATE TABLE event (
    seq          INTEGER PRIMARY KEY AUTOINCREMENT,
    run_id       TEXT NOT NULL REFERENCES run (id),
    step_key     TEXT,
    kind         TEXT NOT NULL,
    payload_json TEXT NOT NULL,
    blob         TEXT,
    at           TEXT NOT NULL,
    session_id   TEXT,
    host_seq     INTEGER,
    CHECK ((session_id IS NULL) = (host_seq IS NULL))
) STRICT;
CREATE INDEX event_by_run ON event (run_id, seq);
CREATE UNIQUE INDEX event_host_seq ON event (session_id, host_seq) WHERE session_id IS NOT NULL;
-- Rows are never changed. Deleting stays allowed for retention compaction (§5).
CREATE TRIGGER event_is_append_only BEFORE UPDATE ON event
BEGIN
    SELECT RAISE(ABORT, 'event is append-only');
END;

CREATE TABLE outbox (
    id           TEXT PRIMARY KEY,
    run_id       TEXT NOT NULL REFERENCES run (id),
    idem_key     TEXT NOT NULL UNIQUE,
    target       TEXT NOT NULL,
    payload_json TEXT NOT NULL,
    status       TEXT NOT NULL,
    result_json  TEXT
) STRICT;
CREATE INDEX outbox_by_status ON outbox (status, id);

CREATE TABLE handoff_pack (
    id           TEXT PRIMARY KEY,
    run_id       TEXT NOT NULL REFERENCES run (id),
    from_session TEXT NOT NULL REFERENCES session (id),
    to_session   TEXT REFERENCES session (id),
    pack_hash    TEXT NOT NULL
) STRICT;

CREATE TABLE context_pack (
    id             TEXT PRIMARY KEY,
    step_id        TEXT NOT NULL REFERENCES step (id),
    pack_hash      TEXT NOT NULL,
    token_estimate INTEGER NOT NULL,
    items_json     TEXT NOT NULL DEFAULT '[]'
) STRICT;

CREATE TABLE note (
    id            TEXT PRIMARY KEY,
    run_id        TEXT NOT NULL REFERENCES run (id),
    step_key      TEXT NOT NULL,
    kind          TEXT NOT NULL,
    body          TEXT NOT NULL,
    superseded_by TEXT REFERENCES note (id)
) STRICT;
CREATE INDEX note_by_run ON note (run_id);

CREATE TABLE artifact (
    id      TEXT PRIMARY KEY,
    task_id TEXT NOT NULL REFERENCES task (id),
    version INTEGER NOT NULL CHECK (version >= 1),
    blob    TEXT NOT NULL,
    mime    TEXT NOT NULL,
    UNIQUE (task_id, version)
) STRICT;

CREATE TABLE mod_install (
    name                     TEXT PRIMARY KEY,
    kind                     TEXT NOT NULL,
    version                  TEXT NOT NULL,
    manifest_json            TEXT NOT NULL,
    granted_permissions_json TEXT NOT NULL DEFAULT '[]'
) STRICT;
