-- Unused host enrolment tokens, so a kernel restart keeps the ones the UI
-- just showed. Only the token's hash is stored (hex sha256). A used token's
-- row is deleted; an expired one stays a day (`HostRepo::insert_host_token`
-- prunes), so the kernel can say "expired" rather than "unknown".
-- `expires_at` is RFC 3339 to the second, so the text sorts as time.
CREATE TABLE host_token (
    token_hash  TEXT PRIMARY KEY,
    enrols_json TEXT NOT NULL,
    expires_at  TEXT NOT NULL
) STRICT;
