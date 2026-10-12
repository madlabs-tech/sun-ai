-- US-10.2: the `owner` role. SQLite can't alter a CHECK, so the member table
-- is rebuilt (nothing references it by foreign key).

CREATE TABLE member_new (
    id       TEXT PRIMARY KEY,
    issuer   TEXT NOT NULL,
    subject  TEXT NOT NULL,
    username TEXT NOT NULL,
    email    TEXT,
    role     TEXT NOT NULL CHECK (role IN ('viewer', 'member', 'admin', 'owner')),
    UNIQUE (issuer, subject)
) STRICT;

INSERT INTO member_new (id, issuer, subject, username, email, role)
SELECT id, issuer, subject, username, email, role FROM member;

DROP TABLE member;

ALTER TABLE member_new RENAME TO member;
