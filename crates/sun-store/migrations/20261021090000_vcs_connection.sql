-- T26: git connections per workspace (a GitHub App installation, or a
-- GitLab account through OAuth or an access token) and the repos added
-- through them. Tokens and webhook secrets live in `secret` (scope
-- `workspace`); a connection holds only their names.

CREATE TABLE vcs_connection (
    id               TEXT PRIMARY KEY,
    workspace_id     TEXT NOT NULL REFERENCES workspace (id) ON DELETE CASCADE,
    provider         TEXT NOT NULL CHECK (provider IN ('github', 'gitlab')),
    -- `https://github.com`, `https://gitlab.com` or a self-hosted GitLab.
    instance_url     TEXT NOT NULL,
    account_login    TEXT NOT NULL,
    -- GitHub: the App installation.
    installation_id  INTEGER,
    -- GitLab: the access token's secret name, and for OAuth the refresh
    -- token's and when the access token expires.
    token_secret     TEXT,
    refresh_secret   TEXT,
    token_expires_at TEXT,
    -- GitLab OAuth: the redirect URI the code was issued for; GitLab wants
    -- it again on every refresh.
    oauth_redirect_uri TEXT,
    -- GitLab: the secret every project hook of this connection carries.
    webhook_secret   TEXT,
    connected_by     TEXT NOT NULL,
    created_at       TEXT NOT NULL,
    UNIQUE (workspace_id, provider, instance_url)
) STRICT;
CREATE INDEX vcs_connection_by_installation ON vcs_connection (installation_id);

-- Old rows have no connection (added by URL before T26).
ALTER TABLE repo ADD COLUMN connection_id TEXT REFERENCES vcs_connection (id) ON DELETE SET NULL;
ALTER TABLE repo ADD COLUMN description TEXT NOT NULL DEFAULT '';

-- The GitLab project hook Sun made when the repo was added to the workspace.
ALTER TABLE workspace_repo ADD COLUMN hook_id INTEGER;
