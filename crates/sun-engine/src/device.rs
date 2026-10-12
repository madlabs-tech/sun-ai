//! The `device.verify_on_device` extension step: check the agent's
//! change on a simulator or emulator.
//!
//! ```yaml
//! - id: verify_ios
//!   type: device.verify_on_device
//!   with:
//!     platform: ios            # or android
//!     device: iPhone 15        # optional; default: the host's first
//!     build: xcodebuild -scheme App -sdk iphonesimulator -derivedDataPath build
//!     app: build/Build/Products/Debug-iphonesimulator/App.app
//!     flow: .maestro/smoke.yaml
//!     timeout_s: 1800          # optional
//!     mod: github              # git host plugin for the PR; default github
//!     on_fail: { send_back_to: implement, max: 2 }   # optional, like a gate
//! ```
//!
//! The workflow must run a `<mod>.open_pr` step before it (sun-workflow
//! checks this), so the PR exists when the screenshots are attached.
//!
//! Lifecycle: `queued → running`; the verify runs off the engine task as a
//! [`Job`] (`prepare_worktree`, then [`HostLink::verify_on_device`]). Then,
//! in one transaction, the executor journals `device_result` (screenshots
//! already stored as blobs), enqueues `<mod>.attach_to_pr` in the outbox when
//! there are screenshots and the task has a repo, and moves the step: `succeeded` if the flow passed,
//! else `failed`, or, with `on_fail`, back to `running` while the work is sent
//! back to the agent with the build and flow output (`gates::send_back`; a
//! missing tool or a device that won't boot is never sent back). The git host plugin attaches the screenshots to the PR; an
//! extension never calls a third party itself.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::Deserialize;
use serde_json::{Value, json};
use sun_core::host_protocol::{DevicePlatform, DeviceRequest, DeviceResult, Worktree};
use sun_core::model::{OutboxItem, OutboxStatus, Run, Step};
use sun_core::{BlobHash, HostId, HostLink, IdemKey, OutboxId, Result, SessionId, StepKey};
use sun_store::{BlobStore, HostRepo, OutboxRepo, Tx};
use sun_workflow::{OnFail, Workflow};

use crate::executor::append;
use crate::gates::{self, Verdict};
use crate::outbox::{RunFacts, repo_slug};

/// The step's `type:`.
pub(crate) const STEP_TYPE: &str = "device.verify_on_device";
/// Event kind: the verify's outcome, on the step.
const DEVICE_RESULT: &str = "device_result";
/// Seconds for build, boot, install and flow; override with `timeout_s`.
const DEFAULT_TIMEOUT_S: u64 = 1800;

/// The step's `with:`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct With {
    platform: DevicePlatform,
    #[serde(default)]
    device: Option<String>,
    #[serde(default)]
    build: Option<String>,
    app: PathBuf,
    flow: PathBuf,
    #[serde(default)]
    timeout_s: Option<u64>,
    /// Git host plugin that attaches the screenshots to the PR.
    #[serde(default, rename = "mod")]
    git_host: Option<String>,
    /// Send a failed build or flow back to the agent, like a gate.
    #[serde(default)]
    on_fail: Option<WithOnFail>,
}

/// `with.on_fail`: the gate's `on_fail` shape.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct WithOnFail {
    send_back_to: StepKey,
    max: u32,
}

impl With {
    /// Parses `with:`; `Err` is a configuration error.
    pub(crate) fn parse(with: Option<&Value>) -> std::result::Result<Self, String> {
        serde_json::from_value(with.cloned().unwrap_or(Value::Null))
            .map_err(|e| format!("`{STEP_TYPE}` with: {e}"))
    }
}

/// A verify, ready to run off the engine task (no store access).
#[derive(Debug, Clone)]
pub(crate) struct Job {
    host: HostId,
    /// The task worktree the agent sessions used (`executor::worktree_of`).
    worktree: Worktree,
    request: DeviceRequest,
}

impl Job {
    /// Places the verify on the first host whose capabilities report the
    /// platform's tool and Maestro ready. `Ok(Err(why))` when
    /// no host has them: the step fails with `why`.
    pub(crate) async fn place(
        tx: &mut Tx,
        worktree: Worktree,
        with: With,
    ) -> Result<std::result::Result<Self, String>> {
        let (tool, label) = match with.platform {
            DevicePlatform::Ios => ("ios_simulator", "the iOS Simulator"),
            DevicePlatform::Android => ("android_emulator", "the Android emulator"),
        };
        let ready = |caps: &Value, tool: &str| caps["devices"][tool]["ready"] == true;
        Ok(tx
            .hosts()
            .await?
            .into_iter()
            .find(|h| ready(&h.capabilities, tool) && ready(&h.capabilities, "maestro"))
            .map(|host| Self::new(host.id, worktree, with))
            .ok_or_else(|| format!("no host with {label} and Maestro registered")))
    }

