-- no-transaction
-- T23: the task lifecycle. `task` is rebuilt (`run`, `artifact`,
-- `task_context`, `task_quality` and `pin` reference it), which needs foreign
-- keys off. sqlx 0.8's SQLite driver would run this file inside its own
-- transaction, where `PRAGMA foreign_keys` is a no-op, so the store applies a
-- `-- no-transaction` migration itself (`sun_store::migrate`): foreign keys
-- off, then this file and its `_sqlx_migrations` row in one transaction,
-- `PRAGMA foreign_key_check` (any violation rolls everything back), commit,
-- foreign keys on. The backup is taken before, as for every migration.
--
-- 1. Issue prefixes become unique in the ecosystem, ignoring case, so a task
--    key (`ENG-12`) names one task anywhere. In id order, a workspace whose
--    prefix an earlier one already has gets the first free digit suffix
--    (`MAIN` → `MAIN2`), cut to 10 characters (`Workspace::prefix_with_suffix`).
WITH RECURSIVE
    base (rn, id, prefix) AS (
        SELECT row_number() OVER (ORDER BY id), id, upper(issue_prefix) FROM workspace
    ),
    n (k) AS (SELECT 1 UNION ALL SELECT k + 1 FROM n WHERE k <= (SELECT count(*) FROM base)),
    candidate (rn, k, prefix) AS (
        SELECT base.rn, n.k, CASE
            WHEN n.k = 1 THEN base.prefix
            ELSE substr(base.prefix, 1, 10 - length(n.k)) || n.k
        END
        FROM base, n
    ),
    given (rn, id, prefix, taken) AS (
        SELECT 0, NULL, NULL, json('[]')
        UNION ALL
        SELECT base.rn, base.id,
            (SELECT c.prefix FROM candidate c
             WHERE c.rn = base.rn AND NOT EXISTS (
                 SELECT 1 FROM json_each(json_insert(given.taken, '$[#]', given.prefix)) t
                 WHERE t.value = c.prefix)
             ORDER BY c.k LIMIT 1),
            json_insert(given.taken, '$[#]', given.prefix)
        FROM given JOIN base ON base.rn = given.rn + 1
    )
UPDATE workspace SET issue_prefix = (SELECT prefix FROM given WHERE given.id = workspace.id);

CREATE UNIQUE INDEX workspace_by_prefix ON workspace (upper(issue_prefix));

-- 2. The new task. `workspace_id` is the task's own (it was its workflow's);
--    `workflow_id` is NULL for a plain task; `number` counts per workspace
--    (`workspace.issue_counter` is the last one given); `priority` is
--    `sun_core::Priority`; `assignee_agent_id` gets its foreign key with the
--    agent table; `created_by` is a member id, NULL for the engine.
CREATE TABLE task_new (
    id                TEXT PRIMARY KEY,
    workspace_id      TEXT NOT NULL REFERENCES workspace (id),
    number            INTEGER NOT NULL CHECK (number >= 1),
    title             TEXT NOT NULL,
    body              TEXT NOT NULL,
    workflow_id       TEXT REFERENCES workflow (id),
    priority          TEXT NOT NULL DEFAULT 'none'
                      CHECK (priority IN ('urgent', 'high', 'medium', 'low', 'none')),
    status            TEXT NOT NULL,
    repo_ids_json     TEXT NOT NULL DEFAULT '[]',
    assignee_agent_id TEXT,
    project_id        TEXT REFERENCES project (id) ON DELETE SET NULL,
    position          REAL NOT NULL DEFAULT 0,
    created_by        TEXT,
    created_at        TEXT NOT NULL,
    updated_at        TEXT NOT NULL,
    UNIQUE (workspace_id, number)
) STRICT;

