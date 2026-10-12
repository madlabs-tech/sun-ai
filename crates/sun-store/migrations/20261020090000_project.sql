-- T25: projects, named groups of one workspace's tasks (T23 adds
-- `task.project_id`). Names are unique per workspace ignoring case; values
-- are validated in `sun-core` (`Project::clean_*`). A workspace can only be
-- deleted without workflows, so its projects have no tasks and go with it.

CREATE TABLE project (
    id           TEXT PRIMARY KEY,
    workspace_id TEXT NOT NULL REFERENCES workspace (id) ON DELETE CASCADE,
    name         TEXT NOT NULL,
    description  TEXT NOT NULL DEFAULT '',
    created_at   TEXT NOT NULL
) STRICT;
CREATE UNIQUE INDEX project_by_name ON project (workspace_id, lower(name));