    fn new(host: HostId, worktree: Worktree, with: With) -> Self {
        let request = DeviceRequest {
            session_id: SessionId::generate(),
            cwd: PathBuf::new(), // the task dir, resolved on the host side
            platform: with.platform,
            device: with.device,
            build: with.build,
            app: with.app,
            flow: with.flow,
            env: Default::default(),
            timeout_s: with.timeout_s.unwrap_or(DEFAULT_TIMEOUT_S),
        };
        Self {
            host,
            worktree,
            request,
        }
    }

    /// Runs the verify in the task's worktree on the host.
    pub(crate) async fn run(self, hosts: &dyn HostLink) -> Result<DeviceResult> {
        let cwd = hosts.prepare_worktree(self.host, self.worktree).await?;
        let request = DeviceRequest {
            cwd,
            ..self.request
        };
        hosts.verify_on_device(self.host, request).await
    }
}

/// A verify's result with its screenshots stored as blobs.
#[derive(Debug)]
pub(crate) struct Verified {
    device: String,
    passed: bool,
    output: String,
    screenshots: Vec<(String, BlobHash)>,
}

/// Stores the screenshots. Call before opening the transaction (no blob I/O
/// inside one).
pub(crate) async fn store(blobs: &BlobStore, result: DeviceResult) -> Result<Verified> {
    let mut screenshots = Vec::new();
    for shot in result.screenshots {
        screenshots.push((shot.name, blobs.put(shot.png).await?));
    }
    Ok(Verified {
        device: result.device,
        passed: result.passed,
        output: result.output,
        screenshots,
    })
}

/// Journals the verify and, when there are screenshots, enqueues the PR
/// attachment, in the caller's transaction. Returns the step's verdict (a
/// send-back already queued its new attempts) and whether the outbox got a
/// new row.
pub(crate) async fn record(
    tx: &mut Tx,
    (workflow, latest): (&Workflow, &HashMap<StepKey, Step>),
    run: &Run,
    step: &Step,
    with: &With,
    verified: Result<Verified>,
) -> Result<(Verdict, bool)> {
    let verified = match verified {
        Ok(verified) => verified,
        Err(e) => {
            let why = format!("device verify: {e}");
            append(tx, step, DEVICE_RESULT, json!({ "error": why })).await?;
            return Ok((Verdict::Fail(why), false));
        }
    };
    let files: Vec<Value> = verified
        .screenshots
        .iter()
        .map(|(name, blob)| json!({ "name": name, "blob": blob, "content_type": "image/png" }))
        .collect();
    let payload = json!({
        "device": verified.device,
        "passed": verified.passed,
        "output": verified.output,
        "screenshots": files,
    });
    append(tx, step, DEVICE_RESULT, payload).await?;
    let mut enqueued = false;
    // A repo-less task has no PR to attach to.
    // ponytail: a multi-repo task attaches to its first repo's PR; name the
    // app's repo in `with:` if device apps live elsewhere.
    let facts = RunFacts::load(tx, run.id).await?;
    if let Some(repo) = facts.repos.first().filter(|_| !files.is_empty()) {
        let git_host = with.git_host.as_deref().unwrap_or("github");
        let key = format!("attach_to_pr:{}", step.attempt);
        enqueued = tx
            .enqueue(&OutboxItem {
                id: OutboxId::generate(),
                run_id: Some(run.id),
                idem_key: IdemKey::new(run.id, &step.step_key, &key),
                target: format!("{git_host}.attach_to_pr"),
                payload: json!({
                    "repo": repo_slug(&repo.url),
                    "task_id": run.task_id,
                    "run_id": run.id,
                    "branch": facts.branch,
                    "title": format!(
                        "Device verify {} on {}",
                        if verified.passed { "passed" } else { "failed" },
                        verified.device
                    ),
                    "passed": verified.passed,
                    "device": verified.device,
                    "files": files,
                }),
                status: OutboxStatus::Pending,
                result: None,
            })
            .await?;
    }
    if verified.passed {
        return Ok((Verdict::Pass, enqueued));
    }
    let failure = format!("device verify failed on {}", verified.device);
    let Some(on_fail) = &with.on_fail else {
        return Ok((Verdict::Fail(failure), enqueued));
    };
    let on_fail = OnFail {
        send_back_to: on_fail.send_back_to.clone(),
        max: on_fail.max,
    };
    let feedback = |n, max| {
        format!(
            "Device verify `{}` failed on {} (send-back {n} of {max}). Fix the app so \
             the build and the Maestro flow pass, then claim done again.\n\n```\n{}\n```\n",
            step.step_key,
            verified.device,
            gates::tail(&verified.output)
        )
    };
    let verdict =
        gates::send_back(tx, workflow, latest, step, &on_fail, &failure, feedback).await?;
    Ok((verdict, enqueued))
}

#[cfg(test)]
mod tests;
