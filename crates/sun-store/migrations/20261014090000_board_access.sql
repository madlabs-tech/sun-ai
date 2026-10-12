-- B3b: a board is visible to the groups attached to it (plus owners and
-- `access_all_boards`). A board with no groups is admin-only. Main goes to
-- the seeded Viewer, Member and Admin groups, so nobody loses it on upgrade.

CREATE TABLE board_group (
    board_id TEXT NOT NULL REFERENCES board (id) ON DELETE CASCADE,
    group_id TEXT NOT NULL REFERENCES user_group (id) ON DELETE CASCADE,
    PRIMARY KEY (board_id, group_id)
) STRICT;
CREATE INDEX board_group_by_group ON board_group (group_id);

INSERT INTO board_group (board_id, group_id)
SELECT '00000000000000000000000001', id FROM user_group
WHERE id IN ('00000000000000000000000001', '00000000000000000000000002',
             '00000000000000000000000003');
