//! Workflows and prompts from the task's repo: git is the
//! source, a `workflow_version` row is a cache.
//!
//! At run start ([`read`], before the start's transaction) the engine reads
//! `.sun/workflows/*.yaml` (and `.yml`) in the run's worktree (a multi-repo
//! task: its first repo). The file whose `name:` is the run's workflow wins;
//! each agent `prompt:` that names a file ([`sun_workflow::is_prompt_file`])
//! is read under `.sun/` and inlined. [`pin`] then, in the start's
//! transaction, saves that source as the next version when it differs from
//! the pinned one, pins the run to it and journals `workflow_source`.
//!
//! No repo, no matching file, or a host that can't be asked: the run keeps
//! its pinned version. A matching file that doesn't parse fails the run.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::json;
use sun_core::host_protocol::{ExecRequest, Worktree};
use sun_core::model::Run;
use sun_core::{ExecEnvSpec, HostId, HostLink, PermissionMode, Result};
use sun_store::{EventLog, NewEvent, RunRepo, Tx, WorkflowRepo};
use sun_workflow::{StepKind, Workflow};

use crate::gates::quote;

/// Event kind; payload `{path, version, prompt_files, prompts, missing}`
/// (`prompts`: step id → prompt file, for memory PRs).
pub(crate) const WORKFLOW_SOURCE: &str = "workflow_source";
const TIMEOUT_S: u64 = 30;
const LIST: &str = "for f in .sun/workflows/*.yaml .sun/workflows/*.yml; \
                    do [ -f \"$f\" ] && printf '%s\\n' \"$f\"; done; true";

/// The run's workflow as the repo has it.
pub(crate) struct Source {
    /// The file, relative to the repo root.
    path: String,
    /// Its YAML with the prompt files inlined.
    yaml: String,
    workflow: Workflow,
    /// Prompt files read (under `.sun/`), by step id.
    prompt_files: BTreeMap<String, String>,
    /// Prompt paths with no such file: they stay as written.
    missing: Vec<String>,
}

/// Reads workflow `name` from the task's repo on `host`. `Ok(None)`: the
/// pinned version runs; `Err`: the repo's file is invalid.
pub(crate) async fn read(
    hosts: &dyn HostLink,
    host: HostId,
    worktree: &Worktree,
    name: &str,
) -> std::result::Result<Option<Source>, String> {
    let Some(cwd) = repo_dir(hosts, host, worktree).await else {
        return Ok(None);
    };
    let Some(list) = cat(hosts, host, &cwd, LIST.into()).await else {
        return Ok(None);
    };
    // ponytail: one exec per file; batch them if repos grow many workflows.
    for path in list.lines() {
        let Some(yaml) = cat(hosts, host, &cwd, format!("cat -- {}", quote(path))).await else {
            continue;
        };
        let stem = Path::new(path).file_stem().and_then(|s| s.to_str());
        match sun_workflow::parse(&yaml) {
            Ok(workflow) if workflow.name == name => {
                return resolve(hosts, host, &cwd, path, yaml, workflow)
                    .await
                    .map(Some);
            }
            Err(errors) if stem == Some(name) => return Err(invalid(path, &errors)),
            _ => {}
        }
    }
    Ok(None)
}

/// Inlines the prompt files of `workflow` read from `path`.
async fn resolve(
    hosts: &dyn HostLink,
    host: HostId,
    cwd: &Path,
    path: &str,
    yaml: String,
    workflow: Workflow,
) -> std::result::Result<Source, String> {
    let mut texts = BTreeMap::new();
    let (mut prompt_files, mut missing) = (BTreeMap::new(), Vec::new());
    for step in &workflow.steps {
        let StepKind::Agent { prompt, .. } = &step.kind else {
            continue;
        };
        if !sun_workflow::is_prompt_file(prompt) {
            continue;
        }
        let file = format!(".sun/{prompt}");
        match cat(hosts, host, cwd, format!("cat -- {}", quote(&file))).await {
            Some(text) => {
                texts.insert(step.id.as_str().to_owned(), text);
                prompt_files.insert(step.id.as_str().to_owned(), file);
            }
            None => missing.push(file),
        }
    }
    let mut source = Source {
        path: path.to_owned(),
        yaml,
        workflow,
        prompt_files,
        missing,
    };
    if !texts.is_empty() {
        source.yaml =
            sun_workflow::inline_prompts(&source.yaml, &texts).map_err(|e| invalid(path, &e))?;
        source.workflow = sun_workflow::parse(&source.yaml).map_err(|e| invalid(path, &e))?;
    }
    Ok(source)
}

