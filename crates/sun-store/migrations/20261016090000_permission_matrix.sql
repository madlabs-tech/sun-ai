-- B4b: permissions per verb. `manage_access` splits into the member
-- permissions, `manage_groups` and `manage_settings`; `edit_board` no longer
-- deletes, so a group that had it also gets `delete_board`. A plain text
-- REPLACE on the JSON array (the same on Postgres); order and duplicates are
-- normalised on read.
UPDATE user_group
SET permissions_json = replace(replace(permissions_json,
    '"manage_access"',
    '"invite_members","create_members","view_members","edit_members","deactivate_members","manage_groups","manage_settings"'),
    '"edit_board"', '"edit_board","delete_board"');
