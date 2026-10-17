-- TRD BLO-117 item 9: the permission mode moves onto the agent. NULL inherits
-- the tool's `runtime_policy` row (dropped in Stage 5), so every existing
-- agent keeps the mode it runs in today.
ALTER TABLE agent ADD COLUMN permission_mode TEXT;
