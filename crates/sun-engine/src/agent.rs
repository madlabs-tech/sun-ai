//! What an agent asks of the engine through the `sun` MCP server:
//! `sun.get` (pointer resolution, scoped to the run) and
//! `sun.ask` (a blocking question to a human).
//!
//! A child module of `executor`. A question is a `question` note plus a
//! `question` event; its step moves `running → waiting_approval` until a
//! human answers ([`Engine::answer`]), then `→ running` with an `answer`
//! event, and the agent's `sun.ask` call returns the answer. The engine task
//! never waits: the caller awaits a oneshot, with no transaction open.
//!
//! [`Engine::answer`]: super::Engine::answer

use serde_json::json;
use sun_core::model::{Note, NoteKind};
use sun_core::redact::redact;
use sun_core::{ArtifactId, BlobHash, Error, NoteId, Result, RunId, StepKey, StepStatus};
use sun_store::{ArtifactRepo, EventLog, NoteRepo, PointerRepo, RunRepo, SecretRepo, Store, Tx};
use tokio::sync::oneshot;

use super::{Executor, append, move_step};
use crate::Actor;
use crate::state;

/// A question was asked: `{question_id, step_id, question, options}`.
pub(super) const QUESTION: &str = "question";
/// A human answered: `{question_id, answer}`.
const ANSWER: &str = "answer";
/// Step reason of a step waiting on an agent's question (not an approval).
pub(crate) const ASKED: &str = "the agent asked a question";
/// Longest text `sun.get` returns.
const MAX_GET_BYTES: usize = 256 * 1024;

/// Resolves a pack pointer for an agent of `run_id` (`sun.get`): `blob:<hash>`
/// (a pack, artifact or event blob of this run) or `artifact:<id>` (of this
/// run's task), as redacted text. Files under `.sun/` are in the agent's own
/// worktree; anything else is `NotFound`.
// ponytail: `memory:` and `doc:` pointers are NotFound until memory and
// knowledge docs get a read path here.
pub(crate) async fn resolve(store: &Store, run_id: RunId, pointer: &str) -> Result<String> {
    let not_found = || Error::NotFound(format!("pointer `{pointer}` in run {run_id}"));
    let mut read = store.read().await?;
    let run = read.get_run(run_id).await?;
    let hash = match pointer.split_once(':') {
        Some(("blob", hash)) => {
            let hash = BlobHash::new(hash).map_err(|_| not_found())?;
            if !read.run_has_blob(run_id, &hash).await? {
                return Err(not_found());
            }
            hash
        }
        Some(("artifact", id)) => {
            let id: ArtifactId = id.parse().map_err(|_| not_found())?;
            match read.get_artifact(id).await {
                Ok(artifact) if artifact.task_id == run.task_id => artifact.blob,
                _ => return Err(not_found()),
            }
        }
        _ if pointer.starts_with(".sun/") => {
            return Err(Error::NotFound(format!(
                "`{pointer}` is a file in your working directory: read it there"
            )));
        }
        _ => return Err(not_found()),
    };
    let secrets = read.known_secrets().await?;
    drop(read);
    let bytes = store.blobs().get(&hash).await?;
    let text = String::from_utf8_lossy(&bytes[..bytes.len().min(MAX_GET_BYTES)]);
    let secrets: Vec<&str> = secrets.iter().map(String::as_str).collect();
    Ok(redact(&text, &secrets))
}

/// The open question of a run: its step and allowed answers.
pub(super) struct Question {
    step_key: StepKey,
    options: Vec<String>,
}

impl Executor {
    /// `sun.ask`: journals the question and parks the step in
    /// `waiting_approval`. The receiver yields the answer.
    pub(super) async fn ask(
        &mut self,
        run_id: RunId,
        step_key: StepKey,
        question: String,
        options: Vec<String>,
    ) -> Result<oneshot::Receiver<String>> {
        let mut tx = self.store.begin().await?;
        let mut step = state::latest(tx.steps_of_run(run_id).await?)
            .remove(&step_key)
            .ok_or_else(|| Error::NotFound(format!("step `{step_key}` of run {run_id}")))?;
        if step.status != StepStatus::Running {
            return Err(Error::InvalidInput(format!(
                "step `{step_key}` is {}; only a running agent can ask",
                step.status
            )));
        }
        let id = NoteId::generate();
        tx.insert_note(&Note {
            id,
            run_id,
            step_key: step_key.clone(),
            kind: NoteKind::Question,
            body: question.clone(),
            superseded_by: None,
        })
        .await?;
        let why = Some(ASKED);
        move_step(&mut tx, &mut step, StepStatus::WaitingApproval, why).await?;
        let payload = json!({
            "question_id": id, "step_id": step.id, "question": question, "options": options,
        });
        append(&mut tx, &step, QUESTION, payload).await?;
        tx.commit().await?;
        let (answer, receiver) = oneshot::channel();
        self.questions.insert(id, (step.id, answer));
        Ok(receiver)
    }

    /// `by` answers question `id` of `run_id`. `options`, when the agent
    /// gave some, are the only valid answers. A capped `by` may not answer a
    /// bypass session (BLO-172). The step runs again.
    pub(super) async fn answer(
        &mut self,
        run_id: RunId,
        id: NoteId,
        answer: String,
        by: &Actor,
    ) -> Result<()> {
        let mut tx = self.store.begin().await?;
        let question = question_of(&mut tx, run_id, id).await?;
        if !question.options.is_empty() && !question.options.contains(&answer) {
            return Err(Error::InvalidInput(format!(
                "answer one of: {}",
                question.options.join(", ")
            )));
        }
        let mut step = state::latest(tx.steps_of_run(run_id).await?)
            .remove(&question.step_key)
            .ok_or_else(|| Error::NotFound(format!("question {id} of run {run_id}")))?;
        if step.status != StepStatus::WaitingApproval {
            return Err(Error::InvalidInput(format!(
                "question {id} is closed (step `{}` is {})",
                step.step_key, step.status
            )));
        }
        if let Some(session) = step.session_id
            && crate::bypass::live_bypass(&mut tx, run_id, session, Some(by)).await?
        {
            return Err(Error::Forbidden(crate::bypass::ANSWER_REFUSED.into()));
        }
        move_step(
            &mut tx,
            &mut step,
            StepStatus::Running,
            Some("question answered"),
        )
        .await?;
        append(
            &mut tx,
            &step,
            ANSWER,
            json!({ "question_id": id, "answer": answer }),
        )
        .await?;
        self.watch(&mut tx, &step).await?;
        tx.commit().await?;
        // ponytail: after an engine restart nobody waits here; the step runs
        // with no session until the watchdog's stall policy restarts it.
        if let Some((_, waiting)) = self.questions.remove(&id) {
            let _ = waiting.send(answer);
        }
        Ok(())
    }
}

/// The `question` event `id` of `run_id`, from the journal.
async fn question_of(tx: &mut Tx, run_id: RunId, id: NoteId) -> Result<Question> {
    let mut cursor = 0;
    loop {
        let page = tx.events_after(Some(run_id), cursor, 500).await?;
        let Some(last) = page.last() else {
            return Err(Error::NotFound(format!("question {id} of run {run_id}")));
        };
        cursor = last.seq;
        for event in page.iter().filter(|e| e.kind == QUESTION) {
            if event.payload["question_id"] == json!(id)
                && let Some(step_key) = event.step_key.clone()
            {
                let options =
                    serde_json::from_value(event.payload["options"].clone()).unwrap_or_default();
                return Ok(Question { step_key, options });
            }
        }
    }
}
