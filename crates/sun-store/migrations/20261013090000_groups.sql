-- B3a: groups replace the fixed roles. A group is a role an admin can
-- rename and customize; `key` is the stable slug approval `who: role:<key>`
-- matches, and it never changes on rename. A member may be in several.
--
-- Viewer, Member and Admin are seeded (fixed ids 1-3) from the stored
-- `role_permissions` map, or the old defaults. `run` used to cover workflow
-- editing, so a group with `run` also gets create_workflow and
-- edit_workflow; Admin also gets what the admin role implied (manage_access
-- and the board permissions). Every member joins each group their old role
-- covered (an admin joins viewer, member and admin), so `who: [role:member]`
-- still admits admins. `owner` becomes `member.is_owner`; then `member.role`
-- goes. `member` is rebuilt (SQLite can't drop a CHECKed column in place;
-- nothing referenced it by FK until now).
CREATE TABLE user_group (
    id               TEXT PRIMARY KEY,
    key              TEXT NOT NULL UNIQUE,
    name             TEXT NOT NULL,
    permissions_json TEXT NOT NULL DEFAULT '[]'
) STRICT;
CREATE UNIQUE INDEX user_group_name ON user_group (lower(name));

INSERT INTO user_group (id, key, name, permissions_json)
SELECT '00000000000000000000000001', 'viewer', 'Viewer',
       coalesce((SELECT json_extract(value_json, '$.viewer') FROM kernel_setting
                 WHERE key = 'role_permissions'), '[]');
INSERT INTO user_group (id, key, name, permissions_json)
SELECT '00000000000000000000000002', 'member', 'Member',
       coalesce((SELECT json_extract(value_json, '$.member') FROM kernel_setting
                 WHERE key = 'role_permissions'), '["run","approve"]');
INSERT INTO user_group (id, key, name, permissions_json)
SELECT '00000000000000000000000003', 'admin', 'Admin',
       coalesce((SELECT json_extract(value_json, '$.admin') FROM kernel_setting
                 WHERE key = 'role_permissions'),
                '["run","approve","manage_plugins","manage_extensions","manage_secrets","manage_runtimes"]');
UPDATE user_group
SET permissions_json = json_insert(permissions_json, '$[#]', 'create_workflow', '$[#]', 'edit_workflow')
WHERE EXISTS (SELECT 1 FROM json_each(user_group.permissions_json) WHERE value = 'run');
UPDATE user_group
SET permissions_json = json_insert(permissions_json, '$[#]', 'manage_access', '$[#]', 'create_board',
                                   '$[#]', 'edit_board', '$[#]', 'access_all_boards')
WHERE key = 'admin';
DELETE FROM kernel_setting WHERE key = 'role_permissions';

CREATE TABLE member_new (
    id                TEXT PRIMARY KEY,
    issuer            TEXT NOT NULL,
    subject           TEXT NOT NULL,
    username          TEXT NOT NULL,
    email             TEXT NOT NULL,
    password_hash     TEXT,
    must_set_password INTEGER NOT NULL DEFAULT 0,
    is_owner          INTEGER NOT NULL DEFAULT 0,
    UNIQUE (issuer, subject)
) STRICT;
INSERT INTO member_new (id, issuer, subject, username, email, password_hash, must_set_password,
                        is_owner)
SELECT id, issuer, subject, username, email, password_hash, must_set_password,
       CASE role WHEN 'owner' THEN 1 ELSE 0 END
FROM member;

CREATE TABLE member_group (
    member_id TEXT NOT NULL REFERENCES member_new (id) ON DELETE CASCADE,
    group_id  TEXT NOT NULL REFERENCES user_group (id) ON DELETE CASCADE,
    PRIMARY KEY (member_id, group_id)
) STRICT;
CREATE INDEX member_group_by_group ON member_group (group_id);
INSERT INTO member_group (member_id, group_id)
SELECT m.id, g.id FROM member m JOIN user_group g
  ON g.key = 'viewer'
  OR (g.key = 'member' AND m.role IN ('member', 'admin', 'owner'))
  OR (g.key = 'admin' AND m.role IN ('admin', 'owner'));

DROP TABLE member;
ALTER TABLE member_new RENAME TO member;
CREATE UNIQUE INDEX member_email ON member (lower(email)) WHERE email <> '';
