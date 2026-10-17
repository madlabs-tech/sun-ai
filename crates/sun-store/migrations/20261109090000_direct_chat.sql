-- BLO-162: a direct chat (a person talking to a tool, no agent) runs its own
-- hidden one-step workflow, marked here and left out of every workflow list
-- like an agent's system workflow. The store's inserts never set both this
-- and `system_agent_id`.
ALTER TABLE workflow ADD COLUMN direct_chat INTEGER NOT NULL DEFAULT 0;
