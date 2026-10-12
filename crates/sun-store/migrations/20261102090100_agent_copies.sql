-- M4: an agent used on a workspace other than its own (Main, after the last
-- migration) by a task or a system workflow gets a copy there: a new id,
-- the same fields (archived too), and that workspace's tasks and system
-- workflow point at the copy. SQL can't make ULIDs, so this is the store's
-- data step (`sun_store::agent::copy_agents_into_their_workspaces`), run in
-- this migration's transaction; nothing is left for SQL to do.
SELECT 1;
