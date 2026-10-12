//! Design-canvas pins, part of the engine's
//! **Facade** for the API. Each change commits the row and, when the task has
//! a run, a `pin` event on that run's timeline in one transaction.
//!
//! Evidence: a pin carries a PNG crop of its element, taken from the task
//! branch's preview when it is pinned (`before_crop`) and again when it is
//! done (`after_crop`); the host screenshots the page and the engine crops
//! it ([`crate::screenshot`]). Capturing runs in the background and never
//! fails a pin: a host without a browser just leaves the crops empty.
//!
//! "Change it myself": [`Engine::commit_pin_edit`] applies the
//! pin's edits to its source line in the task worktree, commits that file
//! under the user's name, journals a `commit` event and re-runs the run's
//! gates. An edit that doesn't map exactly to the line is refused with a
//! hint to hand it to the agent.

use std::collections::BTreeMap;
use std::path::{Component, Path};
use std::sync::Arc;

use chrono::Utc;
use serde::Serialize;
use serde_json::{Value, json};
use sun_core::host_protocol::{ExecRequest, PreviewSide, ScreenshotRequest, Worktree};
use sun_core::model::{OutboxItem, OutboxStatus, Pin, PinEdit, PinStatus, PinTarget};
use sun_core::{
    BlobHash, Error, HostId, HostLink, IdemKey, OutboxId, PinId, Result, RunId, StepKey, TaskId,
};
use sun_store::{
    EventLog, NewEvent, OutboxRepo, PinRepo, RunRepo, Store, TaskRepo, Tx, WorkflowRepo,
};
use sun_workflow::StepKind;

use crate::executor::{append, worktree_of};
use crate::gates::{self, quote};
use crate::outbox::{RunFacts, repo_slug};
use crate::{Engine, screenshot};

/// The canvas breakpoints `(width, viewport height)`: phone, tablet, desktop.
pub(crate) const BREAKPOINTS: [(u32, u32); 3] = [(360, 740), (768, 1024), (1280, 800)];

/// A partial update of a pin; `None` keeps the stored value.
#[derive(Debug, Clone, Default)]
pub struct PinChange {
    /// New status.
    pub status: Option<PinStatus>,
    /// New request text.
    pub comment: Option<String>,
    /// New suggested edits (replaces the list).
    pub edits: Option<Vec<PinEdit>>,
}

/// Who commits a canvas edit.
#[derive(Debug, Clone)]
pub struct Author {
    /// Git author and committer name.
    pub name: String,
    /// Git author and committer email.
    pub email: String,
}

/// A committed canvas edit.
#[derive(Debug, Clone, Serialize)]
pub struct PinCommit {
    /// The new commit on the task branch.
    pub sha: String,
    /// The pin, now `done`.
    pub pin: Pin,
}

/// One screenshot attached as review evidence.
#[derive(Debug, Clone, Serialize)]
pub struct Screen {
    /// File name, e.g. `after-360-settings.png`.
    pub name: String,
    /// The PNG.
    pub blob: BlobHash,
    /// `before` or `after`.
    pub side: PreviewSide,
    /// Page path.
    pub path: String,
    /// Viewport width.
    pub width: u32,
}

impl Engine {
    /// The task's pins, oldest first.
    pub async fn pins(&self, task_id: TaskId) -> Result<Vec<Pin>> {
        Ok(self.store.read().await?.pins_of(task_id).await?)
    }

    /// Pins an element of the task's app; its `before` crop follows in the
    /// background.
    pub async fn create_pin(
        &self,
        task_id: TaskId,
        target: PinTarget,
        comment: String,
    ) -> Result<Pin> {
        let now = Utc::now();
        let pin = Pin {
            id: PinId::generate(),
            task_id,
            status: PinStatus::Open,
            comment,
            target,
            edits: Vec::new(),
            before_crop: None,
            after_crop: None,
            created_at: now,
            updated_at: now,
        };
        let mut tx = self.store.begin().await?;
        tx.get_task(task_id).await?;
        tx.save_pin(&pin).await?;
        journal(&mut tx, &pin, "pinned").await?;
        tx.commit().await?;
        self.capture(task_id, pin.id, PreviewSide::Before);
        Ok(pin)
    }

