-- L4 knowledge (TRD §5, §12.7): sources, doc versions, heading-aware chunks
-- and their FTS5 indexes. Never edit once applied.
--
-- A doc's chunks are replaced as a whole when its content hash changes; chunk
-- rows are never updated, so the FTS triggers cover insert and delete only.

CREATE TABLE knowledge_source (
    id          TEXT PRIMARY KEY,
    kind        TEXT NOT NULL,
    name        TEXT NOT NULL,
    config_json TEXT NOT NULL DEFAULT '{}'
) STRICT;

CREATE TABLE doc (
    id           TEXT PRIMARY KEY,
    source_id    TEXT NOT NULL REFERENCES knowledge_source (id) ON DELETE CASCADE,
    external_id  TEXT NOT NULL,
    title        TEXT NOT NULL,
    version      TEXT NOT NULL,
    content_hash TEXT NOT NULL,
    url          TEXT,
    acl_json     TEXT NOT NULL DEFAULT '[]',
    updated_at   TEXT,
    fetched_at   TEXT NOT NULL,
    UNIQUE (source_id, external_id)
) STRICT;

CREATE TABLE chunk (
    id             INTEGER PRIMARY KEY,
    doc_id         TEXT NOT NULL REFERENCES doc (id) ON DELETE CASCADE,
    ord            INTEGER NOT NULL,
    heading_path   TEXT NOT NULL,
    body           TEXT NOT NULL,
    token_estimate INTEGER NOT NULL,
    UNIQUE (doc_id, ord)
) STRICT;

-- Two external-content indexes over `chunk`: words (unicode61, diacritics
-- folded) and substrings (trigram) for Bahasa Indonesia affixes and
-- mixed-language text. Results are fused by the caller.
CREATE VIRTUAL TABLE chunk_fts USING fts5 (
    heading_path, body,
    content = 'chunk', content_rowid = 'id',
    tokenize = 'unicode61 remove_diacritics 2'
);

CREATE VIRTUAL TABLE chunk_fts_trigram USING fts5 (
    heading_path, body,
    content = 'chunk', content_rowid = 'id',
    tokenize = 'trigram'
);

CREATE TRIGGER chunk_fts_insert AFTER INSERT ON chunk BEGIN
    INSERT INTO chunk_fts (rowid, heading_path, body)
        VALUES (new.id, new.heading_path, new.body);
    INSERT INTO chunk_fts_trigram (rowid, heading_path, body)
        VALUES (new.id, new.heading_path, new.body);
END;

CREATE TRIGGER chunk_fts_delete AFTER DELETE ON chunk BEGIN
    INSERT INTO chunk_fts (chunk_fts, rowid, heading_path, body)
        VALUES ('delete', old.id, old.heading_path, old.body);
    INSERT INTO chunk_fts_trigram (chunk_fts_trigram, rowid, heading_path, body)
        VALUES ('delete', old.id, old.heading_path, old.body);
END;

CREATE TRIGGER chunk_no_update BEFORE UPDATE ON chunk BEGIN
    SELECT RAISE (ABORT, 'chunk rows are immutable; replace the doc instead');
END;
