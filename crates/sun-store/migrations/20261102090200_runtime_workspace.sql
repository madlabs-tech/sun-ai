-- no-transaction
-- M4: runtimes belong to a workspace: `runtime.chain` names resolve among
-- the task's workspace's runtimes, unique per `(workspace_id, name)`, and
-- `created_by` names who added one (NULL: a chain or sun.yaml named it, or
-- it is from before). `runtime` is rebuilt (`runtime_state` and `step`
-- reference it), so foreign keys are off around this file (see
-- 20261022090000_task_lifecycle.sql). Every existing runtime goes to Main,
-- ids kept. A credits runtime's gateway model was a kernel setting keyed by
-- the runtime's name; it is keyed by its id now.
CREATE TABLE runtime_new (
    id           TEXT PRIMARY KEY,
    workspace_id TEXT NOT NULL REFERENCES workspace (id),
    name         TEXT NOT NULL,
    adapter      TEXT NOT NULL,
    source       TEXT NOT NULL,
    config_json  TEXT,
    created_by   TEXT,
    UNIQUE (workspace_id, name)
) STRICT;
INSERT INTO runtime_new (id, workspace_id, name, adapter, source, config_json)
SELECT id, '00000000000000000000000001', name, adapter, source, config_json FROM runtime;
DROP TABLE runtime;
ALTER TABLE runtime_new RENAME TO runtime;

UPDATE kernel_setting
SET key = 'credits_model:' || (SELECT r.id FROM runtime r WHERE 'credits_model:' || r.name = kernel_setting.key)
WHERE key IN (SELECT 'credits_model:' || name FROM runtime);