/// Saves `source` as the next version of the run's workflow unless the
/// pinned or the latest version already is it, pins the run to it and
/// journals `workflow_source`. In the start's transaction. `Ok(Err(why))`:
/// the file raises a step to `bypass_permissions`, which only a save
/// through Sun may do (`manage_runtimes`, `runtimes.allow_bypass`); the run
/// fails.
pub(crate) async fn pin(
    tx: &mut Tx,
    run: &Run,
    source: Source,
) -> Result<std::result::Result<(), String>> {
    let same = |yaml: &str| sun_workflow::parse(yaml).is_ok_and(|w| w.hash == source.workflow.hash);
    let pinned = tx
        .get_workflow_version(run.workflow_id, run.workflow_version)
        .await?;
    if let Some(step) = raised_to_bypass(&pinned.source_yaml, &source.workflow) {
        return Ok(Err(format!(
            "{}: step `{step}` sets permission_mode: bypass_permissions and the saved \
             workflow doesn't; save it in Sun to allow that",
            source.path
        )));
    }
    let version = if same(&pinned.source_yaml) {
        pinned.version
    } else {
        match tx.latest_workflow_version(run.workflow_id).await? {
            Some(latest) if same(&latest.source_yaml) => latest.version,
            _ => {
                tx.save_workflow_version(run.workflow_id, &source.yaml, &source.workflow.hash)
                    .await?
                    .version
            }
        }
    };
    if version != run.workflow_version {
        tx.repin_run(run.id, version, &source.workflow.budget)
            .await?;
    }
    let payload = json!({
        "path": source.path,
        "version": version,
        "prompt_files": source.prompt_files.values().collect::<Vec<_>>(),
        "prompts": source.prompt_files,
        "missing": source.missing,
    });
    tx.append_event(NewEvent::new(run.id, None, WORKFLOW_SOURCE, payload))
        .await?;
    Ok(Ok(()))
}

/// The first step of `repo` in `bypass_permissions` whose namesake in the
/// `pinned` YAML isn't.
fn raised_to_bypass<'a>(pinned: &str, repo: &'a Workflow) -> Option<&'a str> {
    let pinned = sun_workflow::parse(pinned).ok();
    let bypass = Some(PermissionMode::BypassPermissions);
    repo.steps
        .iter()
        .find(|s| {
            repo.permission_mode(&s.id) == bypass
                && pinned
                    .as_ref()
                    .is_none_or(|p| p.permission_mode(&s.id) != bypass)
        })
        .map(|s| s.id.as_str())
}

/// `.sun/exec_env.yaml`: the toolchain the repo declares. The
/// workflow or task still picks the kind and caps; this fills in the
/// container `image` or VM `template` for it.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RepoExecEnv {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    image: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    template: Option<String>,
}

const EXEC_ENV_FILE: &str = ".sun/exec_env.yaml";

impl RepoExecEnv {
    /// `spec` with the repo's image or template; a bare or extension env
    /// stays as is.
    pub(crate) fn apply(&self, spec: &mut ExecEnvSpec) {
        match spec {
            ExecEnvSpec::Container(c) => {
                if let Some(image) = &self.image {
                    c.image.clone_from(image);
                }
            }
            ExecEnvSpec::Vm(vm) => {
                if let Some(template) = &self.template {
                    vm.template.clone_from(template);
                }
            }
            ExecEnvSpec::Bare | ExecEnvSpec::Extension(_) => {}
        }
    }
}