    /// Applies `change` to a pin of `task_id`. A pin that becomes `done`
    /// gets its `after` crop in the background.
    pub async fn update_pin(&self, task_id: TaskId, id: PinId, change: PinChange) -> Result<Pin> {
        let mut tx = self.store.begin().await?;
        let mut pin = owned(&mut tx, task_id, id).await?;
        let status_changed = change.status.is_some_and(|s| s != pin.status);
        pin.status = change.status.unwrap_or(pin.status);
        pin.comment = change.comment.unwrap_or(pin.comment);
        pin.edits = change.edits.unwrap_or(pin.edits);
        pin.updated_at = Utc::now();
        tx.save_pin(&pin).await?;
        if status_changed {
            journal(&mut tx, &pin, pin.status.as_str()).await?;
        }
        tx.commit().await?;
        if status_changed && pin.status == PinStatus::Done {
            self.capture(task_id, id, PreviewSide::After);
        }
        Ok(pin)
    }

    /// Removes a pin of `task_id`.
    pub async fn delete_pin(&self, task_id: TaskId, id: PinId) -> Result<()> {
        let mut tx = self.store.begin().await?;
        let pin = owned(&mut tx, task_id, id).await?;
        tx.delete_pin(id).await?;
        journal(&mut tx, &pin, "removed").await?;
        Ok(tx.commit().await?)
    }

    /// A pin's PNG crop: `before` (when pinned) or `after` (when done).
    pub async fn pin_crop(&self, task_id: TaskId, id: PinId, side: PreviewSide) -> Result<Vec<u8>> {
        let pin = owned(&mut self.store.read().await?, task_id, id).await?;
        let hash = match side {
            PreviewSide::Before => pin.before_crop,
            PreviewSide::After => pin.after_crop,
        };
        let hash = hash.ok_or_else(|| Error::NotFound(format!("{side} crop of pin {id}")))?;
        Ok(self.store.blobs().get(&hash).await?)
    }

    /// Takes a pin's crop in the background.
    fn capture(&self, task_id: TaskId, id: PinId, side: PreviewSide) {
        spawn_capture(&self.store, &self.hosts, self.host, task_id, vec![id], side);
    }

