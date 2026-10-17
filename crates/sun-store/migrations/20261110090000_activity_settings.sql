-- BLO-162: the activity log gets the `settings` kind (a chat without an
-- agent switched model, effort or mode). SQLite can't alter a CHECK, so the
-- table is rebuilt as in 20261025090000 (nothing references it by foreign
-- key).
CREATE TABLE task_activity_new (
    id              TEXT PRIMARY KEY,
    task_id         TEXT NOT NULL REFERENCES task (id),
    seq             INTEGER NOT NULL CHECK (seq >= 1),
    kind            TEXT NOT NULL CHECK (kind IN ('comment', 'status', 'priority', 'assignee',
                        'project', 'title', 'description', 'created', 'run', 'pull_request',
                        'settings')),
    actor_member_id TEXT,
    body_json       TEXT NOT NULL,
    created_at      TEXT NOT NULL,
    UNIQUE (task_id, seq)
) STRICT;
INSERT INTO task_activity_new (id, task_id, seq, kind, actor_member_id, body_json, created_at)
SELECT id, task_id, seq, kind, actor_member_id, body_json, created_at FROM task_activity;
DROP TABLE task_activity;
ALTER TABLE task_activity_new RENAME TO task_activity;

CREATE TRIGGER task_activity_is_append_only BEFORE UPDATE ON task_activity
BEGIN
    SELECT RAISE(ABORT, 'task_activity is append-only');
END;
