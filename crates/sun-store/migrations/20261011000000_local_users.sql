-- B2 invite-only users. Every member has an email (lowercase, unique ignoring
-- case). Members from before B2 without one get '' and are asked for it on
-- their next sign-in (the API only lets them set it); of members sharing an
-- email, the oldest keeps it and the others are asked too. `password_hash` is
-- an Argon2id PHC string (never the password); `must_set_password` = 1 after
-- an invite-code sign-in until the member picks a password. SQLite can't add
-- NOT NULL to a column, so `member` is rebuilt (nothing references it by FK).
CREATE TABLE member_new (
    id                TEXT PRIMARY KEY,
    issuer            TEXT NOT NULL,
    subject           TEXT NOT NULL,
    username          TEXT NOT NULL,
    email             TEXT NOT NULL,
    role              TEXT NOT NULL CHECK (role IN ('viewer', 'member', 'admin', 'owner')),
    password_hash     TEXT,
    must_set_password INTEGER NOT NULL DEFAULT 0,
    UNIQUE (issuer, subject)
) STRICT;
INSERT INTO member_new (id, issuer, subject, username, email, role)
SELECT id, issuer, subject, username, lower(trim(coalesce(email, ''))), role FROM member;
UPDATE member_new SET email = ''
WHERE email <> '' AND id NOT IN (SELECT min(id) FROM member_new WHERE email <> '' GROUP BY email);
DROP TABLE member;
ALTER TABLE member_new RENAME TO member;
CREATE UNIQUE INDEX member_email ON member (lower(email)) WHERE email <> '';

-- `login_token` gets kind `otp`: an invite code, stored as
-- sha256(email ':' code), with its failed `attempts` so far.
CREATE TABLE login_token_new (
    token_hash TEXT PRIMARY KEY,
    kind       TEXT NOT NULL CHECK (kind IN ('session', 'link', 'otp')),
    member_id  TEXT,
    expires_at TEXT NOT NULL,
    attempts   INTEGER NOT NULL DEFAULT 0
) STRICT;
INSERT INTO login_token_new (token_hash, kind, member_id, expires_at)
SELECT token_hash, kind, member_id, expires_at FROM login_token;
DROP TABLE login_token;
ALTER TABLE login_token_new RENAME TO login_token;
CREATE INDEX login_token_expires_at ON login_token (expires_at);