    /// "Change it myself": applies the pin's edits to its source
    /// line in the task worktree and commits that file to the task branch
    /// under `author`. The pin becomes `done`, a `commit` event joins the
    /// timeline, and the run's gates run again in the background.
    /// `InvalidInput` (hand it to the agent) when the pin has no source
    /// line, the line is gone, or a value isn't on it exactly once.
    pub async fn commit_pin_edit(
        &self,
        task_id: TaskId,
        id: PinId,
        author: Author,
    ) -> Result<PinCommit> {
        let (pin, worktree) = {
            let mut tx = self.store.read().await?;
            let pin = owned(&mut tx, task_id, id).await?;
            (pin, worktree_of(&mut tx, task_id).await?)
        };
        let hand_off = |why: String| {
            Error::InvalidInput(format!("{why}. Hand the edit to the agent instead."))
        };
        let Some(source) = pin.target.source.clone() else {
            return Err(hand_off("This element maps to no source line".into()));
        };
        if pin.edits.is_empty() {
            return Err(Error::InvalidInput("Save an edit on the pin first.".into()));
        }
        let (file, line) = (source.file.as_str(), source.line);
        let relative = Path::new(file)
            .components()
            .all(|c| matches!(c, Component::Normal(_)));
        if !relative || line == 0 || file.contains(['\0', '\n']) {
            return Err(hand_off(format!(
                "`{file}:{line}` isn't a file in the repo"
            )));
        }
        if !matches!(worktree, Worktree::Repo { .. }) {
            return Err(hand_off(
                "Only single-repo tasks take canvas commits".into(),
            ));
        }
        let dir = self.hosts.prepare_worktree(self.host, worktree).await?;
        // `./` keeps a file named `-x` from reading as an option (BSD sed has no `--`).
        let path = quote(&format!("./{file}"));
        let read = format!("sed -n '{line}p' {path}");
        let (ok, text) = self.sh(&dir, &read, BTreeMap::new()).await?;
        let old = text.strip_suffix('\n').unwrap_or(&text);
        if !ok || old.is_empty() {
            return Err(hand_off(format!(
                "`{file}:{line}` isn't in the task's worktree"
            )));
        }
        let mut new = old.to_owned();
        for edit in &pin.edits {
            if edit.from.is_empty() || new.matches(edit.from.as_str()).count() != 1 {
                return Err(hand_off(format!(
                    "`{}` isn't on {file}:{line} exactly once",
                    edit.from
                )));
            }
            new = new.replacen(edit.from.as_str(), &edit.to, 1);
        }
        let changed: Vec<&str> = pin.edits.iter().map(|e| e.property.as_str()).collect();
        let message = format!(
            "canvas: change {} of <{}> ({file}:{line})",
            changed.join(", "),
            pin.target.tag
        );
        // User text goes through the environment, never into the script.
        let script = format!(
            "f={path}\nawk -v n={line} 'NR==n{{print ENVIRON[\"SUN_LINE\"]; next}} {{print}}' \"$f\" \
             > \"$f.sun-edit\" && cat \"$f.sun-edit\" > \"$f\" && rm -f \"$f.sun-edit\" && \
             git add -- \"$f\" && git commit -q -m \"$SUN_MESSAGE\" -- \"$f\" && git rev-parse HEAD"
        );
        let env = [
            ("SUN_LINE", new),
            ("SUN_MESSAGE", message),
            ("GIT_AUTHOR_NAME", author.name.clone()),
            ("GIT_AUTHOR_EMAIL", author.email.clone()),
            ("GIT_COMMITTER_NAME", author.name.clone()),
            ("GIT_COMMITTER_EMAIL", author.email),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v))
        .collect();
        let (ok, output) = self.sh(&dir, &script, env).await?;
        let sha = output.lines().last().unwrap_or_default().trim().to_owned();
        if !ok || sha.len() < 7 || !sha.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(Error::Unavailable(format!(
                "committing the edit failed: {}",
                gates::tail(&output)
            )));
        }

        let mut tx = self.store.begin().await?;
        let mut pin = owned(&mut tx, task_id, id).await?;
        pin.status = PinStatus::Done;
        pin.updated_at = Utc::now();
        tx.save_pin(&pin).await?;
        journal(&mut tx, &pin, "committed").await?;
        let run = tx.runs_of_task(task_id).await?.pop();
        if let Some(run) = &run {
            tx.append_event(NewEvent {
                run_id: run.id,
                step_key: None,
                kind: "commit".into(),
                payload: json!({
                    "sha": sha,
                    "file": file,
                    "line": line,
                    "by": author.name,
                    "pin_id": pin.id,
                    "summary": format!(
                        "{} committed a canvas edit · {file}:{line} · {}",
                        author.name,
                        &sha[..7]
                    ),
                }),
                blob: None,
                at: pin.updated_at,
            })
            .await?;
        }
        tx.commit().await?;
        self.capture(task_id, id, PreviewSide::After);
        if let Some(run) = run {
            let engine = self.clone();
            tokio::spawn(async move {
                if let Err(e) = engine.rerun_gates(run.id).await {
                    eprintln!("sun: re-running the gates of run {}: {e}", run.id);
                }
            });
        }
        Ok(PinCommit { sha, pin })
    }

    /// Runs every gate step of the run's workflow again after a canvas
    /// commit and journals each result as a `gate_result` on that
    /// step's latest attempt, marked `rerun: canvas_commit`.
    // ponytail: a re-check only. A failing result doesn't reopen a finished
    // run or send work back; route it through the executor if it should.
    pub(crate) async fn rerun_gates(&self, run_id: RunId) -> Result<()> {
        let (run, workflow, steps, worktree) = {
            let mut tx = self.store.read().await?;
            let run = tx.get_run(run_id).await?;
            let version = tx
                .get_workflow_version(run.workflow_id, run.workflow_version)
                .await?;
            let workflow = sun_workflow::parse(&version.source_yaml).map_err(|_| {
                Error::InvalidInput(format!("workflow of run {run_id} no longer parses"))
            })?;
            let steps = tx.steps_of_run(run_id).await?;
            let worktree = worktree_of(&mut tx, run.task_id).await?;
            (run, workflow, steps, worktree)
        };
        for def in &workflow.steps {
            let StepKind::Gate { gates: specs, .. } = &def.kind else {
                continue;
            };
            let latest = steps
                .iter()
                .filter(|s| s.step_key == def.id)
                .max_by_key(|s| s.attempt);
            let Some(step) = latest else { continue };
            let job = gates::Job::new(self.host, worktree.clone(), run.id, specs)
                .map_err(Error::InvalidInput)?;
            let results = job.run(self.hosts.as_ref(), self.mods.as_ref()).await;
            let mut tx = self.store.begin().await?;
            for result in &results {
                let payload = json!({
                    "step_id": step.id,
                    "attempt": step.attempt,
                    "result": result,
                    "rerun": "canvas_commit",
                });
                append(&mut tx, step, "gate_result", payload).await?;
            }
            tx.commit().await?;
        }
        Ok(())
    }

    /// Review evidence: screenshots of the agreed screens (`paths`
    /// × `widths`, before and after) are stored, journalled as a
    /// `canvas_screens` event on the task's latest run, and attached to the
    /// task's PR through `git_host` (default `github`) via the outbox.
    pub async fn attach_screens(
        &self,
        task_id: TaskId,
        paths: Vec<String>,
        widths: Vec<u32>,
        git_host: Option<String>,
    ) -> Result<Vec<Screen>> {
        let worktree = self.worktree(task_id).await?;
        let mut screens = Vec::new();
        for path in &paths {
            for &width in &widths {
                let height = BREAKPOINTS
                    .iter()
                    .find(|(w, _)| *w == width)
                    .map_or(800, |(_, h)| *h);
                for side in PreviewSide::ALL {
                    let request = ScreenshotRequest {
                        worktree: worktree.clone(),
                        side: *side,
                        path: path.clone(),
                        width,
                        height,
                    };
                    let png = self.hosts.preview_screenshot(self.host, request).await?;
                    screens.push(Screen {
                        name: format!("{side}-{width}-{}.png", slug(path)),
                        blob: self.store.blobs().put(png).await?,
                        side: *side,
                        path: path.clone(),
                        width,
                    });
                }
            }
        }
        let mut tx = self.store.begin().await?;
        let run = tx.runs_of_task(task_id).await?.pop().ok_or_else(|| {
            Error::InvalidInput("The task has no run to attach screenshots to.".into())
        })?;
        let seen = tx
            .events_after(Some(run.id), 0, u32::MAX)
            .await?
            .iter()
            .filter(|e| e.kind == "canvas_screens")
            .count();
        let files: Vec<Value> = screens
            .iter()
            .map(|s| json!({ "name": s.name, "blob": s.blob, "content_type": "image/png" }))
            .collect();
        tx.append_event(NewEvent {
            run_id: run.id,
            step_key: None,
            kind: "canvas_screens".into(),
            payload: json!({
                "screenshots": files,
                "screens": screens,
                "summary": format!("Canvas screenshots attached · {} image(s)", screens.len()),
            }),
            blob: None,
            at: Utc::now(),
        })
        .await?;
        let facts = RunFacts::load(&mut tx, run.id).await?;
        if let Some(repo) = facts.repos.first() {
            let widths: Vec<String> = widths.iter().map(u32::to_string).collect();
            tx.enqueue(&OutboxItem {
                id: OutboxId::generate(),
                run_id: Some(run.id),
                // Each attach is its own comment.
                idem_key: IdemKey::new(
                    run.id,
                    &StepKey::new("canvas")?,
                    &format!("attach_to_pr:{}", seen + 1),
                ),
                target: format!("{}.attach_to_pr", git_host.as_deref().unwrap_or("github")),
                payload: json!({
                    "repo": repo_slug(&repo.url),
                    "task_id": run.task_id,
                    "run_id": run.id,
                    "branch": facts.branch,
                    "title": "Design canvas: before and after",
                    "passed": true,
                    "device": format!("{} px", widths.join(" / ")),
                    "files": files,
                }),
                status: OutboxStatus::Pending,
                result: None,
            })
            .await?;
        }
        tx.commit().await?;
        self.outbox_wake.notify_one();
        Ok(screens)
    }

    /// Runs `command` in `dir` on the engine's host: `(exit 0, output)`.
    async fn sh(
        &self,
        dir: &Path,
        command: &str,
        env: BTreeMap<String, String>,
    ) -> Result<(bool, String)> {
        let request = ExecRequest {
            cwd: dir.to_path_buf(),
            command: command.to_owned(),
            env,
            timeout_s: 60,
        };
        let out = self.hosts.exec(self.host, request).await?;
        Ok((out.code == Some(0), out.output))
    }
}

