-- M5: every catalog install from before is Main's. They are listed in a
-- file (`<data>/catalog/installed.json`, id -> version), not a table, so this
-- is the store's data step (`sun_store::workspace::seed_main_catalog_installs`),
-- run in this migration's transaction; nothing is left for SQL to do.
SELECT 1;
