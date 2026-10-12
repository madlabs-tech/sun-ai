-- B1b: each board has its own repos and knowledge sources. Links, not a
-- column: a repo or source imported into another board is the same row (one
-- clone, one index). Every existing repo and source goes on Main.

CREATE TABLE board_repo (
    board_id TEXT NOT NULL REFERENCES board (id) ON DELETE CASCADE,
    repo_id  TEXT NOT NULL REFERENCES repo (id) ON DELETE CASCADE,
    PRIMARY KEY (board_id, repo_id)
) STRICT;
CREATE INDEX board_repo_by_repo ON board_repo (repo_id);

CREATE TABLE board_knowledge_source (
    board_id  TEXT NOT NULL REFERENCES board (id) ON DELETE CASCADE,
    source_id TEXT NOT NULL REFERENCES knowledge_source (id) ON DELETE CASCADE,
    PRIMARY KEY (board_id, source_id)
) STRICT;
CREATE INDEX board_knowledge_source_by_source ON board_knowledge_source (source_id);

INSERT INTO board_repo (board_id, repo_id)
SELECT '00000000000000000000000001', id FROM repo;
INSERT INTO board_knowledge_source (board_id, source_id)
SELECT '00000000000000000000000001', id FROM knowledge_source;