/// A file-name-safe form of a page path (`home` for `/`).
fn slug(path: &str) -> String {
    let slug: String = path
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    match slug.trim_matches('-') {
        "" => "home".into(),
        slug => slug.into(),
    }
}

/// An agent step of the task succeeded: every pin that was with the agent
/// (`in_progress`) is `done`, also while no canvas is open. Returns those
/// pins: their `after` crops are due once the transaction commits.
// ponytail: all in_progress pins of the task, not only the ones this step
// was sent; record the receiving step on the pin if steps run in parallel.
pub(crate) async fn agent_done(tx: &mut Tx, task_id: TaskId) -> Result<Vec<PinId>> {
    let mut done = Vec::new();
    for mut pin in tx.pins_of(task_id).await? {
        if pin.status == PinStatus::InProgress {
            pin.status = PinStatus::Done;
            pin.updated_at = Utc::now();
            tx.save_pin(&pin).await?;
            journal(tx, &pin, pin.status.as_str()).await?;
            done.push(pin.id);
        }
    }
    Ok(done)
}

/// Takes the `side` crops of `pins` in the background: screenshots the task
/// branch's preview at each pin's width and page, crops the pin's box and
/// stores it on the pin. Failures are logged, never raised.
// ponytail: the after crop reuses the box from pick time; re-locate the
// selector in the page if elements move a lot after a change.
pub(crate) fn spawn_capture(
    store: &Store,
    hosts: &Arc<dyn HostLink>,
    host: HostId,
    task_id: TaskId,
    pins: Vec<PinId>,
    side: PreviewSide,
) {
    if pins.is_empty() {
        return;
    }
    let (store, hosts) = (store.clone(), hosts.clone());
    tokio::spawn(async move {
        for id in pins {
            if let Err(e) = capture(&store, hosts.as_ref(), host, task_id, id, side).await {
                eprintln!("sun: the {side} crop of pin {id}: {e}");
            }
        }
    });
}

