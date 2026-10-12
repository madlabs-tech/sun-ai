-- M1: invitations into one workspace with a role, sent to an email. The
-- link token is stored as sha256(token) only. At most one pending (neither
-- accepted nor declined) invitation per workspace and email; inviting again
-- replaces it.
CREATE TABLE workspace_invitation (
    id           TEXT PRIMARY KEY,
    workspace_id TEXT NOT NULL REFERENCES workspace (id) ON DELETE CASCADE,
    email        TEXT NOT NULL,
    role         TEXT NOT NULL CHECK (role IN ('owner', 'admin', 'member', 'viewer')),
    inviter_id   TEXT REFERENCES member (id) ON DELETE SET NULL,
    token_hash   TEXT NOT NULL UNIQUE,
    created_at   TEXT NOT NULL,
    expires_at   TEXT NOT NULL,
    accepted_at  TEXT,
    declined_at  TEXT
) STRICT;
CREATE UNIQUE INDEX workspace_invitation_pending ON workspace_invitation (workspace_id, lower(email))
WHERE accepted_at IS NULL AND declined_at IS NULL;
CREATE INDEX workspace_invitation_by_email ON workspace_invitation (lower(email));