/// Reads [`EXEC_ENV_FILE`] from the task's repo on `host`. `Ok(None)`: no
/// repo or no file; `Err`: the file is invalid (it fails the run).
pub(crate) async fn read_exec_env(
    hosts: &dyn HostLink,
    host: HostId,
    worktree: &Worktree,
) -> std::result::Result<Option<RepoExecEnv>, String> {
    let Some(cwd) = repo_dir(hosts, host, worktree).await else {
        return Ok(None);
    };
    let command = format!("[ -f {0} ] && cat -- {0} || true", quote(EXEC_ENV_FILE));
    let Some(yaml) = cat(hosts, host, &cwd, command).await else {
        return Ok(None);
    };
    if yaml.trim().is_empty() {
        return Ok(None);
    }
    let invalid = |e: String| format!("{EXEC_ENV_FILE}: {e}");
    let env: RepoExecEnv = serde_saphyr::from_str(&yaml).map_err(|e| invalid(e.to_string()))?;
    // The same name rules as a workflow's `exec_env:`.
    let mut container = ExecEnvSpec::Container(sun_core::ContainerSpec {
        image: "x".into(),
        memory_mb: None,
        cpu_millis: None,
        network: None,
    });
    let mut vm = ExecEnvSpec::Vm(sun_core::VmSpec {
        template: "x".into(),
        memory_mb: None,
        cpus: None,
        network: None,
        keep: false,
    });
    env.apply(&mut container);
    env.apply(&mut vm);
    container.validate().map_err(|e| invalid(e.to_string()))?;
    vm.validate().map_err(|e| invalid(e.to_string()))?;
    Ok(Some(env))
}

/// The task's repo checkout on `host` (a multi-repo task: its first repo).
/// `None`: no repo, or the host can't prepare it.
pub(crate) async fn repo_dir(
    hosts: &dyn HostLink,
    host: HostId,
    worktree: &Worktree,
) -> Option<PathBuf> {
    let first = match worktree {
        Worktree::Folder { .. } => return None,
        Worktree::Repo { .. } => None,
        Worktree::Repos { repos, .. } => repos.first().map(|r| r.name.clone()),
    };
    let root = hosts.prepare_worktree(host, worktree.clone()).await.ok()?;
    Some(first.map_or(root.clone(), |name| root.join(name)))
}

/// Runs `command` in `cwd`; its output when it exited 0.
pub(crate) async fn cat(
    hosts: &dyn HostLink,
    host: HostId,
    cwd: &Path,
    command: String,
) -> Option<String> {
    let request = ExecRequest {
        cwd: PathBuf::from(cwd),
        command,
        env: Default::default(),
        timeout_s: TIMEOUT_S,
    };
    let output = hosts.exec(host, request).await.ok()?;
    (output.code == Some(0)).then_some(output.output)
}

