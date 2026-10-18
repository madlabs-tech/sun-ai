-- TRD BLO-117 item 7 (one-way): who pays leaves `runtime`. Every `byo_key`
-- and `credits` runtime row moves to `credential` under the same id and
-- name, so chains naming it, `account_cooldown` (`cred:<id>`) and the
-- credits gateway model (`kernel_setting credits_model:<id>`) keep pointing
-- at it. `default_tool` is the old `adapter`; `family` is the key's provider
-- API (`config_json.kind`, `openai_compatible` when absent) and, for
-- credits, the one the tool speaks. An empty endpoint (the provider's own)
-- is stored as NULL.
CREATE TABLE credential (
    id            TEXT PRIMARY KEY,
    workspace_id  TEXT NOT NULL REFERENCES workspace (id),
    name          TEXT NOT NULL,
    family        TEXT NOT NULL,
    source        TEXT NOT NULL,
    endpoint      TEXT,
    default_model TEXT,
    key_secret    TEXT,
    default_tool  TEXT NOT NULL,
    created_by    TEXT,
    UNIQUE (workspace_id, name)
) STRICT;

INSERT INTO credential (
    id, workspace_id, name, family, source, endpoint, default_model, key_secret,
    default_tool, created_by
)
SELECT id, workspace_id, name,
       CASE
           WHEN source = 'byo_key'
               THEN COALESCE(json_extract(config_json, '$.kind'), 'openai_compatible')
           WHEN adapter = 'claude-code' THEN 'anthropic'
           ELSE 'openai_compatible'
       END,
       source,
       NULLIF(json_extract(config_json, '$.endpoint'), ''),
       json_extract(config_json, '$.model'),
       json_extract(config_json, '$.key_secret'),
       adapter, created_by
FROM runtime
WHERE source IN ('byo_key', 'credits');

-- A step that ran on a key names its credential; `runtime_id` is left for
-- the CLI's own login (the tool rows that stay in `runtime`). Deleting a
-- credential keeps its steps, without the link.
ALTER TABLE step ADD COLUMN credential_id TEXT REFERENCES credential (id) ON DELETE SET NULL;
UPDATE step SET credential_id = runtime_id, runtime_id = NULL
WHERE runtime_id IN (SELECT id FROM credential);

-- Their old states were folded into `account_cooldown` (item 5).
DELETE FROM runtime_state WHERE runtime_id IN (SELECT id FROM credential);
DELETE FROM runtime WHERE id IN (SELECT id FROM credential);
