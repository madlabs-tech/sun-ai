-- Sun Editor: editor capabilities granted in a workspace (`editor.read`,
-- `editor.write`, `terminal`), to every member (`all`, no `subject_id`), a
-- role (`role`, the role key) or one member (`member`, the member id), on
-- every repo of the workspace (`repo_id` NULL) or one. `subject_type` is
-- free text so a `group` subject can be added without rewriting this table.
-- Owners hold every capability implicitly: never stored here.
CREATE TABLE editor_grant (
    id           TEXT PRIMARY KEY,
    workspace_id TEXT NOT NULL REFERENCES workspace (id) ON DELETE CASCADE,
    repo_id      TEXT REFERENCES repo (id) ON DELETE CASCADE,
    subject_type TEXT NOT NULL,
    subject_id   TEXT,
    capability   TEXT NOT NULL CHECK (capability IN ('editor.read', 'editor.write', 'terminal')),
    granted_by   TEXT REFERENCES member (id) ON DELETE SET NULL,
    created_at   TEXT NOT NULL
) STRICT;
CREATE UNIQUE INDEX editor_grant_once ON editor_grant (
    workspace_id, COALESCE(repo_id, ''), subject_type, COALESCE(subject_id, ''), capability
);

-- A member who leaves a workspace loses their own grants there, so joining
-- again doesn't bring them back.
CREATE TRIGGER editor_grant_member_left AFTER DELETE ON workspace_member
BEGIN
    DELETE FROM editor_grant
    WHERE workspace_id = OLD.workspace_id
      AND subject_type = 'member'
      AND subject_id = OLD.member_id;
END;