fn invalid(path: &str, errors: &[sun_workflow::WorkflowError]) -> String {
    let errors: Vec<String> = errors.iter().map(|e| format!("{path}:{e}")).collect();
    errors.join("; ")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use async_trait::async_trait;
    use serde_json::{Value, json};
    use sun_core::{Error, ModCall, ModLink, RunId, RunStatus, StepKey, StepStatus, TaskId};
    use sun_store::{RepoRepo, Store};

    use super::*;
    use crate::gates::tests::{LocalHost, events, seed, seed_with, steps, until_run_ends};
    use crate::{Engine, EngineConfig};

    async fn engine(store: &Store, host: Arc<LocalHost>, mods: Arc<dyn ModLink>) -> Engine {
        let config = EngineConfig {
            host: HostId::generate(),
            adapter: "fake".into(),
            delivery_timeout: Duration::from_secs(5),
            watchdog: crate::WatchdogConfig::default(),
        };
        Engine::start(store.clone(), host, mods, config)
            .await
            .unwrap()
            .0
    }

    /// The echo test mod's `echo` and `fail` actions, in process.
    #[derive(Default)]
    struct Echo {
        calls: Mutex<Vec<ModCall>>,
    }

    #[async_trait]
    impl ModLink for Echo {
        async fn call(&self, call: ModCall) -> Result<Value> {
            self.calls.lock().unwrap().push(call.clone());
            match call.method.as_str() {
                "echo" => Ok(json!({ "params": call.params, "idem_key": call.idem_key })),
                _ => Err(Error::InvalidInput("mod echo: boom".into())),
            }
        }
    }

    const REPO_FIX: &str = "name: fix
steps:
  - id: implement
    type: agent
    prompt: prompts/implement.md
";

    #[tokio::test(flavor = "multi_thread")]
    async fn repo_workflow_and_prompt_file_are_used() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).await.unwrap();
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let sink = seen.clone();
        let host = LocalHost::new(dir.path(), move |_, envelope| {
            sink.lock().unwrap().push(envelope.to_owned());
        });
        // The repo has its own `fix` workflow and prompt file.
        let task_id = TaskId::generate();
        let work = host
            .prepare_worktree(HostId::generate(), Worktree::Folder { task_id })
            .await
            .unwrap();
        std::fs::create_dir_all(work.join(".sun/workflows")).unwrap();
        std::fs::create_dir_all(work.join(".sun/prompts")).unwrap();
        std::fs::write(work.join(".sun/workflows/fix.yaml"), REPO_FIX).unwrap();
        std::fs::write(
            work.join(".sun/workflows/other.yml"),
            "name: other\nsteps: []\n",
        )
        .unwrap();
        std::fs::write(
            work.join(".sun/prompts/implement.md"),
            "Fix it the way the repo says.\n",
        )
        .unwrap();
        let repo = crate::gates::tests::repo("https://github.com/acme/app.git");
        let mut tx = store.begin().await.unwrap();
        tx.insert_repo(&repo).await.unwrap();
        tx.commit().await.unwrap();
        // The database has an older copy.
        let stale = "name: fix\nsteps:\n  - id: implement\n    type: agent\n    prompt: the stale db prompt\n";
        let run_id = seed_with(&store, stale, vec![repo.id]).await;
        let engine = engine(&store, host, Arc::new(Echo::default())).await;

        engine.start_run(run_id).await.unwrap();
        let run = until_run_ends(&store, run_id).await;
        assert_eq!(run.status, RunStatus::Succeeded, "{:?}", run.end_reason);
        let envelopes = seen.lock().unwrap().clone();
        assert_eq!(envelopes.len(), 1);
        assert!(envelopes[0].contains("Fix it the way the repo says."));
        assert!(!envelopes[0].contains("stale"));
        // The repo's file became version 2 and the run is pinned to it.
        assert_eq!(run.workflow_version, 2);
        let source = events(&store, run_id, WORKFLOW_SOURCE).await;
        assert_eq!(
            source,
            [json!({ "path": ".sun/workflows/fix.yaml", "version": 2,
                     "prompt_files": [".sun/prompts/implement.md"],
                     "prompts": { "implement": ".sun/prompts/implement.md" }, "missing": [] })]
        );

        // The next run of the same file reuses the cached version 2.
        let again = Run {
            id: RunId::generate(),
            status: RunStatus::Queued,
            workflow_version: 1,
            ..run
        };
        let mut tx = store.begin().await.unwrap();
        tx.insert_run(&again).await.unwrap();
        tx.commit().await.unwrap();
        engine.start_run(again.id).await.unwrap();
        assert_eq!(until_run_ends(&store, again.id).await.workflow_version, 2);
        let mut read = store.read().await.unwrap();
        let latest = read.latest_workflow_version(run.workflow_id).await.unwrap();
        assert_eq!(latest.unwrap().version, 2);
        drop(read);

        // A broken file in git fails the run instead of running a stale copy.
        std::fs::write(
            work.join(".sun/workflows/fix.yaml"),
            "name: fix\nsteps: 3\n",
        )
        .unwrap();
        let broken = Run {
            id: RunId::generate(),
            ..again
        };
        let mut tx = store.begin().await.unwrap();
        tx.insert_run(&broken).await.unwrap();
        tx.commit().await.unwrap();
        engine.start_run(broken.id).await.unwrap();
        let ended = until_run_ends(&store, broken.id).await;
        assert_eq!(ended.status, RunStatus::Failed);
        assert!(
            ended
                .end_reason
                .unwrap()
                .starts_with(".sun/workflows/fix.yaml:")
        );
    }

    /// The workflow picks a container; the repo's
    /// `.sun/exec_env.yaml` names the image every session of it runs in.
    #[tokio::test(flavor = "multi_thread")]
    async fn repo_declared_image_is_used() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).await.unwrap();
        let host = LocalHost::new(dir.path(), |_, _| {});
        let work = host
            .prepare_worktree(
                HostId::generate(),
                Worktree::Folder {
                    task_id: TaskId::generate(),
                },
            )
            .await
            .unwrap();
        std::fs::create_dir_all(work.join(".sun")).unwrap();
        std::fs::write(
            work.join(".sun/exec_env.yaml"),
            "image: ghcr.io/acme/toolchain:1.4\ntemplate: ubuntu-lts\n",
        )
        .unwrap();
        let repo = crate::gates::tests::repo("https://github.com/acme/app.git");
        let mut tx = store.begin().await.unwrap();
        tx.insert_repo(&repo).await.unwrap();
        tx.commit().await.unwrap();
        let yaml = "name: build
