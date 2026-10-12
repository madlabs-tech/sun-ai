-- An artifact's file name (e.g. `brief.md`), so one task can version several
-- outputs. Versions stay numbered per task (`UNIQUE (task_id, version)`).
ALTER TABLE artifact ADD COLUMN name TEXT NOT NULL DEFAULT '';
