-- Git like multica: repos are a per-workspace list, runs record the repos
-- they work in, and pull requests are mirrored from the git hosts and linked
-- to tasks. Old repo rows are dropped, not migrated (the user chose to
-- delete the old implementation); `vcs_connection` stays as it is.

-- 1. Repos belong to one workspace; the link table and the old rows go.
DROP TABLE workspace_repo;
DROP TABLE repo;
CREATE TABLE repo (
    id               TEXT PRIMARY KEY,
    workspace_id     TEXT NOT NULL REFERENCES workspace (id) ON DELETE CASCADE,
    url              TEXT NOT NULL,
    default_branch   TEXT NOT NULL,
    gate_config_json TEXT NOT NULL DEFAULT '{}',
    description      TEXT NOT NULL DEFAULT '',
    -- The GitLab project hook Sun made for it.
    hook_id          INTEGER,
    created_at       TEXT NOT NULL,
    UNIQUE (workspace_id, url)
) STRICT;

-- 2. Tasks no longer pick repos; a person may keep one task where it is
--    when its PRs merge. Runs record the repos resolved for them.
ALTER TABLE task DROP COLUMN repo_ids_json;
ALTER TABLE task ADD COLUMN pr_auto_complete_disabled INTEGER NOT NULL DEFAULT 0;
ALTER TABLE run ADD COLUMN repo_ids_json TEXT NOT NULL DEFAULT '[]';

-- 3. The workspace's git rules (`sun_core::model::GitSettings`; `{}` = all
--    on). `workspace` is the renamed `board` table, which never had the
--    M1 `settings_json` column, so this is an add, not a rename.
ALTER TABLE workspace ADD COLUMN git_settings_json TEXT NOT NULL DEFAULT '{}';

-- 4. Mirrored pull requests. `pr_updated_at` guards upserts (an older
--    delivery never overwrites a newer one); the snapshot columns are
--    fetched separately and hold for `snapshot_head_sha` only.
CREATE TABLE pull_request (
    id                  TEXT PRIMARY KEY,
    workspace_id        TEXT NOT NULL REFERENCES workspace (id) ON DELETE CASCADE,
    provider            TEXT NOT NULL CHECK (provider IN ('github', 'gitlab')),
    installation_id     INTEGER,
    repo_owner          TEXT NOT NULL,
    repo_name           TEXT NOT NULL,
    number              INTEGER NOT NULL,
    title               TEXT NOT NULL,
    state               TEXT NOT NULL CHECK (state IN ('open', 'closed', 'merged', 'draft')),
    html_url            TEXT NOT NULL,
    branch              TEXT,
    author_login        TEXT,
    author_avatar_url   TEXT,
    merged_at           TEXT,
    closed_at           TEXT,
    pr_created_at       TEXT NOT NULL,
    pr_updated_at       TEXT NOT NULL,
    head_sha            TEXT NOT NULL DEFAULT '',
    additions           INTEGER NOT NULL DEFAULT 0,
    deletions           INTEGER NOT NULL DEFAULT 0,
    changed_files       INTEGER NOT NULL DEFAULT 0,
    mergeable           TEXT,
    merge_state_status  TEXT,
    checks_rollup_state TEXT,
    checks_total        INTEGER NOT NULL DEFAULT 0,
    checks_passed       INTEGER NOT NULL DEFAULT 0,
    checks_failed       INTEGER NOT NULL DEFAULT 0,
    checks_pending      INTEGER NOT NULL DEFAULT 0,
    snapshot_head_sha   TEXT NOT NULL DEFAULT '',
    snapshot_fetched_at TEXT,
    UNIQUE (workspace_id, repo_owner, repo_name, number)
) STRICT;
CREATE INDEX pull_request_by_head ON pull_request (workspace_id, repo_owner, repo_name, head_sha);
CREATE INDEX pull_request_by_url ON pull_request (workspace_id, html_url);

-- A task's linked PRs; `linked_by` NULL = linked automatically.
CREATE TABLE task_pull_request (
    task_id         TEXT NOT NULL REFERENCES task (id) ON DELETE CASCADE,
    pull_request_id TEXT NOT NULL REFERENCES pull_request (id) ON DELETE CASCADE,
    linked_by       TEXT REFERENCES member (id) ON DELETE SET NULL,
    linked_at       TEXT NOT NULL,
    PRIMARY KEY (task_id, pull_request_id)
) STRICT;
CREATE INDEX task_pull_request_by_pr ON task_pull_request (pull_request_id);

-- A PR a person removed from a task: auto-link leaves it alone from then on.
CREATE TABLE task_pull_request_exclusion (
    task_id         TEXT NOT NULL REFERENCES task (id) ON DELETE CASCADE,
    pull_request_id TEXT NOT NULL REFERENCES pull_request (id) ON DELETE CASCADE,
    excluded_by     TEXT,
    created_at      TEXT NOT NULL,
    PRIMARY KEY (task_id, pull_request_id)
) STRICT;

-- 5. The activity log gets the `pull_request` kind. SQLite can't alter a
--    CHECK, so the table is rebuilt (nothing references it by foreign key).
CREATE TABLE task_activity_new (
    id              TEXT PRIMARY KEY,
    task_id         TEXT NOT NULL REFERENCES task (id),
    seq             INTEGER NOT NULL CHECK (seq >= 1),
    kind            TEXT NOT NULL CHECK (kind IN ('comment', 'status', 'priority', 'assignee',
                        'project', 'title', 'description', 'created', 'run', 'pull_request')),
    actor_member_id TEXT,
    body_json       TEXT NOT NULL,
    created_at      TEXT NOT NULL,
    UNIQUE (task_id, seq)
) STRICT;
INSERT INTO task_activity_new (id, task_id, seq, kind, actor_member_id, body_json, created_at)
SELECT id, task_id, seq, kind, actor_member_id, body_json, created_at FROM task_activity;
DROP TABLE task_activity;
ALTER TABLE task_activity_new RENAME TO task_activity;

CREATE TRIGGER task_activity_is_append_only BEFORE UPDATE ON task_activity
BEGIN
    SELECT RAISE(ABORT, 'task_activity is append-only');
END;
