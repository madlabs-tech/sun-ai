-- Groups v2 (BLO-192): deployment-wide sets of people. Unlike the groups
-- 20261101090000 dropped, they attach no role to a workspace: they grant
-- nothing by themselves and are only a subject of editor grants
-- (`editor_grant.subject_type = 'group'`), host sharing and the terminal
-- policy. Effective membership is the manual rows here plus every member
-- whose verified email has a domain in `domains` (a JSON array of lowercase
-- domains), computed per request.
CREATE TABLE member_group (
    id          TEXT PRIMARY KEY,
    name        TEXT NOT NULL UNIQUE COLLATE NOCASE,
    description TEXT NOT NULL DEFAULT '',
    domains     TEXT NOT NULL DEFAULT '[]' CHECK (json_valid(domains) AND json_type(domains) = 'array'),
    created_by  TEXT REFERENCES member (id) ON DELETE SET NULL,
    created_at  TEXT NOT NULL
) STRICT;

CREATE TABLE member_group_member (
    group_id   TEXT NOT NULL REFERENCES member_group (id) ON DELETE CASCADE,
    member_id  TEXT NOT NULL REFERENCES member (id) ON DELETE CASCADE,
    added_by   TEXT REFERENCES member (id) ON DELETE SET NULL,
    created_at TEXT NOT NULL,
    PRIMARY KEY (group_id, member_id)
) STRICT;
CREATE INDEX member_group_member_by_member ON member_group_member (member_id);

-- A deleted group's editor grants go with it.
CREATE TRIGGER member_group_deleted AFTER DELETE ON member_group
BEGIN
    DELETE FROM editor_grant WHERE subject_type = 'group' AND subject_id = OLD.id;
END;
