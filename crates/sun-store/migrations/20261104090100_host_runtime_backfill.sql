-- Fills `host_runtime` from every host's current `capabilities_json`, so the
-- table is complete before any host reconnects. SQL can't make ULIDs, so
-- this is the store's data step (`sun_store::host::backfill_host_runtimes`),
-- run in this migration's transaction; nothing is left for SQL to do.
SELECT 1;
