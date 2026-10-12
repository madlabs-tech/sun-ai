//! Push before PR: a git host's `open_pr` needs its head branch on the
//! remote. Before the action's outbox row is enqueued, the run's host commits
//! what the agent left uncommitted and pushes the task branch to `origin`
//! through `HostLink::exec`. Idempotent: an up-to-date push is a no-op.
//! A multi-repo task pushes each repo's worktree and skips the repos the task
//! didn't change (no commit ahead of their base): they get no PR.
//!
//! Credentials are the token of the task workspace's connection on the
//! repo's host (`vcs`: a GitHub installation token minted for the push, or
//! the GitLab token), else the git host mod's `<MOD>_TOKEN` secret (`sun
//! secret set --mod github GITHUB_TOKEN`). Git gets it from a credential
//! helper that reads it from the exec env, so it is never in argv or a file
//! and the host redacts it from output. Without either, the host's own git
//! credentials (ssh keys, credential store) are used.

use std::collections::BTreeMap;

use sun_core::host_protocol::{ExecRequest, Worktree};
use sun_core::model::{SecretScope, VcsConnection, VcsProvider};
use sun_core::{Error, HostId, HostLink, Result};
use sun_store::{SecretRepo, Tx};

use crate::gates::quote;
use crate::outbox::RunFacts;

/// The env var the credential helper reads the token from.
const TOKEN_ENV: &str = "SUN_GIT_TOKEN";
const TIMEOUT_S: u64 = 300;
/// Starts the push line of every push script; an eval sandbox skips it (`eval`).
pub(crate) const PUSH: &str = "GIT_TERMINAL_PROMPT=0 git ";

/// Exit code of [`SecretScan::script`] when it finds a secret.
pub(crate) const SECRET_FOUND: i32 = 4;

/// The diff secret scan: agents see secrets in their env, so
/// before Sun commits (checkpoint) or pushes their work, the change is
/// searched for every stored secret value. The values travel only in the
/// exec env (`SUN_SCAN_<i>`; the host redacts env values from output), the
/// search runs on the host with `grep -F`, and the script exits
/// [`SECRET_FOUND`] without committing.
pub(crate) struct SecretScan(Vec<String>);

impl SecretScan {
    /// From `SecretRepo::known_secrets`; values too short for
    /// `sun_core::redact` to treat as secrets are skipped the same way.
    pub(crate) fn new(known_secrets: Vec<String>) -> Self {
        Self(
            known_secrets
                .into_iter()
                .filter(|s| s.len() >= sun_core::redact::MIN_SECRET_LEN)
                .collect(),
        )
    }

    /// Shell run after `git add -A`: searches the staged change and every
    /// commit not on `origin`.
    pub(crate) fn script(&self) -> String {
        if self.0.is_empty() {
            return String::new();
        }
        let patterns: String = (0..self.0.len())
            .map(|i| format!(" -e \"$SUN_SCAN_{i}\""))
            .collect();
        format!(
            "if {{ git diff --cached; git log -p --format= HEAD --not --remotes=origin; }} \
             2>/dev/null | grep -F -q{patterns}; then\n\
               echo 'blocked: the change contains the value of a stored secret; \
             remove it from the worktree (and its commits) and retry'; exit {SECRET_FOUND}\n\
             fi\n"
        )
    }

    /// The env [`Self::script`] reads.
    pub(crate) fn env(&self) -> impl Iterator<Item = (String, String)> + '_ {
        self.0
            .iter()
            .enumerate()
            .map(|(i, value)| (format!("SUN_SCAN_{i}"), value.clone()))
    }
}

