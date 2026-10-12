-- Run-less outbox rows: side effects without a run (a failed trigger's
-- notification) go through the outbox too. SQLite can't drop a NOT NULL, so
-- the table is rebuilt (nothing references it by foreign key).

CREATE TABLE outbox_new (
    id           TEXT PRIMARY KEY,
    run_id       TEXT REFERENCES run (id),
    idem_key     TEXT NOT NULL UNIQUE,
    target       TEXT NOT NULL,
    payload_json TEXT NOT NULL,
    status       TEXT NOT NULL,
    result_json  TEXT
) STRICT;

INSERT INTO outbox_new (id, run_id, idem_key, target, payload_json, status, result_json)
SELECT id, run_id, idem_key, target, payload_json, status, result_json FROM outbox;

DROP TABLE outbox;

ALTER TABLE outbox_new RENAME TO outbox;

CREATE INDEX outbox_by_status ON outbox (status, id);
