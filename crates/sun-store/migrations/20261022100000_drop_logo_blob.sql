-- T23b: drop what only the store's one-time moves read. The data step for
-- this version (`sun_store` `DATA_STEPS`) runs first, in this migration's
-- transaction: it re-seals the ecosystem secrets `secret_to_reseal` lists
-- (from 20261018090000) and moves each logo `workspace.logo_blob` names
-- from the blob store into `workspace_logo` (from 20261022090000). An
-- install upgrading straight across all three runs them in that order, and
-- one that already applied the earlier two still gets the moves here.
ALTER TABLE workspace DROP COLUMN logo_blob;
DROP TABLE secret_to_reseal;
