-- Bug issues linked to a merged PR (US-11.4): the origin issue, referenced
-- issues and post-merge regressions, as a JSON list of BugIssue.
ALTER TABLE task_quality ADD COLUMN bug_issues_json TEXT NOT NULL DEFAULT '[]';
