-- B4c 2-step sign-in. `member_factor` holds a member's second steps: an
-- authenticator (`totp`, its secret sealed with the master key, AAD
-- `member_factor \0 member_id \0 id`), an emailed code (`email`, no secret)
-- or a passkey (B4d, its credential in `data`). `last_step` is the last
-- RFC 6238 time step used, so a code works once; a `totp` row without one is
-- an authenticator still being added (not yet confirmed with a code). No FK:
-- members are deactivated, never deleted.
CREATE TABLE member_factor (
    id            TEXT PRIMARY KEY,
    member_id     TEXT NOT NULL,
    kind          TEXT NOT NULL CHECK (kind IN ('totp', 'email', 'passkey')),
    label         TEXT NOT NULL,
    secret_sealed BLOB,
    data          TEXT NOT NULL DEFAULT '',
    last_step     INTEGER,
    created_at    TEXT NOT NULL
) STRICT;
CREATE INDEX member_factor_member ON member_factor (member_id);

-- `login_token` gets kind `two_step`: a 5-minute sign-in challenge after the
-- password (stored as sha256(challenge), with its failed `attempts`), and a
-- `data` column (the hash of a mailed code). B4a removed every `link` row.
CREATE TABLE login_token_new (
    token_hash TEXT PRIMARY KEY,
    kind       TEXT NOT NULL CHECK (kind IN ('session', 'otp', 'two_step')),
    member_id  TEXT,
    expires_at TEXT NOT NULL,
    attempts   INTEGER NOT NULL DEFAULT 0,
    data       TEXT NOT NULL DEFAULT ''
) STRICT;
INSERT INTO login_token_new (token_hash, kind, member_id, expires_at, attempts)
SELECT token_hash, kind, member_id, expires_at, attempts FROM login_token WHERE kind <> 'link';
DROP TABLE login_token;
ALTER TABLE login_token_new RENAME TO login_token;
CREATE INDEX login_token_expires_at ON login_token (expires_at);
