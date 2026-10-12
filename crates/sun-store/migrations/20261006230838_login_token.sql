-- Sign-in sessions and one-time login links (1.19), so a restart or upgrade
-- signs nobody out. Only `sha256(token)` is stored, never the token.
-- `member_id` NULL: the local admin (token holder), else an OIDC member (no
-- FK: a member row may come later in migration order, and a stale session of
-- a removed member just fails to resolve). Expired rows are ignored by reads
-- and pruned on insert. `expires_at` is RFC 3339 UTC to the second, so text
-- order is time order.
CREATE TABLE login_token (
    token_hash TEXT PRIMARY KEY,
    kind       TEXT NOT NULL CHECK (kind IN ('session', 'link')),
    member_id  TEXT,
    expires_at TEXT NOT NULL
) STRICT;
CREATE INDEX login_token_expires_at ON login_token (expires_at);
