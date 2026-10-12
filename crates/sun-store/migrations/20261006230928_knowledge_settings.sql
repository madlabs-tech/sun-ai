-- Knowledge wiring (W3): kernel-wide settings (the embedding provider, never
-- its key: that is a secret) and each task's context selection (US-18.2).
-- Never edit once applied.
CREATE TABLE kernel_setting (
    key        TEXT PRIMARY KEY,
    value_json TEXT NOT NULL
) STRICT;

-- `sources_json`: knowledge source ids the task retrieves from (NULL = all).
-- `budget_tokens`: cap on the context pack (NULL = the step's own budget).
CREATE TABLE task_context (
    task_id       TEXT PRIMARY KEY REFERENCES task (id) ON DELETE CASCADE,
    sources_json  TEXT,
    budget_tokens INTEGER CHECK (budget_tokens > 0)
) STRICT;
