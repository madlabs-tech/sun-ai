//! Data-touched detection, an engine extension.
//!
//! The `data_touched` gate lists the task's changed files on the host
//! ([`SCRIPT`]) and matches them against a rule list: migrations, schema
//! files, SQL, ORM models, seed/fixture data, and config that points at a
//! data store. Workflows extend the list with `data_touched: {paths: [glob]}`
//! and silence paths with `ignore: [glob]`. The result is best effort and
//! labelled "detected". The agents' `tool_call` events add the databases,
//! tables and APIs their commands used (`psql … from users`, `curl https://…`).
//!
//! The gate always passes: it surfaces, it doesn't judge. Each check journals
//! a `data_touched` event on the gate step; an approval step with
//! `when: data_touched` only asks a human when the latest one found files
//! ([`touched`]).

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sun_core::model::Step;
use sun_core::{Result, RunId, TaskId};
use sun_store::{EventLog, EventQuery, RunRepo, Tx};

use crate::Engine;
use crate::executor::append;
use crate::gates::GateResult;

/// Event kind: one detection, `{step_id, attempt, label, files: [Touch]}`.
pub(crate) const KIND: &str = "data_touched";

/// Built-in rules, first match wins: `(rule, glob)`. `**` spans directories.
// ponytail: path globs only; parsing SQL or ORM code for table names is the
// upgrade when a path list proves too coarse.
const RULES: &[(&str, &str)] = &[
    ("migration", "**/migrations/**"),
    ("migration", "**/migrate/**"),
    ("migration", "**/alembic/versions/**"),
    ("schema", "**/*.prisma"),
    ("schema", "**/schema.rb"),
    ("schema", "**/structure.sql"),
    ("schema", "**/*.dbml"),
    ("schema", "**/db/schema.*"),
    ("seed_data", "**/seeds/**"),
    ("seed_data", "**/seeds.*"),
    ("seed_data", "**/seed.*"),
    ("seed_data", "**/fixtures/**"),
    ("sql", "**/*.sql"),
    ("orm_model", "**/models/**"),
    ("orm_model", "**/models.py"),
    ("orm_model", "**/entities/**"),
    ("orm_model", "**/entity/**"),
    ("data_config", "**/database.yml"),
    ("data_config", "**/database.yaml"),
    ("data_config", "**/ormconfig.*"),
    ("data_config", "**/knexfile.*"),
    ("data_config", "**/alembic.ini"),
    ("data_config", "**/drizzle.config.*"),
];

/// Config files that count when their content names a data store.
const CONFIG: &[&str] = &[
    "**/.env*",
    "**/*.env",
    "**/*.yml",
    "**/*.yaml",
    "**/*.toml",
    "**/*.json",
    "**/*.ini",
    "**/*.properties",
    "**/*.conf",
    "**/*.cfg",
];

/// Lists the task's changed files (committed since the branch left origin —
/// a pushed task branch `origin/sun/*` aside — uncommitted and untracked) as
/// `<flag> <path>`; flag 1 when the file names
/// a data store (connection URL or the usual env vars).
const SCRIPT: &str = r#"git rev-parse --git-dir >/dev/null 2>&1 || { echo 'not a git repository'; exit 1; }
first=$(git rev-list --reverse HEAD --not --exclude='origin/sun/*' --remotes=origin 2>/dev/null | head -n 1)
if [ -z "$first" ]; then base=HEAD
else base=$(git rev-parse -q --verify "$first^" || git hash-object -t tree /dev/null); fi
{ git -c core.quotePath=false diff --name-only "$base" --
  git -c core.quotePath=false ls-files --others --exclude-standard; } | sort -u |
while IFS= read -r f; do
  if [ -f "$f" ] && grep -qIiE '(postgres(ql)?|mysql|mariadb|mongodb(\+srv)?|rediss?|sqlite|mssql|sqlserver|clickhouse|cockroachdb)://|jdbc:|database_url|db_(host|name|url|user|password)|redis_url|mongo(db)?_ur[il]' -- "$f"
  then echo "1 $f"; else echo "0 $f"; fi
done"#;

/// The shell script the gate runs in the task worktree.
pub(crate) fn script() -> &'static str {
    SCRIPT
}

/// One changed file that touches data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Touch {
    /// Path relative to the worktree root; for a use (`database`, `table`,
    /// `api`) its name: a client, a URL without credentials, a table.
    pub path: String,
    /// Which rule matched: `migration`, `schema`, `seed_data`, `sql`,
    /// `orm_model`, `data_config` or `custom` (a workflow `paths:` glob) for
    /// a changed file; `database`, `table` or `api` for what the agents'
    /// commands and tool calls used.
    pub rule: String,
}

