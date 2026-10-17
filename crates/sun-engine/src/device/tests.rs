//! The device verify step end to end: the real engine and store, a host fake
//! that answers `verify_on_device`, and a mod fake that records the PR
//! attachment delivered from the outbox.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::json;
use sun_core::host_protocol::{Screenshot, Worktree};
use sun_core::{
    BlobHash, BoxStream, Error, HostEvent, HostId, ModCall, RunStatus, SessionHandle, SessionId,
    SessionSpec, Steer, StepStatus, StopMode,
};
use sun_store::{HostRepo, Store};
use tokio::sync::mpsc;

use super::*;
use crate::gates::tests::{Mods, Receiver, events, mods, seed, seed_with, steps, until_run_ends};
use crate::{Engine, EngineConfig};

type Answer = Box<dyn Fn(&DeviceRequest) -> Result<DeviceResult> + Send + Sync>;

/// A host with one task dir that answers every verify with `answer`.
struct DeviceHost {
    work: PathBuf,
    answer: Answer,
    requests: Mutex<Vec<DeviceRequest>>,
    /// Keeps the event stream open.
    sink: Mutex<Option<mpsc::UnboundedSender<HostEvent>>>,
}

#[async_trait]
impl HostLink for DeviceHost {
    async fn start_session(&self, _: HostId, _: &str, _: SessionSpec) -> Result<SessionHandle> {
        Err(Error::Unavailable("no agents here".into()))
    }

    async fn steer(&self, _: &SessionHandle, _: Steer) -> Result<()> {
        Ok(())
    }

    async fn stop_session(&self, _: &SessionHandle, _: StopMode) -> Result<()> {
        Ok(())
    }

    async fn is_alive(&self, _: &SessionHandle) -> Result<bool> {
        Ok(false)
    }

    fn events(&self) -> BoxStream<'static, HostEvent> {
        let (tx, rx) = mpsc::unbounded_channel();
        *self.sink.lock().unwrap() = Some(tx);
        Box::pin(Receiver(rx))
    }

    async fn ack(&self, _: SessionId, _: u64) -> Result<()> {
        Ok(())
    }

    async fn prepare_worktree(&self, _: HostId, _: Worktree) -> Result<PathBuf> {
        Ok(self.work.clone())
    }

    /// The `open_pr` push before the verify succeeds.
    async fn exec(
        &self,
        _: HostId,
        _: sun_core::host_protocol::ExecRequest,
    ) -> Result<sun_core::host_protocol::ExecOutput> {
        Ok(sun_core::host_protocol::ExecOutput {
            code: Some(0),
            output: String::new(),
            timed_out: false,
        })
    }

    async fn verify_on_device(&self, _: HostId, request: DeviceRequest) -> Result<DeviceResult> {
        self.requests.lock().unwrap().push(request.clone());
        (self.answer)(&request)
    }
}

async fn start(
    dir: &Path,
    answer: impl Fn(&DeviceRequest) -> Result<DeviceResult> + Send + Sync + 'static,
) -> (Store, Arc<DeviceHost>, Arc<Mods>, Engine) {
    let store = Store::open(dir).await.unwrap();
    let host = Arc::new(DeviceHost {
        work: dir.join("work/task"),
        answer: Box::new(answer),
        requests: Mutex::default(),
        sink: Mutex::default(),
    });
    let mods = mods(json!({ "url": "https://example.test/pr/1#comment" }));
    // Placement picks a host whose device tool and Maestro are ready.
    let id = HostId::generate();
    let mut tx = store.begin().await.unwrap();
    tx.insert_host(&sun_core::model::Host {
        id,
        name: "mac".into(),
        capabilities: json!({ "devices": {
            "ios_simulator": { "ready": true }, "android_emulator": { "ready": true },
            "maestro": { "ready": true } } }),
        enrolled_by: None,
    })
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let config = EngineConfig {
        allow_bypass: false,
        host: id,
        adapter: "fake".into(),
        delivery_timeout: Duration::from_secs(5),
        watchdog: crate::WatchdogConfig::default(),
    };
    let (engine, _task) = Engine::start(store.clone(), host.clone(), mods.clone(), config)
        .await
        .unwrap();
    (store, host, mods, engine)
}

fn shots(passed: bool) -> DeviceResult {
    DeviceResult {
        device: "iPhone 15 (iOS 17.0)".into(),
        passed,
        output: "Flow output".into(),
        screenshots: vec![
            Screenshot {
                name: "login.png".into(),
                png: b"PNGlogin".to_vec(),
            },
            Screenshot {
                name: "sun-final.png".into(),
                png: b"PNGfinal".to_vec(),
            },
        ],
    }
}