exec_env: { kind: container, image: 'node:22', memory_mb: 2048 }
steps:
  - id: implement
    type: agent
    prompt: build it
";
        let run_id = seed_with(&store, yaml, vec![repo.id]).await;
        let engine = engine(&store, host, Arc::new(Echo::default())).await;

        engine.start_run(run_id).await.unwrap();
        let run = until_run_ends(&store, run_id).await;
        assert_eq!(run.status, RunStatus::Succeeded, "{:?}", run.end_reason);
        let assigned = events(&store, run_id, crate::executor::kind::SESSION_ASSIGNED).await;
        assert_eq!(
            assigned[0]["exec_env"],
            json!({ "kind": "container", "image": "ghcr.io/acme/toolchain:1.4",
                    "memory_mb": 2048, "cpu_millis": null, "network": null })
        );

        // A bad file fails the run instead of running the wrong toolchain.
        std::fs::write(work.join(".sun/exec_env.yaml"), "image: '-rm'\n").unwrap();
        let again = Run {
            id: RunId::generate(),
            status: RunStatus::Queued,
            ..run
        };
        let mut tx = store.begin().await.unwrap();
        tx.insert_run(&again).await.unwrap();
        tx.commit().await.unwrap();
        engine.start_run(again.id).await.unwrap();
        let ended = until_run_ends(&store, again.id).await;
        assert_eq!(ended.status, RunStatus::Failed);
        assert!(ended.end_reason.unwrap().starts_with(".sun/exec_env.yaml:"));
    }

    /// BLO-139: a workflow file in the repo can't raise a step to
    /// `bypass_permissions`. Saving that step through the API needs
    /// `manage_runtimes` and `runtimes.allow_bypass`; whoever can push to the
    /// repo (or an earlier session that edited it) must not get a bypass
    /// session past that gate.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_repo_workflow_file_cannot_raise_a_step_to_bypass() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).await.unwrap();
        let sessions = Arc::new(Mutex::new(0_usize));
        let count = sessions.clone();
        let host = LocalHost::new(dir.path(), move |_, _| *count.lock().unwrap() += 1);
        let work = host
            .prepare_worktree(
                HostId::generate(),
                Worktree::Folder {
                    task_id: TaskId::generate(),
                },
            )
            .await
            .unwrap();
        // A container, so the bare-host refusal doesn't hide the mode.
        let pinned = "name: fix
