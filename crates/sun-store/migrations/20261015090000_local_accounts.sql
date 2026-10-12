-- B4a: members are deactivated instead of deleted, browser sessions are
-- always member sessions (no token-holder sessions or one-time login links)
-- and the seeded "Member" group is shown as "User" (its key stays `member`).

ALTER TABLE member ADD COLUMN deactivated_at TEXT;

DELETE FROM login_token WHERE kind = 'link' OR member_id IS NULL;

UPDATE user_group SET name = 'User' WHERE key = 'member' AND name = 'Member';
