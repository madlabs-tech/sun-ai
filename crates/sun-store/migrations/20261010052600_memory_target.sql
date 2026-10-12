-- The repo file a memory proposal's PR edits (F20, US-20.2): AGENTS.md,
-- CLAUDE.md, a skill's SKILL.md or a workflow prompt; NULL: the repo's agent
-- instructions file.
ALTER TABLE memory_item ADD COLUMN target TEXT;
