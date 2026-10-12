-- Deleted-doc tombstones (US-18.4): a connector reports a doc gone, its
-- chunks (and so its FTS rows and vectors) are deleted and the doc row stays
-- with `deleted_at`, so `doc:<id>` pointers keep resolving to "deleted at
-- source". Ingesting the doc again revives the row.
ALTER TABLE doc ADD COLUMN deleted_at TEXT;