/// `data_touched` gate params.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Params {
    /// Extra globs that count as data (rule `custom`).
    #[serde(default)]
    paths: Vec<String>,
    /// Globs never reported.
    #[serde(default)]
    ignore: Vec<String>,
    /// Seconds the listing may take.
    #[serde(default)]
    pub(crate) timeout_s: Option<u64>,
}

impl Params {
    pub(crate) fn parse(params: Option<&Value>) -> std::result::Result<Self, String> {
        let parsed: Self = match params {
            Some(p) => serde_json::from_value(p.clone())
                .map_err(|e| format!("`data_touched` params: {e}"))?,
            None => Self::default(),
        };
        if parsed
            .paths
            .iter()
            .chain(&parsed.ignore)
            .any(|g| g.trim().is_empty())
        {
            return Err("`data_touched`: empty glob".into());
        }
        Ok(parsed)
    }
}

/// The data-touching files in [`SCRIPT`]'s output.
pub(crate) fn classify(listing: &str, params: &Params) -> Vec<Touch> {
    listing
        .lines()
        .filter_map(|line| {
            let (flag, path) = line.split_once(' ')?;
            if path.is_empty() || params.ignore.iter().any(|g| glob(g, path)) {
                return None;
            }
            let rule = if params.paths.iter().any(|g| glob(g, path)) {
                "custom"
            } else if let Some((rule, _)) = RULES.iter().find(|(_, g)| glob(g, path)) {
                rule
            } else if flag == "1" && CONFIG.iter().any(|g| glob(g, path)) {
                "data_config"
            } else {
                return None;
            };
            Some(Touch {
                path: path.to_owned(),
                rule: rule.to_owned(),
            })
        })
        .collect()
}

/// The gate's output for a human.
pub(crate) fn summary(touched: &[Touch]) -> String {
    if touched.is_empty() {
        return "no data touched (detected)".into();
    }
    let mut text = format!("data touched (detected): {} file(s)", touched.len());
    for t in touched {
        text.push_str(&format!("\n{}  {}", t.rule, t.path));
    }
    text
}

/// Journals a `data_touched` event for each `data_touched` gate in `results`.
/// The changed files come with the databases, tables and APIs the run's
/// agents used ([`uses_in`]).
pub(crate) async fn journal(tx: &mut Tx, gate_step: &Step, results: &[GateResult]) -> Result<()> {
    for result in results.iter().filter(|r| r.name == KIND) {
        let mut files = result.touched.clone();
        files.extend(uses(tx, gate_step.run_id).await?);
        let payload = json!({
            "step_id": gate_step.id,
            "attempt": gate_step.attempt,
            "label": "detected",
            "files": files,
        });
        append(tx, gate_step, KIND, payload).await?;
    }
    Ok(())
}

/// Whether the run's latest detection found data-touching files.
pub(crate) async fn touched(tx: &mut Tx, run_id: RunId) -> Result<bool> {
    Ok(!latest(tx, run_id).await?.is_empty())
}

/// The files of the run's latest detection (empty when none ran).
async fn latest(tx: &mut Tx, run_id: RunId) -> Result<Vec<Touch>> {
    let mut files = Value::Null;
    let mut after = 0;
    loop {
        let page = tx.events_after(Some(run_id), after, 500).await?;
        let Some(last) = page.last() else { break };
        after = last.seq;
        if let Some(event) = page.iter().rev().find(|e| e.kind == KIND) {
            files = event.payload["files"].clone();
        }
    }
    Ok(serde_json::from_value(files).unwrap_or_default())
}

impl Engine {
    /// What the latest data-touched detection of the task's latest run found;
    /// empty when nothing was touched or nothing checked.
    pub async fn data_touched(&self, task_id: TaskId) -> Result<Vec<Touch>> {
        let mut tx = self.store.read().await?;
        match tx.runs_of_task(task_id).await?.pop() {
            Some(run) => latest(&mut tx, run.id).await,
            None => Ok(Vec::new()),
        }
    }
}

/// Database clients whose use counts as touching a database.
const DB_CLIENTS: &[&str] = &[
    "psql",
    "pg_dump",
    "pg_restore",
    "mysql",
    "mysqldump",
    "mariadb",
    "sqlite3",
    "mongosh",
    "mongo",
    "redis-cli",
    "clickhouse-client",
    "sqlcmd",
    "cockroach",
];
/// Connection URL schemes of data stores.
const DB_SCHEMES: &[&str] = &[
    "postgres",
    "postgresql",
    "mysql",
    "mariadb",
    "mongodb",
    "mongodb+srv",
    "redis",
    "rediss",
    "sqlite",
    "clickhouse",
];
/// Data stores an MCP tool name (`mcp__postgres__query`) may name.
const DB_TOOLS: &[&str] = &[
    "postgres", "mysql", "sqlite", "mongo", "redis", "supabase", "neon",
];
/// Programs (and tools) whose URLs are API calls.
const HTTP_CLIENTS: &[&str] = &["curl", "wget", "http", "https", "httpie", "xh", "WebFetch"];
/// SQL words that are never a table name.
const SQL_WORDS: &[&str] = &[
    "select", "set", "where", "values", "if", "not", "exists", "only", "lateral", "table",
];

