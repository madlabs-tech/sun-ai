-- Post-merge quality (US-11.4): one row per task whose PR merged, with the
-- follow-up signals the last quality check found.
CREATE TABLE task_quality (
    task_id          TEXT PRIMARY KEY REFERENCES task (id),
    mod_name         TEXT NOT NULL,
    repo             TEXT NOT NULL,
    pr_number        INTEGER,
    base             TEXT NOT NULL,
    merge_sha        TEXT NOT NULL,
    merged_at        TEXT NOT NULL,
    window_days      INTEGER NOT NULL,
    ci_state         TEXT,
    reverted_by      TEXT,
    fix_commits_json TEXT NOT NULL,
    checked_at       TEXT
) STRICT;
