-- Shared hosts (BLO-197): a deployment owner shares a host with groups;
-- their members may attach it to workspaces where they manage runtimes.
-- `workspace_host.shared_via` is the member who attached it that way (NULL:
-- attached directly, by a deployment owner, the member who enrolled it, or
-- enrolment itself). Such a link gives the workspace the host only while
-- the host is still shared with a group that member is in: an unshare or a
-- member leaving the group stops new placements there at once, and running
-- sessions finish.
CREATE TABLE host_share (
    host_id    TEXT NOT NULL REFERENCES host (id) ON DELETE CASCADE,
    group_id   TEXT NOT NULL REFERENCES member_group (id) ON DELETE CASCADE,
    shared_by  TEXT REFERENCES member (id) ON DELETE SET NULL,
    created_at TEXT NOT NULL,
    PRIMARY KEY (host_id, group_id)
) STRICT;
CREATE INDEX host_share_by_group ON host_share (group_id);

ALTER TABLE workspace_host ADD COLUMN shared_via TEXT REFERENCES member (id) ON DELETE CASCADE;