/// One push, built in the engine and run off its task.
pub(crate) struct Job {
    host: HostId,
    worktree: Worktree,
    branch: String,
    message: String,
    /// `(username, token)` for HTTPS remotes: the mod secret, used where
    /// no workspace connection reaches the repo.
    credentials: Option<(&'static str, String)>,
    /// Mints or reads connection tokens at run time (outside any transaction).
    vcs: Option<crate::vcs::Vcs>,
    /// The single repo's connection (a one-repo task).
    connection: Option<VcsConnection>,
    scan: SecretScan,
    /// A multi-repo task's repos, each with its own connection; empty otherwise.
    repos: Vec<Target>,
}

/// One repo of a multi-repo push.
struct Target {
    folder: String,
    slug: String,
    base: String,
    /// The connection on this repo's host, if the workspace has one.
    connection: Option<VcsConnection>,
}

/// Printed by a multi-repo push for a repo with nothing to push.
const UNCHANGED: &str = "sun: unchanged";

/// The mod secret holding `mod_name`'s push token, e.g. `GITHUB_TOKEN`.
fn token_name(mod_name: &str) -> String {
    format!("{}_TOKEN", mod_name.to_uppercase())
}

impl Job {
    /// The push an `open_pr` of mod `mod_name` needs, or `None` for a task
    /// without a repo (the mod then names what's missing).
    pub(crate) async fn new(
        tx: &mut Tx,
        host: HostId,
        facts: &RunFacts,
        worktree: Worktree,
        mod_name: &str,
        vcs: &crate::vcs::Vcs,
    ) -> Result<Option<Self>> {
        if facts.repos.is_empty() {
            return Ok(None);
        }
        let workspace = facts.task.workspace_id;
        // Each repo gets the connection on its own host.
        let mut connections = Vec::new();
        for repo in &facts.repos {
            connections.push(crate::vcs::connection_of_repo(tx, workspace, repo).await?);
        }
        let repos = match &worktree {
            Worktree::Repos { repos, .. } => repos
                .iter()
                .zip(&facts.repos)
                .zip(&connections)
                .map(|((checkout, repo), connection)| Target {
                    folder: checkout.name.clone(),
                    slug: crate::outbox::repo_slug(&repo.url),
                    base: repo.default_branch.clone(),
                    connection: connection.clone(),
                })
                .collect(),
            _ => Vec::new(),
        };
        let branch = facts.branch.clone();
        let message = facts.task.title.clone();
        let mut job = Self::branch(tx, host, worktree, mod_name, branch, message).await?;
        job.repos = repos;
        job.connection = connections.into_iter().next().flatten();
        job.vcs = Some(vcs.clone());
        // From the seller's machine a marketplace task pushes only with a
        // token the buyer allowed it; the buyer's own host (a
        // mid-run failover's checkpoint) keeps it.
        if crate::market::is_market_host(tx, host).await?
            && crate::market::secret_withheld(tx, facts.task.id, &token_name(mod_name)).await?
        {
            job.credentials = None;
            job.vcs = None;
        }
        Ok(Some(job))
    }

    /// Commits what `worktree` holds as `message` and pushes `branch`,
    /// with mod `mod_name`'s token (e.g. a memory proposal's PR branch).
    pub(crate) async fn branch(
        tx: &mut Tx,
        host: HostId,
        worktree: Worktree,
        mod_name: &str,
        branch: String,
        message: String,
    ) -> Result<Self> {
        let scope = SecretScope::Mod(mod_name.to_owned());
        let token = match tx.secret_value(&scope, &token_name(mod_name)).await {
            Ok(token) => Some(token),
            Err(e) => match Error::from(e) {
                Error::NotFound(_) => None,
                e => return Err(e),
            },
        };
        // Usernames the hosts accept with a token as the password.
        let username = match mod_name {
            "github" => "x-access-token",
            "gitlab" => "oauth2",
            _ => "sun",
        };
        Ok(Self {
            host,
            worktree,
            branch,
            message,
            credentials: token.map(|token| (username, token)),
            vcs: None,
            connection: None,
            scan: SecretScan::new(tx.known_secrets().await?),
            repos: Vec::new(),
        })
    }

    /// The shell script run in the worktree; with `base`, a worktree with
    /// no commit ahead of `origin/<base>` prints [`UNCHANGED`] and pushes nothing.
    fn command(&self, base: Option<&str>, credentials: Option<&(&str, String)>) -> String {
        let refspec = quote(&format!("refs/heads/{0}:refs/heads/{0}", self.branch));
        let helper = match credentials {
            Some((username, _)) => format!(
                "-c credential.helper= -c {} ",
                quote(&format!(
                    "credential.helper=!f() {{ test \"$1\" = get || exit 0; \
                     echo username={username}; echo \"password=${TOKEN_ENV}\"; }}; f"
                ))
            ),
            None => String::new(),
        };
        format!(
            "git add -A || exit 1\n\
             {scan}\
             if [ -n \"$(git status --porcelain)\" ]; then\n\
               git -c user.name=sun -c user.email=sun@localhost \
             -c core.hooksPath=/dev/null commit -q --no-verify -m {message} || exit 1\n\
             fi\n\
             {unchanged}\
             {PUSH}{helper}push -q origin {refspec} 2>&1\n",
            scan = self.scan.script(),
            message = quote(&self.message),
            unchanged = match base {
                Some(base) => format!(
                    "if [ \"$(git rev-list --count {}..HEAD)\" = 0 ]; then echo '{UNCHANGED}'; exit 0; fi\n",
                    quote(&format!("refs/remotes/origin/{base}"))
                ),
                None => String::new(),
            },
        )
    }