/// The databases, tables and APIs a tool call uses (rules `database`,
/// `table`, `api`; `path` is the name), read from its one-line summary
/// (`Bash psql -c 'select * from users'`). Credentials and query strings
/// never make it into a name.
// ponytail: word heuristics over the summary; a SQL parser is the upgrade
// when tables in subqueries or quoted identifiers matter.
pub(crate) fn uses_in(text: &str) -> Vec<Touch> {
    let mut out: Vec<Touch> = Vec::new();
    let mut add = |rule: &str, name: String| {
        if !out.iter().any(|t| t.rule == rule && t.path == name) {
            out.push(Touch {
                path: name,
                rule: rule.to_owned(),
            });
        }
    };
    let words: Vec<&str> = text
        .split(|c: char| c.is_whitespace() || "'\"`;(),=".contains(c))
        .filter(|w| !w.is_empty())
        .collect();
    let program = |w: &str| w.rsplit('/').next().unwrap_or(w).to_owned();
    let tool = words.first().copied().unwrap_or_default();
    let (mut db, mut http) = (false, tool == "WebFetch");
    if tool.starts_with("mcp__") && DB_TOOLS.iter().any(|d| tool.contains(d)) {
        db = true;
        add("database", tool.to_owned());
    }
    for w in &words {
        let p = program(w);
        if DB_CLIENTS.contains(&p.as_str()) {
            db = true;
            add("database", p);
        } else if HTTP_CLIENTS.contains(&p.as_str()) {
            http = true;
        }
        if let Some((scheme, rest)) = w.split_once("://")
            && DB_SCHEMES.contains(&scheme.to_ascii_lowercase().as_str())
        {
            db = true;
            add("database", format!("{scheme}://{}", host_path(rest)));
        }
    }
    let lower = text.to_ascii_lowercase();
    let sql = db
        || (lower.contains("select ") && lower.contains(" from "))
        || [
            "insert into ",
            "delete from ",
            "create table ",
            "alter table ",
            "drop table ",
        ]
        .iter()
        .any(|s| lower.contains(s));
    if sql {
        let lower: Vec<String> = words.iter().map(|w| w.to_ascii_lowercase()).collect();
        for (i, w) in lower.iter().enumerate() {
            let table_kw = w == "table"
                && i > 0
                && ["create", "alter", "drop", "truncate"].contains(&lower[i - 1].as_str());
            if !(table_kw || ["from", "join", "into", "update"].contains(&w.as_str())) {
                continue;
            }
            let name = lower[i + 1..]
                .iter()
                .find(|n| !SQL_WORDS.contains(&n.as_str()))
                .filter(|n| {
                    n.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
                        && n.chars()
                            .all(|c| c.is_ascii_alphanumeric() || "_.".contains(c))
                });
            if let Some(name) = name {
                add("table", name.clone());
            }
        }
    }
    if http {
        for w in &words {
            if let Some(at) = w.find("http://").or_else(|| w.find("https://"))
                && let Some((scheme, rest)) = w[at..].split_once("://")
            {
                add("api", format!("{scheme}://{}", host_path(rest)));
            }
        }
    }
    out
}

/// `host/path` of a URL's rest after `://`: no user info, query or fragment.
fn host_path(rest: &str) -> String {
    let rest = rest.split(['?', '#']).next().unwrap_or_default();
    let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
    let host = authority.rsplit('@').next().unwrap_or(authority);
    match path {
        "" => host.to_owned(),
        _ => format!("{host}/{path}"),
    }
}

/// The databases, tables and APIs the run's agents used (`tool_call`
/// events), each once.
async fn uses(tx: &mut Tx, run_id: RunId) -> Result<Vec<Touch>> {
    let mut out: Vec<Touch> = Vec::new();
    let mut after = 0;
    loop {
        let page = tx
            .search_events(run_id, Some("tool_call"), None, after, 500)
            .await?;
        let Some(last) = page.last() else { break };
        after = last.seq;
        for event in &page {
            let summary = event.payload["summary"].as_str().unwrap_or_default();
            for touch in uses_in(summary) {
                if !out.contains(&touch) {
                    out.push(touch);
                }
            }
        }
    }
    Ok(out)
}

