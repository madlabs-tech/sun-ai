-- Design-canvas pins (F25, US-25.2..4): an element of the task's running app
-- with a request for the agent and/or values the user changed themselves.
CREATE TABLE pin (
    id          TEXT PRIMARY KEY,
    task_id     TEXT NOT NULL REFERENCES task (id),
    status      TEXT NOT NULL,
    comment     TEXT NOT NULL,
    target_json TEXT NOT NULL,
    edits_json  TEXT NOT NULL,
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL
) STRICT;
CREATE INDEX pin_by_task ON pin (task_id);