    /// Commits and pushes; `Err` is the step's failure reason. A multi-repo
    /// task's `Ok` lists the slugs of the repos it pushed (the changed ones).
    pub(crate) async fn run(
        self,
        hosts: &dyn HostLink,
    ) -> std::result::Result<Option<Vec<String>>, String> {
        let cwd = hosts
            .prepare_worktree(self.host, self.worktree.clone())
            .await
            .map_err(|e| format!("push: host: {e}"))?;
        if self.repos.is_empty() {
            let credentials = self.credentials_for(self.connection.as_ref()).await?;
            return self
                .push(hosts, cwd, None, credentials.as_ref())
                .await
                .map(|_| None);
        }
        let mut changed = Vec::new();
        for target in &self.repos {
            let credentials = self.credentials_for(target.connection.as_ref()).await?;
            let out = self
                .push(
                    hosts,
                    cwd.join(&target.folder),
                    Some(&target.base),
                    credentials.as_ref(),
                )
                .await?;
            if !out.contains(UNCHANGED) {
                changed.push(target.slug.clone());
            }
        }
        Ok(Some(changed))
    }

    /// `(username, token)` for a repo: its connection's token, minted now,
    /// else the mod secret.
    async fn credentials_for(
        &self,
        connection: Option<&VcsConnection>,
    ) -> std::result::Result<Option<(&'static str, String)>, String> {
        let (Some(vcs), Some(connection)) = (&self.vcs, connection) else {
            return Ok(self.credentials.clone());
        };
        let credentials = vcs
            .credentials(connection, false)
            .await
            .map_err(|e| format!("push: {} token: {e}", connection.provider))?;
        // Usernames the hosts accept with a token as the password.
        let username = match connection.provider {
            VcsProvider::Github => "x-access-token",
            VcsProvider::Gitlab => "oauth2",
        };
        Ok(Some((username, credentials.token)))
    }

