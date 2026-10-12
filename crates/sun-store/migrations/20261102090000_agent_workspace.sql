-- M4: an agent belongs to one workspace, and its name is unique among that
-- workspace's active agents (ignoring case). Every existing agent goes to
-- Main; the next migration copies an agent into each other workspace whose
-- tasks or system workflows use it. No foreign key: SQLite can't add one
-- with a default, and deleting a workspace deletes its agents.
ALTER TABLE agent ADD COLUMN workspace_id TEXT NOT NULL DEFAULT '00000000000000000000000001';
DROP INDEX agent_by_name;
CREATE UNIQUE INDEX agent_by_name ON agent (workspace_id, lower(name)) WHERE archived_at IS NULL;