async fn capture(
    store: &Store,
    hosts: &dyn HostLink,
    host: HostId,
    task_id: TaskId,
    id: PinId,
    side: PreviewSide,
) -> Result<()> {
    let (target, worktree) = {
        let mut tx = store.read().await?;
        let pin = owned(&mut tx, task_id, id).await?;
        (pin.target, worktree_of(&mut tx, task_id).await?)
    };
    let request = ScreenshotRequest {
        worktree,
        side: PreviewSide::After,
        path: match target.path.as_str() {
            "" => "/".into(),
            path => path.into(),
        },
        width: target.width,
        height: screenshot::height_for(&target.rect),
    };
    let png = hosts.preview_screenshot(host, request).await?;
    let crop = screenshot::crop(&png, &target.rect)?;
    let hash = store.blobs().put(crop).await?;
    let mut tx = store.begin().await?;
    let mut pin = owned(&mut tx, task_id, id).await?;
    match side {
        PreviewSide::Before => pin.before_crop = Some(hash),
        PreviewSide::After => pin.after_crop = Some(hash),
    }
    tx.save_pin(&pin).await?;
    Ok(tx.commit().await?)
}

async fn owned(tx: &mut Tx, task_id: TaskId, id: PinId) -> Result<Pin> {
    let pin = tx.get_pin(id).await?;
    if pin.task_id != task_id {
        return Err(Error::NotFound(format!("pin {id}")));
    }
    Ok(pin)
}

/// `pin {pin_id, action, status, selector}` on the task's latest run, if any.
async fn journal(tx: &mut Tx, pin: &Pin, action: &str) -> Result<()> {
    let Some(run) = tx.runs_of_task(pin.task_id).await?.pop() else {
        return Ok(());
    };
    tx.append_event(NewEvent {
        run_id: run.id,
        step_key: None,
        kind: "pin".into(),
        payload: json!({
            "pin_id": pin.id,
            "action": action,
            "status": pin.status,
            "selector": pin.target.selector,
        }),
        blob: None,
        at: pin.updated_at,
    })
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests;
