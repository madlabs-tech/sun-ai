-- Long-term memory (L3, F20, TRD §12.9): proposals from repeated or starred
-- corrections and learning notes, reviewed by a human before they become
-- active. `scope_id` is the repo or workflow id (NULL: the workspace).
-- `dedupe_key` makes a repeated trigger (the same cause again, a second
-- star) a no-op. `pr_key` is the idem key of the outbox row that opens a
-- repo-scope PR, set once its branch is pushed.
-- No foreign keys: this version sorts before m1_core (see correction).
CREATE TABLE memory_item (
    id          TEXT PRIMARY KEY,
    scope       TEXT NOT NULL,
    scope_id    TEXT,
    body        TEXT NOT NULL,
    reason      TEXT NOT NULL,
    source_run  TEXT,
    status      TEXT NOT NULL,
    author      TEXT NOT NULL,
    reviewed_by TEXT,
    created_at  TEXT NOT NULL,
    dedupe_key  TEXT UNIQUE,
    pr_key      TEXT
) STRICT;
CREATE INDEX memory_item_by_status ON memory_item (status, created_at);