/// `path` matches `pattern`: `*` is any run of characters within a segment,
/// `**` any number of whole segments.
fn glob(pattern: &str, path: &str) -> bool {
    fn segments(p: &[&str], s: &[&str]) -> bool {
        match p.split_first() {
            None => s.is_empty(),
            Some((&"**", rest)) => (0..=s.len()).any(|i| segments(rest, &s[i..])),
            Some((first, rest)) => {
                s.first()
                    .is_some_and(|seg| segment(first.as_bytes(), seg.as_bytes()))
                    && segments(rest, &s[1..])
            }
        }
    }
    fn segment(p: &[u8], s: &[u8]) -> bool {
        match p.split_first() {
            None => s.is_empty(),
            Some((b'*', rest)) => (0..=s.len()).any(|i| segment(rest, &s[i..])),
            Some((c, rest)) => s.first() == Some(c) && segment(rest, &s[1..]),
        }
    }
    let p: Vec<&str> = pattern.split('/').collect();
    let s: Vec<&str> = path.split('/').collect();
    segments(&p, &s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_touched_globs() {
        assert!(glob("**/migrations/**", "migrations/001.sql"));
        assert!(glob("**/migrations/**", "crates/db/migrations/x/y.sql"));
        assert!(glob("**/*.sql", "a.sql"));
        assert!(glob("**/.env*", "deploy/.env.production"));
        assert!(!glob("**/*.sql", "a.sqlx"));
        assert!(!glob("**/models/**", "src/model/user.rs"));
        assert!(glob("infra/rds/*", "infra/rds/main.tf"));
        assert!(!glob("infra/rds/*", "infra/rds/x/main.tf"));
    }

    #[test]
    fn data_touched_rules_classify_a_listing() {
        let listing = "0 db/migrations/20240101_users.sql\n0 prisma/schema.prisma\n\
                       0 app/models/user.rb\n0 test/fixtures/users.yml\n0 src/main.rs\n\
                       1 .env\n0 config/app.yml\n1 config/database.yml\n1 README.md\n\
                       0 infra/rds/main.tf\n0 docs/models/intro.md\n0 queries/report.sql";
        let params = Params::parse(Some(&json!({
            "paths": ["infra/rds/**"],
            "ignore": ["docs/**"],
        })))
        .unwrap();
        let touched = classify(listing, &params);
        let got: Vec<(&str, &str)> = touched
            .iter()
            .map(|t| (t.rule.as_str(), t.path.as_str()))
            .collect();
        assert_eq!(
            got,
            [
                ("migration", "db/migrations/20240101_users.sql"),
                ("schema", "prisma/schema.prisma"),
                ("orm_model", "app/models/user.rb"),
                ("seed_data", "test/fixtures/users.yml"),
                ("data_config", ".env"),
                ("data_config", "config/database.yml"),
                ("custom", "infra/rds/main.tf"),
                ("sql", "queries/report.sql"),
            ]
        );
        assert_eq!(summary(&[]), "no data touched (detected)");
    }

    #[test]
    fn psql_command_detects_users_table() {
        let got = |text: &str| -> Vec<(String, String)> {
            uses_in(text)
                .into_iter()
                .map(|t| (t.rule, t.path))
                .collect()
        };
        let pair = |r: &str, p: &str| (r.to_owned(), p.to_owned());
        assert_eq!(
            got(
                "Bash psql postgres://app:s3cret@db.prod:5432/shop -c 'SELECT id FROM users JOIN orders o'"
            ),
            [
                pair("database", "psql"),
                pair("database", "postgres://db.prod:5432/shop"),
                pair("table", "users"),
                pair("table", "orders"),
            ]
        );
        assert_eq!(
            got(r#"mcp__postgres__query {"sql":"insert into public.audit values (1)"}"#),
            [
                pair("database", "mcp__postgres__query"),
                pair("table", "public.audit")
            ]
        );
        assert_eq!(
            got("Bash curl -s https://user:pw@api.stripe.com/v1/charges?limit=3"),
            [pair("api", "https://api.stripe.com/v1/charges")]
        );
        assert_eq!(
            got("WebFetch https://docs.rs/serde"),
            [pair("api", "https://docs.rs/serde")]
        );
        // Words that only look like SQL or URLs outside a client: nothing.
        assert!(got("Bash git log --oneline from main").is_empty());
        assert!(got("Bash git clone https://github.com/a/b").is_empty());
        assert!(got("Read src/models/user.rs").is_empty());
    }

    #[test]
    fn data_touched_params_are_validated() {
        assert!(Params::parse(Some(&json!({"paths": [""]}))).is_err());
        assert!(Params::parse(Some(&json!({"nope": 1}))).is_err());
        assert!(Params::parse(None).is_ok());
    }
}
