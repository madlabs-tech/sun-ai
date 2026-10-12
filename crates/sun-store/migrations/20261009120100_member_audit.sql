-- F10 members and the audit log (TRD §5, §15). One workspace per install, so
-- a member's role is a column here; a `role_binding` table comes with
-- multi-tenant workspaces (Sun Cloud).

CREATE TABLE member (
    id       TEXT PRIMARY KEY,
    issuer   TEXT NOT NULL,
    subject  TEXT NOT NULL,
    username TEXT NOT NULL,
    email    TEXT,
    role     TEXT NOT NULL CHECK (role IN ('viewer', 'member', 'admin')),
    UNIQUE (issuer, subject)
) STRICT;

-- Append-only: a trigger blocks UPDATE and DELETE.
CREATE TABLE audit (
    id     TEXT PRIMARY KEY,
    actor  TEXT NOT NULL,
    action TEXT NOT NULL,
    target TEXT NOT NULL,
    detail TEXT NOT NULL DEFAULT '',
    at     TEXT NOT NULL
) STRICT;

CREATE TRIGGER audit_no_update BEFORE UPDATE ON audit
BEGIN
    SELECT RAISE(ABORT, 'audit is append-only');
END;

CREATE TRIGGER audit_no_delete BEFORE DELETE ON audit
BEGIN
    SELECT RAISE(ABORT, 'audit is append-only');
END;
