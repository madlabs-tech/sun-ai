-- Pinned constraints of a task (F15, US-15.3): a JSON array of strings that
-- every context pack's P0 carries, so they survive context rotation and
-- hand-offs. NULL = none. Never edit once applied.
ALTER TABLE task_context ADD COLUMN pinned_json TEXT;
