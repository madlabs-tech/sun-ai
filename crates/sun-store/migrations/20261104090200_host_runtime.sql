-- `host_runtime`: one row per (host, tool) a host reported in its `hello`
-- or `capabilities` frame. A projection of `host.capabilities_json` (the raw
-- report stays the source of truth), rebuilt from every report by the
-- store's `HostRepo`. A tool missing from a later report gets `removed_at`;
-- it comes back (same row, same id) with `removed_at` cleared.
CREATE TABLE host_runtime (
    id           TEXT PRIMARY KEY,
    host_id      TEXT NOT NULL REFERENCES host (id),
    tool         TEXT NOT NULL,           -- adapter / profile id
    family       TEXT,
    version      TEXT,
    installed    INTEGER NOT NULL,
    auth         TEXT NOT NULL,           -- logged_in | logged_out | unknown
    detail       TEXT,
    max_sessions INTEGER,
    probed_at    TEXT NOT NULL,
    removed_at   TEXT,                    -- tool gone from the host; row kept for history
    UNIQUE (host_id, tool)
) STRICT;
