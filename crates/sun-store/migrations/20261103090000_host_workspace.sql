-- M5: hosts stay deployment rows, attached to workspaces through
-- `workspace_host` (the `WorkspaceItem` link pattern): a workspace's runs go
-- only to the hosts attached to it. `enrolled_by` names who minted the token
-- a host joined with (NULL: from before, the kernel's own host, or a leased
-- seller host). Every existing host is attached to Main, except a leased
-- seller host (`kernel_setting` `host_market.<id>`), which serves only its
-- task.
ALTER TABLE host ADD COLUMN enrolled_by TEXT;
CREATE TABLE workspace_host (
    workspace_id TEXT NOT NULL REFERENCES workspace (id) ON DELETE CASCADE,
    host_id      TEXT NOT NULL REFERENCES host (id) ON DELETE CASCADE,
    PRIMARY KEY (workspace_id, host_id)
) STRICT;
CREATE INDEX workspace_host_by_host ON workspace_host (host_id);
INSERT INTO workspace_host (workspace_id, host_id)
SELECT '00000000000000000000000001', id FROM host
WHERE 'host_market.' || id NOT IN (SELECT key FROM kernel_setting);
