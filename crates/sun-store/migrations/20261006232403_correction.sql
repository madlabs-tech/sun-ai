-- Corrections of agent work (F20, TRD §12.9): gate fails, rejects, reverts,
-- contradicting steers and overridden decisions, labelled by cause.
-- No foreign keys: this version sorts before m1_core (it was created later),
-- so the tables it would reference may not exist yet on a fresh database.
-- `dedupe_key` makes a redelivered capture (a webhook retry) a no-op.
CREATE TABLE correction (
    id            TEXT PRIMARY KEY,
    task_id       TEXT NOT NULL,
    run_id        TEXT NOT NULL,
    workflow_id   TEXT NOT NULL,
    step_key      TEXT,
    runtime       TEXT,
    kind          TEXT NOT NULL,
    cause_label   TEXT NOT NULL,
    classified_by TEXT NOT NULL,
    body          TEXT NOT NULL,
    created_at    TEXT NOT NULL,
    dedupe_key    TEXT UNIQUE
) STRICT;
CREATE INDEX correction_by_task ON correction (task_id, created_at);
CREATE INDEX correction_by_time ON correction (created_at);
