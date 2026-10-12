-- Over-budget mode of a task's context packs (US-18.2): 1 = block a pack
-- that doesn't fit the budget instead of trimming it to pointers. Never edit
-- once applied.
ALTER TABLE task_context ADD COLUMN block_over_budget INTEGER NOT NULL DEFAULT 0;
