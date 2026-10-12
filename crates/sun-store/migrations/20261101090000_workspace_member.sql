-- M1: workspaces have members with a role (owner, admin, member, viewer)
-- instead of groups attached to workspaces. Each member gets, per
-- workspace, the highest role any of these rules gives them:
--   1. a deployment owner (`member.is_owner`) is `owner` of every workspace;
--   2. a group membership gives the group's role in each workspace the group
--      was attached to (`workspace_group`), and in every workspace when the
--      group had `access_all_workspaces`;
--   3. a group membership also gives that role in Main;
--   4. every member is at least `viewer` of Main.
-- A group's role is its key for the seeded Viewer, Member and Admin; a
-- custom group is `admin` when it had any admin-only permission, `member`
-- when it could run, approve or edit workflows, else `viewer`. Customized
-- permission matrices are not kept: roles grant fixed permissions.
-- `member.email_verified_at` is backfilled (now) for everyone with a
-- password or an identity-provider or token sign-in, and `workspace` gains
-- `created_by` (unknown, NULL, for existing workspaces). Then the group
-- tables go.

CREATE TABLE workspace_member (
    workspace_id TEXT NOT NULL REFERENCES workspace (id) ON DELETE CASCADE,
    member_id    TEXT NOT NULL REFERENCES member (id) ON DELETE CASCADE,
    role         TEXT NOT NULL CHECK (role IN ('owner', 'admin', 'member', 'viewer')),
    created_at   TEXT NOT NULL,
    PRIMARY KEY (workspace_id, member_id)
) STRICT;
CREATE INDEX workspace_member_by_member ON workspace_member (member_id);

WITH group_role (id, rank, every_workspace) AS (
    SELECT id,
           CASE
               WHEN key = 'admin' THEN 3
               WHEN key = 'member' THEN 2
               WHEN key = 'viewer' THEN 1
               WHEN EXISTS (SELECT 1 FROM json_each(user_group.permissions_json)
                            WHERE value IN ('manage_plugins', 'manage_extensions',
                                            'manage_secrets', 'manage_runtimes',
                                            'manage_settings', 'invite_members',
                                            'create_members', 'view_members', 'edit_members',
                                            'deactivate_members', 'manage_groups',
                                            'create_workspace', 'edit_workspace',
                                            'delete_workspace', 'access_all_workspaces'))
                   THEN 3
               WHEN EXISTS (SELECT 1 FROM json_each(user_group.permissions_json)
                            WHERE value IN ('run', 'approve', 'create_workflow',
                                            'edit_workflow'))
                   THEN 2
               ELSE 1
           END,
           EXISTS (SELECT 1 FROM json_each(user_group.permissions_json)
                   WHERE value = 'access_all_workspaces')
    FROM user_group
),
grant_row (workspace_id, member_id, rank) AS (
    SELECT w.id, m.id, 4 FROM member m CROSS JOIN workspace w WHERE m.is_owner = 1
    UNION ALL
    SELECT wg.workspace_id, mg.member_id, g.rank
    FROM member_group mg
    JOIN group_role g ON g.id = mg.group_id
    JOIN workspace_group wg ON wg.group_id = mg.group_id
    UNION ALL
    SELECT w.id, mg.member_id, g.rank
    FROM member_group mg
    JOIN group_role g ON g.id = mg.group_id
    CROSS JOIN workspace w
    WHERE g.every_workspace
    UNION ALL
    SELECT w.id, mg.member_id, g.rank
    FROM member_group mg
    JOIN group_role g ON g.id = mg.group_id
    JOIN workspace w ON w.id = '00000000000000000000000001'
    UNION ALL
    SELECT w.id, m.id, 1
    FROM member m
    JOIN workspace w ON w.id = '00000000000000000000000001'
)
INSERT INTO workspace_member (workspace_id, member_id, role, created_at)
SELECT workspace_id, member_id,
       CASE max(rank) WHEN 4 THEN 'owner' WHEN 3 THEN 'admin' WHEN 2 THEN 'member'
                      ELSE 'viewer' END,
       strftime('%Y-%m-%dT%H:%M:%S+00:00', 'now')
FROM grant_row
GROUP BY workspace_id, member_id;

ALTER TABLE member ADD COLUMN email_verified_at TEXT;
UPDATE member SET email_verified_at = strftime('%Y-%m-%dT%H:%M:%S+00:00', 'now')
WHERE password_hash IS NOT NULL OR issuer <> 'local';

ALTER TABLE workspace ADD COLUMN created_by TEXT REFERENCES member (id) ON DELETE SET NULL;

DROP TABLE workspace_group;
DROP TABLE member_group;
DROP TABLE user_group;