    /// One worktree's push; `Ok` is its output.
    async fn push(
        &self,
        hosts: &dyn HostLink,
        cwd: std::path::PathBuf,
        base: Option<&str>,
        credentials: Option<&(&'static str, String)>,
    ) -> std::result::Result<String, String> {
        let env: BTreeMap<String, String> = credentials
            .iter()
            .map(|(_, token)| (TOKEN_ENV.to_owned(), token.clone()))
            .chain(self.scan.env())
            .collect();
        let request = ExecRequest {
            cwd,
            command: self.command(base, credentials),
            env,
            timeout_s: TIMEOUT_S,
        };
        let out = hosts
            .exec(self.host, request)
            .await
            .map_err(|e| format!("push: host: {e}"))?;
        match (out.code, out.timed_out) {
            (Some(0), _) => Ok(out.output),
            (Some(SECRET_FOUND), _) => {
                Err(format!("push of {} {}", self.branch, out.output.trim()))
            }
            (_, true) => Err(format!("push of {} timed out", self.branch)),
            _ => Err(format!(
                "push of {} failed: {}",
                self.branch,
                out.output.trim()
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use std::path::Path;

    use sun_core::TaskId;

    use sun_store::WorkflowRepo;

    use super::*;
    use crate::gates::tests::LocalHost;

    fn git(cwd: &Path, args: &str) -> String {
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("git {args}"))
            .current_dir(cwd)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args}: {out:?}");
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    #[tokio::test]
    async fn push_commits_leftovers_pushes_the_branch_and_repeats_as_a_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let host = LocalHost::new(dir.path(), |_, _| {});
        let task_id = TaskId::generate();
        let worktree = Worktree::Folder { task_id };
        let id = HostId::generate();
        let cwd = host.prepare_worktree(id, worktree.clone()).await.unwrap();
        let remote = dir.path().join("remote.git");
        git(dir.path(), "init -q --bare remote.git");
        let branch = "sun/MAIN-1-fix-it".to_owned();
        git(&cwd, &format!("checkout -q -b {branch}"));
        git(&cwd, &format!("remote add origin {}", remote.display()));
        std::fs::write(cwd.join("work.txt"), "left uncommitted\n").unwrap();

        let job = || Job {
            host: id,
            worktree: worktree.clone(),
            branch: branch.clone(),
            message: "Fix 'it'".into(),
            credentials: Some(("sun", "s3cret".into())),
            vcs: None,
            connection: None,
            scan: SecretScan::new(vec!["byo-key-0123456789".into()]),
            repos: Vec::new(),
        };
        let token = ("sun", "s3cret".to_owned());
        assert!(job().command(None, Some(&token)).contains("$SUN_GIT_TOKEN"));
        assert!(
            !job().command(None, Some(&token)).contains("s3cret"),
            "never in argv"
        );
        job().run(&*host).await.unwrap();
        let last = |dir: &Path| {
            git(
                dir,
                &format!("--git-dir remote.git log -1 --format=%s {branch}"),
            )
        };
        assert_eq!(last(dir.path()), "Fix 'it'");
        job().run(&*host).await.unwrap();
        assert_eq!(last(dir.path()), "Fix 'it'", "a second push is a no-op");
        assert_eq!(git(&cwd, "status --porcelain"), "");

        // A stored secret in the change blocks the commit and the push, and
        // the reason never shows the value.
        std::fs::write(cwd.join("leak.txt"), "key = byo-key-0123456789\n").unwrap();
        let why = job().run(&*host).await.unwrap_err();
        assert!(
            why.contains("blocked") && why.contains("stored secret"),
            "{why}"
        );
        assert!(!why.contains("byo-key-0123456789"), "{why}");
        assert_eq!(last(dir.path()), "Fix 'it'", "nothing pushed");
        assert_eq!(
            git(&cwd, "log -1 --format=%s"),
            "Fix 'it'",
            "nothing committed"
        );
        // Once it is gone, the push goes through.
        std::fs::write(cwd.join("leak.txt"), "key from env\n").unwrap();
        job().run(&*host).await.unwrap();
    }

    /// A host that only records each push: its folder and the token in its env.
    #[derive(Default)]
    struct Recorder(std::sync::Mutex<Vec<(String, Option<String>)>>);

    #[async_trait::async_trait]
    impl HostLink for Recorder {
        async fn start_session(
            &self,
            _: HostId,
            _: &str,
            _: sun_core::SessionSpec,
        ) -> Result<sun_core::SessionHandle> {
            Err(Error::Unavailable("no sessions".into()))
        }
        async fn steer(&self, _: &sun_core::SessionHandle, _: sun_core::Steer) -> Result<()> {
            Ok(())
        }
        async fn stop_session(
            &self,
            _: &sun_core::SessionHandle,
            _: sun_core::StopMode,
        ) -> Result<()> {
            Ok(())
        }
        async fn is_alive(&self, _: &sun_core::SessionHandle) -> Result<bool> {
            Ok(false)
        }
        fn events(&self) -> sun_core::BoxStream<'static, sun_core::HostEvent> {
            Box::pin(futures_util_empty())
        }
        async fn ack(&self, _: sun_core::SessionId, _: u64) -> Result<()> {
            Ok(())
        }
        async fn prepare_worktree(&self, _: HostId, _: Worktree) -> Result<std::path::PathBuf> {
            Ok("/session".into())
        }
        async fn exec(
            &self,
            _: HostId,
            request: ExecRequest,
        ) -> Result<sun_core::host_protocol::ExecOutput> {
            let folder = request.cwd.display().to_string();
            let token = request.env.get(TOKEN_ENV).cloned();
            self.0.lock().unwrap().push((folder, token));
            Ok(sun_core::host_protocol::ExecOutput {
                code: Some(0),
                output: String::new(),
                timed_out: false,
            })
        }
    }

    fn futures_util_empty() -> impl futures_core::Stream<Item = sun_core::HostEvent> + Send {
        struct Empty;
        impl futures_core::Stream for Empty {
            type Item = sun_core::HostEvent;
            fn poll_next(
                self: std::pin::Pin<&mut Self>,
                _: &mut std::task::Context<'_>,
            ) -> std::task::Poll<Option<Self::Item>> {
                std::task::Poll::Ready(None)
            }
        }
        Empty
    }

    /// One run, two repos on two hosts: each repo is pushed with its own
    /// host's connection (a minted GitHub installation token, the GitLab
    /// token), not the first repo's.
    #[tokio::test(flavor = "multi_thread")]
    async fn push_gives_each_repo_of_a_multi_repo_task_its_own_host_token() {
        use serde_json::json;
        use sun_core::host_protocol::RepoCheckout;
        use sun_core::model::{Repo, Task, Workflow};
        use sun_core::{RepoId, RunId, TaskStatus, VcsConnectionId, WorkflowId, WorkspaceId};
        use sun_store::{RepoRepo, Store, TaskRepo, VcsRepo};

        let (github_api, _) = sun_mod_sdk::http::mock("", |r| {
            let jwt = r
                .header("authorization")
                .is_some_and(|a| a.starts_with("Bearer ey"));
            match (r.method.as_str(), r.target.as_str()) {
                ("POST", "/app/installations/42/access_tokens") if jwt => {
                    let expires = (chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339();
                    (
                        201,
                        json!({ "token": "ghs_minted_for_web", "expires_at": expires }),
                    )
                }
                _ => (401, json!({ "message": "Bad credentials" })),
            }
        })
        .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).await.unwrap();
        let connection =
            |provider, instance: &str, installation, token: Option<&str>| VcsConnection {
                id: VcsConnectionId::generate(),
                workspace_id: WorkspaceId::MAIN,
                provider,
                instance_url: instance.into(),
                account_login: "acme".into(),
                installation_id: installation,
                token_secret: token.map(str::to_owned),
                refresh_secret: None,
                token_expires_at: None,
                oauth_redirect_uri: None,
                webhook_secret: None,
                connected_by: "user:ann".into(),
                created_at: chrono::Utc::now(),
            };
        let github = connection(VcsProvider::Github, "https://github.com", Some(42), None);
        let gitlab = connection(
            VcsProvider::Gitlab,
            "https://gitlab.acme.io",
            None,
            Some("GL_TOKEN"),
        );
        let repo = |url: &str| Repo {
            id: RepoId::generate(),
            workspace_id: WorkspaceId::MAIN,
            url: url.into(),
            default_branch: "main".into(),
            gate_config: json!({}),
            description: String::new(),
            hook_id: None,
        };
        let repos = vec![
            repo("https://github.com/acme/web.git"),
            repo("https://gitlab.acme.io/acme/api.git"),
        ];
        let workflow = Workflow {
            id: WorkflowId::generate(),
            workspace_id: WorkspaceId::MAIN,
            name: "w".into(),
        };
        let task = Task {
            workflow_id: Some(workflow.id),
            status: TaskStatus::Backlog,
            ..Task::new(WorkspaceId::MAIN, "Ship it")
        };
        let eco = crate::ecosystem_scope().unwrap();
        let mut tx = store.begin().await.unwrap();
        tx.insert_workflow(&workflow).await.unwrap();
        for r in &repos {
            tx.insert_repo(r).await.unwrap();
        }
        tx.insert_task(&task).await.unwrap();
        tx.put_connection(&github).await.unwrap();
        tx.put_connection(&gitlab).await.unwrap();
        let scope = SecretScope::Workspace(WorkspaceId::MAIN);
        tx.set_secret(&scope, "GL_TOKEN", "glpat-for-api")
            .await
            .unwrap();
        for (name, value) in [
            ("GITHUB_APP_ID", "123456"),
            (
                "GITHUB_APP_PRIVATE_KEY",
                include_str!("vcs/test_app_key.pem"),
            ),
            ("GITHUB_APP_API_URL", github_api.as_str()),
        ] {
            tx.set_secret(&eco, name, value).await.unwrap();
        }
        tx.commit().await.unwrap();

        let branch = "sun/MAIN-1-ship-it".to_owned();
        let facts = RunFacts {
            run_id: RunId::generate(),
            task: task.clone(),
            branch: branch.clone(),
            repos: repos.clone(),
            multi: true,
            runtimes: Vec::new(),
            trigger: serde_json::Value::Null,
            prs: Vec::new(),
            artifacts: Vec::new(),
        };
        let worktree = Worktree::Repos {
            task_id: task.id,
            repos: ["web", "api"]
                .iter()
                .zip(&repos)
                .map(|(name, r)| RepoCheckout {
                    name: (*name).into(),
                    url: r.url.clone(),
                    base: None,
                    branch: branch.clone(),
                })
                .collect(),
        };
        let vcs = crate::vcs::Vcs::new(store.clone());
        let mut tx = store.read().await.unwrap();
        let job = Job::new(
            &mut tx,
            HostId::generate(),
            &facts,
            worktree,
            "github",
            &vcs,
        )
        .await
        .unwrap()
        .unwrap();
        drop(tx);
        let host = Recorder::default();
        job.run(&host).await.unwrap();
        let pushes = host.0.lock().unwrap().clone();
        assert_eq!(
            pushes,
            vec![
                ("/session/web".into(), Some("ghs_minted_for_web".into())),
                ("/session/api".into(), Some("glpat-for-api".into())),
            ]
        );
    }
}
