-- T24: agents, shared by every workspace of the ecosystem. Values are
-- validated in `sun-core` (`Agent::clean_*`); names are unique among
-- active agents ignoring case (an archived agent frees its name).
-- `runtime_chain` and `skill_ids` are JSON arrays of names.
CREATE TABLE agent (
    id            TEXT PRIMARY KEY,
    name          TEXT NOT NULL,
    description   TEXT NOT NULL DEFAULT '',
    instructions  TEXT NOT NULL DEFAULT '',
    runtime_chain TEXT NOT NULL DEFAULT '[]',
    skill_ids     TEXT NOT NULL DEFAULT '[]',
    created_at    TEXT NOT NULL,
    archived_at   TEXT
) STRICT;
CREATE UNIQUE INDEX agent_by_name ON agent (lower(name)) WHERE archived_at IS NULL;

-- A task assigned to an agent runs the agent's hidden system workflow on
-- the task's workspace: one per (agent, workspace), marked here and left
-- out of every workflow list.
ALTER TABLE workflow ADD COLUMN system_agent_id TEXT REFERENCES agent (id);
CREATE UNIQUE INDEX workflow_by_system_agent ON workflow (system_agent_id, workspace_id)
    WHERE system_agent_id IS NOT NULL;

-- The foreign key T23 left for `task.assignee_agent_id`. `task` was just
-- rebuilt, so triggers enforce it instead of another rebuild.
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
