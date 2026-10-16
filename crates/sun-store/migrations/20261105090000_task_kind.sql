-- no-transaction
-- BLO-128: a conversation with an agent is a hidden task. `kind` is `task`
-- (the board's) or `chat`; a chat started by an @mention names the task it
-- came from (`source_task_id`), one per (task, agent). Only a `task` takes a
-- number, so chats leave no gaps in the keys. `task` is rebuilt to make
-- `number` nullable (`run`, `task_activity` and others reference it), so
-- foreign keys are off around this file (see
-- 20261022090000_task_lifecycle.sql) and the agent triggers on and about it
-- are made again.
CREATE TABLE task_new (
    id                        TEXT PRIMARY KEY,
    workspace_id              TEXT NOT NULL REFERENCES workspace (id),
    number                    INTEGER CHECK (number >= 1),
    title                     TEXT NOT NULL,
    body                      TEXT NOT NULL,
    workflow_id               TEXT REFERENCES workflow (id),
    priority                  TEXT NOT NULL DEFAULT 'none'
                              CHECK (priority IN ('urgent', 'high', 'medium', 'low', 'none')),
    status                    TEXT NOT NULL,
    assignee_agent_id         TEXT,
    project_id                TEXT REFERENCES project (id) ON DELETE SET NULL,
    position                  REAL NOT NULL DEFAULT 0,
    created_by                TEXT,
    created_at                TEXT NOT NULL,
    updated_at                TEXT NOT NULL,
    pr_auto_complete_disabled INTEGER NOT NULL DEFAULT 0,
    kind                      TEXT NOT NULL DEFAULT 'task' CHECK (kind IN ('task', 'chat')),
    source_task_id            TEXT REFERENCES task (id),
    UNIQUE (workspace_id, number),
    CHECK ((kind = 'task') = (number IS NOT NULL))
) STRICT;

INSERT INTO task_new (id, workspace_id, number, title, body, workflow_id, priority, status,
                      assignee_agent_id, project_id, position, created_by, created_at,
                      updated_at, pr_auto_complete_disabled)
SELECT id, workspace_id, number, title, body, workflow_id, priority, status,
       assignee_agent_id, project_id, position, created_by, created_at,
       updated_at, pr_auto_complete_disabled
FROM task;

DROP TRIGGER agent_delete_while_assigned;
DROP TABLE task;
ALTER TABLE task_new RENAME TO task;
CREATE INDEX task_by_status ON task (status, id);
CREATE INDEX task_by_workspace ON task (workspace_id, status);
CREATE UNIQUE INDEX task_mention_chat ON task (source_task_id, assignee_agent_id)
    WHERE source_task_id IS NOT NULL;

CREATE TRIGGER task_assignee_agent_insert BEFORE INSERT ON task
WHEN NEW.assignee_agent_id IS NOT NULL
    AND NOT EXISTS (SELECT 1 FROM agent WHERE id = NEW.assignee_agent_id)
BEGIN
    SELECT RAISE(ABORT, 'FOREIGN KEY constraint failed: task.assignee_agent_id');
END;

CREATE TRIGGER task_assignee_agent_update BEFORE UPDATE OF assignee_agent_id ON task
WHEN NEW.assignee_agent_id IS NOT NULL
    AND NOT EXISTS (SELECT 1 FROM agent WHERE id = NEW.assignee_agent_id)
BEGIN
    SELECT RAISE(ABORT, 'FOREIGN KEY constraint failed: task.assignee_agent_id');
END;

CREATE TRIGGER agent_delete_while_assigned BEFORE DELETE ON agent
WHEN EXISTS (SELECT 1 FROM task WHERE assignee_agent_id = OLD.id)
BEGIN
    SELECT RAISE(ABORT, 'FOREIGN KEY constraint failed: task.assignee_agent_id');
END;
