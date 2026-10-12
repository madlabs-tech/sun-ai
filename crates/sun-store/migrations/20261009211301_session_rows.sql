-- Sessions get rows (1.12b): `step.session_id` and `handoff_pack` point at them.
-- Rebuilt without the `host` FK (a host is revoked by deleting its row, and
-- its old sessions stay history), with `worktree` known only once the host
-- prepared it and the runtime's `native_id` for resume (TRD §4.3 #6).
-- The old table was never written, so nothing is copied.
CREATE TABLE session_new (
    id          TEXT PRIMARY KEY,
    host_id     TEXT NOT NULL,
    worktree    TEXT,
    native_id   TEXT,
    pid         INTEGER,
    rss_bytes   INTEGER,
    cpu_percent REAL
) STRICT;
DROP TABLE session;
ALTER TABLE session_new RENAME TO session;