-- Old rows: the workspace of their workflow (Main if it is somehow gone),
-- numbered per workspace in id (ULID, so creation) order; created when their
-- ULID says (its first 10 Crockford base-32 characters are the milliseconds).
-- Statuses map as the T23 table says. A running task is derived from its
-- latest run's latest step attempts, as the API derived it: waiting approval
-- is needs_you, verifying is verifying, anything else in_progress (a running
-- gate step reads as in_progress until its next move). A task that needed
-- attention is blocked when its latest run is paused, else needs_you.
WITH
    alphabet (a) AS (SELECT '0123456789ABCDEFGHJKMNPQRSTVWXYZ'),
    old AS (
        SELECT t.*, coalesce(w.workspace_id, '00000000000000000000000001') AS ws,
            (SELECT r.id FROM run r WHERE r.task_id = t.id ORDER BY r.id DESC LIMIT 1) AS run_id
        FROM task t LEFT JOIN workflow w ON w.id = t.workflow_id
    ),
    stamped AS (
        SELECT old.*,
            row_number() OVER (PARTITION BY ws ORDER BY id) AS num,
            strftime('%Y-%m-%dT%H:%M:%f+00:00', (
                ((((((((((instr(a, upper(substr(id, 1, 1))) - 1) * 32
                + instr(a, upper(substr(id, 2, 1))) - 1) * 32
                + instr(a, upper(substr(id, 3, 1))) - 1) * 32
                + instr(a, upper(substr(id, 4, 1))) - 1) * 32
                + instr(a, upper(substr(id, 5, 1))) - 1) * 32
                + instr(a, upper(substr(id, 6, 1))) - 1) * 32
                + instr(a, upper(substr(id, 7, 1))) - 1) * 32
                + instr(a, upper(substr(id, 8, 1))) - 1) * 32
                + instr(a, upper(substr(id, 9, 1))) - 1) * 32
                + instr(a, upper(substr(id, 10, 1))) - 1)
            ) / 1000.0, 'unixepoch') AS made
        FROM old, alphabet
    ),
    -- The latest attempt of each step of a run.
    latest_step (run_id, status) AS (
        SELECT s.run_id, s.status FROM step s
        WHERE s.attempt = (SELECT max(attempt) FROM step x
                           WHERE x.run_id = s.run_id AND x.step_key = s.step_key)
    )
INSERT INTO task_new (id, workspace_id, number, title, body, workflow_id, priority, status,
                      repo_ids_json, position, created_at, updated_at)
SELECT id, ws, num, title, body, workflow_id,
    CASE WHEN priority > 0 THEN 'high' WHEN priority < 0 THEN 'low' ELSE 'none' END,
    CASE status
        WHEN 'queued' THEN 'todo'
        WHEN 'running' THEN CASE
            WHEN EXISTS (SELECT 1 FROM latest_step l
                         WHERE l.run_id = stamped.run_id AND l.status = 'waiting_approval')
                THEN 'needs_you'
            WHEN EXISTS (SELECT 1 FROM latest_step l
                         WHERE l.run_id = stamped.run_id AND l.status = 'verifying')
                THEN 'verifying'
            ELSE 'in_progress'
        END
        WHEN 'needs_attention' THEN CASE
            WHEN (SELECT r.status FROM run r WHERE r.id = stamped.run_id) = 'paused'
                THEN 'blocked'
            ELSE 'needs_you'
        END
        ELSE status
    END,
    repo_ids_json, num, made, made
FROM stamped;

DROP TABLE task;
ALTER TABLE task_new RENAME TO task;
CREATE INDEX task_by_status ON task (status, id);
CREATE INDEX task_by_workspace ON task (workspace_id, status);

UPDATE workspace SET issue_counter = max(issue_counter,
    coalesce((SELECT max(number) FROM task WHERE task.workspace_id = workspace.id), 0));

-- 3. The append-only activity log: comments and every change, by a person
--    (`actor_member_id`) or the engine (NULL). `seq` counts per task from 1.
CREATE TABLE task_activity (
    id              TEXT PRIMARY KEY,
    task_id         TEXT NOT NULL REFERENCES task (id),
    seq             INTEGER NOT NULL CHECK (seq >= 1),
    kind            TEXT NOT NULL CHECK (kind IN ('comment', 'status', 'priority', 'assignee',
                        'project', 'title', 'description', 'created', 'run')),
    actor_member_id TEXT,
    body_json       TEXT NOT NULL,
    created_at      TEXT NOT NULL,
    UNIQUE (task_id, seq)
) STRICT;

CREATE TRIGGER task_activity_is_append_only BEFORE UPDATE ON task_activity
BEGIN
    SELECT RAISE(ABORT, 'task_activity is append-only');
END;

-- 4. A workspace logo is its own row, not a shared content-addressed blob:
--    replacing or removing one deletes exactly it, and it goes with its
--    workspace. Logos are at most 1 MB (`Workspace::check_logo`). The old
--    `workspace.logo_blob` files are in the blob store, which SQL can't
--    read: the store moves each into this table at open and clears the
--    column, which nothing reads any more.
CREATE TABLE workspace_logo (
    workspace_id TEXT PRIMARY KEY REFERENCES workspace (id) ON DELETE CASCADE,
    content_type TEXT NOT NULL,
    hash         TEXT NOT NULL,
    bytes        BLOB NOT NULL
) STRICT;
