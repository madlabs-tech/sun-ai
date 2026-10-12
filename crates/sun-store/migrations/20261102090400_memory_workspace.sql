-- M4: memory belongs to a workspace, and a run gets only its workspace's.
-- An item goes to the workspace of the task its source run worked on, else
-- (no run) its workflow's for `workflow` scope, else Main.
ALTER TABLE memory_item ADD COLUMN workspace_id TEXT NOT NULL DEFAULT '00000000000000000000000001';
UPDATE memory_item SET workspace_id = COALESCE(
    (SELECT t.workspace_id FROM run r JOIN task t ON t.id = r.task_id
     WHERE r.id = memory_item.source_run),
    (SELECT w.workspace_id FROM workflow w
     WHERE memory_item.scope = 'workflow' AND w.id = memory_item.scope_id),
    '00000000000000000000000001'
);
CREATE INDEX memory_item_by_workspace ON memory_item (workspace_id, status, created_at);
