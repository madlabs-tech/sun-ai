-- F10 secrets (TRD §15). Values are ChaCha20-Poly1305 sealed with the master
-- key: `sealed` = 12-byte nonce || ciphertext || 16-byte tag, and the AAD is
-- `scope_kind \0 scope_id \0 name`, so a sealed value cannot be moved to
-- another row. Scope is polymorphic (a workspace id or a mod name), hence no FK.

CREATE TABLE secret (
    id         TEXT PRIMARY KEY,
    scope_kind TEXT NOT NULL CHECK (scope_kind IN ('workspace', 'mod')),
    scope_id   TEXT NOT NULL,
    name       TEXT NOT NULL,
    sealed     BLOB NOT NULL,
    updated_at TEXT NOT NULL,
    UNIQUE (scope_kind, scope_id, name)
) STRICT;
