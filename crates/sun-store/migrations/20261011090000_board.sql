-- B1: named boards (workspace = board). A board owns its workflows and,
-- through them, their tasks. Board "Main" (a fixed id, `BoardId::MAIN`)
-- takes every existing workflow. The unused `workspace` table stays.

CREATE TABLE board (
    id         TEXT PRIMARY KEY,
    name       TEXT NOT NULL,
    created_at TEXT NOT NULL
) STRICT;
CREATE UNIQUE INDEX board_by_name ON board (lower(name));

INSERT INTO board (id, name, created_at)
VALUES ('00000000000000000000000001', 'Main', strftime('%Y-%m-%dT%H:%M:%S+00:00', 'now'));

-- `workflow` isn't rebuilt (as 20261010033748_member_owner.sql rebuilds
-- `member`): `workflow_version` and `task` reference it, and dropping a
-- referenced table needs foreign keys off, which can't change inside the
-- migration's transaction. So the column is added (SQLite only allows a NULL
-- default for a new foreign key column) and the triggers make it NOT NULL.
ALTER TABLE workflow ADD COLUMN board_id TEXT REFERENCES board (id);
UPDATE workflow SET board_id = '00000000000000000000000001';

CREATE TRIGGER workflow_board_insert BEFORE INSERT ON workflow
WHEN NEW.board_id IS NULL
BEGIN
    SELECT RAISE(ABORT, 'NOT NULL constraint failed: workflow.board_id');
END;

CREATE TRIGGER workflow_board_update BEFORE UPDATE OF board_id ON workflow
WHEN NEW.board_id IS NULL
BEGIN
    SELECT RAISE(ABORT, 'NOT NULL constraint failed: workflow.board_id');
END;

CREATE INDEX workflow_by_board ON workflow (board_id);