/// [`seed`] for a task with a GitHub repo (the PR the screenshots go to).
async fn seed_repo(store: &Store, yaml: &str) -> sun_core::RunId {
    let repo = crate::gates::tests::repo("https://github.com/acme/app.git");
    let mut tx = store.begin().await.unwrap();
    sun_store::RepoRepo::insert_repo(&mut tx, &repo)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    seed_with(store, yaml, vec![repo.id]).await
}

/// Waits until the outbox delivered `n` `attach_to_pr` calls.
async fn calls(mods: &Mods, n: usize) -> Vec<ModCall> {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let calls: Vec<ModCall> = mods
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c.method == "attach_to_pr")
            .cloned()
            .collect();
        if calls.len() >= n {
            return calls;
        }
        assert!(Instant::now() < deadline, "only {} mod calls", calls.len());
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

const IOS: &str = r#"
name: mobile
steps:
  - id: pr
    type: action
    use: github.open_pr
  - id: verify_ios
    type: device.verify_on_device
    with:
      platform: ios
      device: iPhone 15
      build: make simulator-build
      app: build/App.app
      flow: .maestro/smoke.yaml
"#;

#[tokio::test(flavor = "multi_thread")]
async fn device_step_stores_screenshots_and_attaches_them_through_the_git_host() {
    let dir = tempfile::tempdir().unwrap();
    let (store, host, mods, engine) = start(dir.path(), |_| Ok(shots(true))).await;
    let run = seed_repo(&store, IOS).await;
    engine.start_run(run).await.unwrap();
    assert_eq!(
        until_run_ends(&store, run).await.status,
        RunStatus::Succeeded
    );

    let request = host.requests.lock().unwrap()[0].clone();
    assert_eq!(request.cwd, dir.path().join("work/task"));
    assert_eq!(request.platform, DevicePlatform::Ios);
    assert_eq!(request.device.as_deref(), Some("iPhone 15"));
    assert_eq!(request.build.as_deref(), Some("make simulator-build"));
    assert_eq!(request.app, Path::new("build/App.app"));
    assert_eq!(request.flow, Path::new(".maestro/smoke.yaml"));
    assert_eq!(request.timeout_s, DEFAULT_TIMEOUT_S);

    // Screenshots are blobs; the timeline event cites them.
    let results = events(&store, run, DEVICE_RESULT).await;
    assert_eq!(results.len(), 1);
    let result = &results[0];
    assert_eq!(result["passed"], true);
    assert_eq!(result["device"], "iPhone 15 (iOS 17.0)");
    let first = &result["screenshots"][0];
    assert_eq!(first["name"], "login.png");
    let blob = BlobHash::new(first["blob"].as_str().unwrap()).unwrap();
    assert_eq!(store.blobs().get(&blob).await.unwrap(), b"PNGlogin");

    // The git host plugin gets them, from the outbox, with an idem key.
    let calls = calls(&mods, 1).await;
    assert_eq!(calls.len(), 1);
    let call = &calls[0];
    assert_eq!(
        (call.mod_name.as_str(), call.method.as_str()),
        ("github", "attach_to_pr")
    );
    assert!(call.idem_key.is_some());
    assert_eq!(call.params["passed"], true);
    assert_eq!(call.params["repo"], "acme/app");
    // The outbox inlined the blob bytes (base64 of "PNGlogin").
    assert_eq!(
        call.params["files"][0],
        json!({ "name": "login.png", "content_base64": "UE5HbG9naW4=", "content_type": "image/png" })
    );
    let task_id = call.params["task_id"].as_str().unwrap().parse().unwrap();
    assert_eq!(
        call.params["branch"],
        crate::gates::tests::branch_of(&store, task_id).await
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn device_flow_failure_fails_the_step_and_still_attaches_the_screens() {
    let dir = tempfile::tempdir().unwrap();
    let (store, _host, mods, engine) = start(dir.path(), |_| Ok(shots(false))).await;
    let yaml = IOS.replace("platform: ios", "platform: android").replace(
        "      device: iPhone 15\n",
        "      mod: gitlab\n      timeout_s: 60\n",
    );
    let run = seed_repo(&store, &yaml).await;
    engine.start_run(run).await.unwrap();

    let ended = until_run_ends(&store, run).await;
    assert_eq!(ended.status, RunStatus::Failed);
    assert_eq!(steps(&store, run).await[1].status, StepStatus::Failed);
    let call = calls(&mods, 1).await.remove(0);
    assert_eq!(
        (call.mod_name.as_str(), call.method.as_str()),
        ("gitlab", "attach_to_pr")
    );
    assert_eq!(call.params["passed"], false);
    assert!(
        call.params["title"]
            .as_str()
            .unwrap()
            .starts_with("Device verify failed")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn device_verify_without_a_capable_host_or_with_bad_config_fails_clearly() {
    let dir = tempfile::tempdir().unwrap();
    let (store, host, mods, engine) = start(dir.path(), |_| {
        Err(Error::Unavailable(
            "Maestro not found; install Maestro".into(),
        ))
    })
    .await;
    let run = seed(&store, IOS).await;
    engine.start_run(run).await.unwrap();
    assert_eq!(until_run_ends(&store, run).await.status, RunStatus::Failed);
    let results = events(&store, run, DEVICE_RESULT).await;
    assert!(
        results[0]["error"]
            .as_str()
            .unwrap()
            .contains("install Maestro")
    );

    // `app` is missing: a config error, the host is never asked.
    let run = seed(&store, &IOS.replace("      app: build/App.app\n", "")).await;
    engine.start_run(run).await.unwrap();
    assert_eq!(until_run_ends(&store, run).await.status, RunStatus::Failed);
    assert_eq!(host.requests.lock().unwrap().len(), 1);
    assert!(events(&store, run, DEVICE_RESULT).await.is_empty());
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        !mods
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|c| c.method == "attach_to_pr"),
        "nothing to attach"
    );

    // No host has Maestro ready: the step fails before any host is asked.
    let mut tx = store.begin().await.unwrap();
    let id = tx.hosts().await.unwrap()[0].id;
    let caps = json!({ "devices": { "ios_simulator": { "ready": true } } });
    tx.set_host_capabilities(id, &caps).await.unwrap();
    tx.commit().await.unwrap();
    let run = seed(&store, IOS).await;
    engine.start_run(run).await.unwrap();
    assert_eq!(until_run_ends(&store, run).await.status, RunStatus::Failed);
    assert_eq!(host.requests.lock().unwrap().len(), 1);
    let moves = events(&store, run, "step_status").await;
    assert!(
        moves.iter().any(|m| m["reason"]
            .as_str()
            .is_some_and(|r| r.contains("no host with the iOS Simulator"))),
        "{moves:#?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn failed_device_verify_is_sent_back_with_its_output_until_max() {
    let dir = tempfile::tempdir().unwrap();
    let (store, host, _mods, engine) = start(dir.path(), |_| Ok(shots(false))).await;
    let yaml = r#"
name: mobile
steps:
  - id: pr
    type: action
    use: github.open_pr
  - id: approve
    type: approval
    who: [role:admin]
  - id: verify
    type: device.verify_on_device
    needs: [approve]
    with:
      platform: ios
      app: build/App.app
      flow: .maestro/smoke.yaml
      on_fail: { send_back_to: approve, max: 1 }
"#;
    let run = seed(&store, yaml).await;
    engine.start_run(run).await.unwrap();
    let admin = crate::Principal {
        user: "ann".into(),
        roles: vec!["admin".into()],
    };
    let approve = sun_core::StepKey::new("approve").unwrap();
    // Approves the latest `approve` attempt once it waits.
    let decide = || async {
        let deadline = Instant::now() + Duration::from_secs(20);
        while engine
            .decide(run, approve.clone(), true, admin.clone())
            .await
            .is_err()
        {
            assert!(Instant::now() < deadline, "approve never waited");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };

    // The failed flow goes back to `approve` with its output as feedback.
    decide().await;
    decide().await;
    let sent = events(&store, run, "sent_back").await;
    assert_eq!(sent.len(), 1, "{sent:#?}");
    assert_eq!(sent[0]["from"], "verify");
    let feedback = sent[0]["feedback"].as_str().unwrap();
    assert!(feedback.contains("send-back 1 of 1"), "{feedback}");
    assert!(feedback.contains("Flow output"), "{feedback}");

    // The second failure is past `max`: the step and the run fail.
    assert_eq!(until_run_ends(&store, run).await.status, RunStatus::Failed);
    assert_eq!(host.requests.lock().unwrap().len(), 2);
    let moves = events(&store, run, "step_status").await;
    assert!(
        moves.iter().any(|m| m["reason"]
            .as_str()
            .is_some_and(|r| r.ends_with("after 1 send-back(s)"))),
        "{moves:#?}"
    );
}
