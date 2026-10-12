-- T21: boards are workspaces, and the whole installation is the ecosystem.
-- The old `workspace` table (one row per installation, never used) goes first
-- to free the name; then every board table, column, index, trigger and
-- permission key is renamed. Rows, ids (Main keeps
-- `00000000000000000000000001`) and links are unchanged. The ecosystem-wide
-- memory scope is `ecosystem` (it was `workspace`). The old 'workspace'
-- secrets become 'ecosystem' secrets; their AAD still says 'workspace', so
-- they are listed in `secret_to_reseal` and the store re-seals them at open
-- (SQL can't). The append-only audit log keeps `board_change` rows (read as
-- `workspace_change`).

DROP TABLE workspace;

DROP TRIGGER workflow_board_insert;
DROP TRIGGER workflow_board_update;
DROP INDEX workflow_by_board;
DROP INDEX board_by_name;
DROP INDEX board_repo_by_repo;
DROP INDEX board_knowledge_source_by_source;
DROP INDEX board_group_by_group;

ALTER TABLE board RENAME TO workspace;
ALTER TABLE board_repo RENAME TO workspace_repo;
ALTER TABLE board_knowledge_source RENAME TO workspace_knowledge_source;
ALTER TABLE board_group RENAME TO workspace_group;

ALTER TABLE workflow RENAME COLUMN board_id TO workspace_id;
ALTER TABLE workspace_repo RENAME COLUMN board_id TO workspace_id;
ALTER TABLE workspace_knowledge_source RENAME COLUMN board_id TO workspace_id;
ALTER TABLE workspace_group RENAME COLUMN board_id TO workspace_id;

CREATE UNIQUE INDEX workspace_by_name ON workspace (lower(name));
CREATE INDEX workflow_by_workspace ON workflow (workspace_id);
CREATE INDEX workspace_repo_by_repo ON workspace_repo (repo_id);
CREATE INDEX workspace_knowledge_source_by_source ON workspace_knowledge_source (source_id);
CREATE INDEX workspace_group_by_group ON workspace_group (group_id);

CREATE TRIGGER workflow_workspace_insert BEFORE INSERT ON workflow
WHEN NEW.workspace_id IS NULL
BEGIN
    SELECT RAISE(ABORT, 'NOT NULL constraint failed: workflow.workspace_id');
END;

CREATE TRIGGER workflow_workspace_update BEFORE UPDATE OF workspace_id ON workflow
WHEN NEW.workspace_id IS NULL
BEGIN
    SELECT RAISE(ABORT, 'NOT NULL constraint failed: workflow.workspace_id');
END;

-- Permission keys, a plain text REPLACE on the JSON array (as
-- 20261016090000_permission_matrix.sql does).
UPDATE user_group
SET permissions_json = replace(replace(replace(replace(permissions_json,
    '"create_board"', '"create_workspace"'),
    '"edit_board"', '"edit_workspace"'),
    '"delete_board"', '"delete_workspace"'),
    '"access_all_boards"', '"access_all_workspaces"');

UPDATE memory_item SET scope = 'ecosystem' WHERE scope = 'workspace';

-- An import without secret values lists the scopes to re-enter (compact JSON).
UPDATE kernel_setting
SET value_json = replace(value_json, '"kind":"workspace"', '"kind":"ecosystem"')
WHERE key = 'secrets_to_reenter';

-- Nothing references `secret` (its scope is polymorphic), so it is rebuilt in
-- place. 'ecosystem' is the old 'workspace' scope; 'workspace' stays allowed
-- for per-workspace secrets.
CREATE TABLE secret_new (
    id         TEXT PRIMARY KEY,
    scope_kind TEXT NOT NULL CHECK (scope_kind IN ('ecosystem', 'mod', 'workspace')),
    scope_id   TEXT NOT NULL,
    name       TEXT NOT NULL,
    sealed     BLOB NOT NULL,
    updated_at TEXT NOT NULL,
    UNIQUE (scope_kind, scope_id, name)
) STRICT;
INSERT INTO secret_new (id, scope_kind, scope_id, name, sealed, updated_at)
SELECT id, scope_kind, scope_id, name, sealed, updated_at FROM secret;
DROP TABLE secret;
ALTER TABLE secret_new RENAME TO secret;

-- The old 'workspace' rows become 'ecosystem' rows here, but their sealed
-- bytes still carry 'workspace' in the AAD. This table lists exactly them:
-- the store re-seals each listed row at open and deletes its entry, so a
-- 'workspace' row written later (a per-workspace secret) is never touched.
CREATE TABLE secret_to_reseal (
    id TEXT PRIMARY KEY REFERENCES secret (id) ON DELETE CASCADE
) STRICT;
INSERT INTO secret_to_reseal (id) SELECT id FROM secret WHERE scope_kind = 'workspace';
UPDATE secret SET scope_kind = 'ecosystem' WHERE scope_kind = 'workspace';