exec_env: { kind: container, image: 'node:22' }
steps:
  - id: implement
    type: agent
    prompt: fix it
";
        std::fs::create_dir_all(work.join(".sun/workflows")).unwrap();
        std::fs::write(
            work.join(".sun/workflows/fix.yaml"),
            format!("{pinned}    permission_mode: bypass_permissions\n"),
        )
        .unwrap();
        let repo = crate::gates::tests::repo("https://github.com/acme/app.git");
        let mut tx = store.begin().await.unwrap();
        tx.insert_repo(&repo).await.unwrap();
        tx.commit().await.unwrap();
        let run_id = seed_with(&store, pinned, vec![repo.id]).await;
        let engine = engine(&store, host, Arc::new(Echo::default())).await;

        engine.start_run(run_id).await.unwrap();
        let run = until_run_ends(&store, run_id).await;
        let mut read = store.read().await.unwrap();
        let ran = read
            .get_workflow_version(run.workflow_id, run.workflow_version)
            .await
            .unwrap();
        let step = StepKey::new("implement").unwrap();
        let mode = sun_workflow::parse(&ran.source_yaml)
            .unwrap()
            .permission_mode(&step);
        let started = *sessions.lock().unwrap();
        assert!(
            !(started > 0 && mode == Some(sun_core::PermissionMode::BypassPermissions)),
            "{started} session(s) ran on version {} whose step is bypass_permissions, \
             taken from the repo's .sun/workflows/fix.yaml",
            run.workflow_version
        );
        // Refused with the reason, before any session; the pinned version stays.
        assert_eq!(
            (run.status, started, run.workflow_version),
            (RunStatus::Failed, 0, 1)
        );
        let reason = run.end_reason.unwrap_or_default();
        assert!(
            reason.starts_with(".sun/workflows/fix.yaml: step `implement` sets permission_mode"),
            "{reason}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn extension_step_runs_through_the_echo_mod() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).await.unwrap();
        let mods = Arc::new(Echo::default());
        let host = LocalHost::new(dir.path(), |_, _| {});
        let engine = engine(&store, host, mods.clone()).await;
        let yaml = "name: ext
steps:
  - id: shot
    type: echo.echo
    with: { url: https://example.test }
  - id: boom
    type: echo.fail
    needs: [shot]
";
        let run_id = seed(&store, yaml).await;

        engine.start_run(run_id).await.unwrap();
        let run = until_run_ends(&store, run_id).await;
        assert_eq!(run.status, RunStatus::Failed);
        let rows = steps(&store, run_id).await;
        let status = |key: &str| {
            let key = StepKey::new(key).unwrap();
            rows.iter().find(|s| s.step_key == key).map(|s| s.status)
        };
        assert_eq!(status("shot"), Some(StepStatus::Succeeded));
        assert_eq!(status("boom"), Some(StepStatus::Failed));

        let calls = mods.calls.lock().unwrap().clone();
        assert_eq!(calls.len(), 2);
        assert_eq!(
            (calls[0].mod_name.as_str(), calls[0].method.as_str()),
            ("echo", "echo")
        );
        assert_eq!(
            calls[0].params["with"],
            json!({ "url": "https://example.test" })
        );
        assert_eq!(
            calls[0].idem_key.as_ref().map(|k| k.as_str().to_owned()),
            Some(format!("{run_id}:shot:echo.echo#1"))
        );
        let results = events(&store, run_id, crate::executor::kind::EXTENSION_RESULT).await;
        assert_eq!(
            results[0]["output"]["params"]["with"]["url"],
            "https://example.test"
        );
        assert!(results[1]["error"].as_str().unwrap().contains("boom"));
    }
}
