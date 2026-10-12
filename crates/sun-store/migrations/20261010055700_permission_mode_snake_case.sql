-- A24: Sun's permission modes are snake_case; Claude Code's camelCase
-- spelling now lives only in its adapter.
UPDATE runtime_policy SET permission_mode = 'accept_edits' WHERE permission_mode = 'acceptEdits';
UPDATE runtime_policy SET permission_mode = 'bypass_permissions' WHERE permission_mode = 'bypassPermissions';
