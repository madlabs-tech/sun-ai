-- Eval replay (F20, US-20.4): saved sets of past tasks, and replays of them
-- against a candidate instruction / memory / workflow version with their report.
CREATE TABLE eval_set (
    id            TEXT PRIMARY KEY,
    name          TEXT NOT NULL,
    task_ids_json TEXT NOT NULL,
    created_at    TEXT NOT NULL
) STRICT;

CREATE TABLE eval_run (
    id          TEXT PRIMARY KEY,
    name        TEXT NOT NULL,
    spec_json   TEXT NOT NULL,
    status      TEXT NOT NULL,
    report_json TEXT,
    error       TEXT,
    created_at  TEXT NOT NULL,
    finished_at TEXT
) STRICT;
