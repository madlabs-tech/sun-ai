//! Corrections: `GET /corrections`, `GET /tasks/{id}/corrections`,
//! `POST /tasks/{id}/decision` (a human answers an escalated decide step),
//! `GET /decisions/export` and `POST /decisions/replay`,
//! and reverts read from verified git host push webhooks.

use axum::Json;
use axum::extract::{Extension, Path, Query, State};
use axum::http::header;
use axum::response::IntoResponse;
use serde::Deserialize;
use serde_json::Value;
use sun_core::model::{Correction, TaskKey};
use sun_core::{StepStatus, TaskId};
use sun_engine::DecisionReplay;

use crate::Api;
use crate::auth::Caller;
use crate::error::{ApiError, ApiResult};
use crate::routes::{Body, id, latest_run, summary};
use crate::wire::TaskView;

/// Default and largest page of `GET /corrections`.
const PAGE: u32 = 100;
const MAX_PAGE: u32 = 500;
/// Largest `answer` object, as JSON.
const MAX_ANSWER: usize = 1000;
/// Reverts read from one push (GitHub sends at most 20 commits inline).
const MAX_REVERTS: usize = 50;

#[derive(Deserialize)]
pub(crate) struct Page {
    limit: Option<u32>,
}

/// `GET /api/v1/corrections?limit=`: every task's latest, newest first.
pub(crate) async fn list(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Query(page): Query<Page>,
) -> ApiResult<Json<Vec<Correction>>> {
    let limit = page.limit.unwrap_or(PAGE).clamp(1, MAX_PAGE);
    let workflows = crate::wire::Workflows::load(&api.engine).await?;
    let mut corrections = api.engine.corrections(limit).await?;
    if caller.workspaces.is_some() {
        corrections.retain(|c| workflows.on_workspace(c.workflow_id, |b| caller.sees_text(b)));
    }
    Ok(Json(corrections))
}

/// `GET /api/v1/tasks/{id}/corrections`: the task's, oldest first.
pub(crate) async fn of_task(
    State(api): State<Api>,
    Path(task): Path<String>,
) -> ApiResult<Json<Vec<Correction>>> {
    let task_id: TaskId = id(&task, "task")?;
    api.engine.task(task_id).await?;
    Ok(Json(api.engine.corrections_of(task_id).await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Answer {
    /// `{"choice": "billing"}`, `{"score": 2}` or `{"noul": 0.1}`.
    answer: Value,
}

/// `POST /api/v1/tasks/{id}/decision {answer}`: answers the task's escalated
/// decide step; a different answer than the model's is a correction.
pub(crate) async fn decide(
    State(api): State<Api>,
    Extension(by): Extension<Caller>,
    Path(task): Path<String>,
    Body(input): Body<Answer>,
) -> ApiResult<Json<TaskView>> {
    let answer = input.answer;
    let one_key = answer.as_object().is_some_and(|o| o.len() == 1);
    if !one_key || answer.to_string().len() > MAX_ANSWER {
        return Err(ApiError::invalid(
            "Send one answer: {\"choice\": label}, {\"score\": level} or {\"noul\": 0..1}.",
        ));
    }
    let task_id = id(&task, "task")?;
    let run = latest_run(&api, task_id).await?;
    let step = api
        .engine
        .steps(run.id)
        .await?
        .into_iter()
        .find(|s| s.status == StepStatus::WaitingApproval)
        .ok_or_else(|| ApiError::conflict("This task is not waiting for a decision."))?;
    let by = by.principal_in(api.engine.workspace_of_task(task_id).await?);
    api.engine
        .correct_decision(run.id, step.step_key, by, answer)
        .await?;
    summary(&api, task_id).await
}

/// `GET /api/v1/decisions/export`: human-answered decisions as JSONL
/// fine-tuning rows for the typed-decisions model.
pub(crate) async fn export(State(api): State<Api>) -> ApiResult<impl IntoResponse> {
    let jsonl = api.engine.export_decisions().await?;
    let disposition = "attachment; filename=\"decisions.jsonl\"";
    Ok((
        [
            (header::CONTENT_TYPE, "application/jsonl"),
            (header::CONTENT_DISPOSITION, disposition),
        ],
        jsonl,
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Candidate {
    /// The candidate backend's base URL (a new model version).
    url: String,
}

/// `POST /api/v1/decisions/replay {url}`: replays past decisions on the
/// candidate and switches it in only when it escalates less without more
/// overrides.
pub(crate) async fn replay(
    State(api): State<Api>,
    Body(candidate): Body<Candidate>,
) -> ApiResult<Json<DecisionReplay>> {
    Ok(Json(api.engine.replay_decisions(&candidate.url).await?))
}

/// Records the reverts in a verified webhook delivery (`{event, payload}`)
/// that pushed to a task branch `sun/<KEY>-<slug>`. Other deliveries, and a
/// key no task has: no-op.
pub(crate) async fn record_reverts(api: &Api, delivery: &Value) -> ApiResult<()> {
    let Some((key, reverts)) = reverts(delivery) else {
        return Ok(());
    };
    let task_id = match api.engine.task_id_by_key(&key).await {
        Ok(id) => id,
        Err(sun_core::Error::NotFound(_)) => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    for (sha, message) in reverts {
        api.engine.record_revert(task_id, &sha, &message).await?;
    }
    Ok(())
}

/// The task key and `(sha, message)` of each revert commit in a GitHub,
/// Gitea or GitLab push to `refs/heads/sun/<KEY>-<slug>`.
fn reverts(delivery: &Value) -> Option<(TaskKey, Vec<(String, String)>)> {
    if !matches!(delivery["event"].as_str(), Some("push" | "Push Hook")) {
        return None;
    }
    let payload = &delivery["payload"];
    let branch = payload["ref"].as_str()?.strip_prefix("refs/heads/")?;
    let key = TaskKey::from_branch(branch)?;
    let reverts = payload["commits"]
        .as_array()?
        .iter()
        .filter_map(|c| {
            let sha = c["id"].as_str()?;
            let message = c["message"].as_str()?;
            let valid = (7..=64).contains(&sha.len()) && sha.bytes().all(|b| b.is_ascii_hexdigit());
            let revert =
                message.starts_with("Revert \"") || message.contains("This reverts commit ");
            (valid && revert).then(|| (sha.to_owned(), message.to_owned()))
        })
        .take(MAX_REVERTS)
        .collect();
    Some((key, reverts))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn correction_reverts_come_from_pushes_to_task_branches() {
        let branch = "refs/heads/sun/SUN-12-fix-login".to_owned();
        let push = |event: &str, branch: String| {
            json!({ "event": event, "payload": { "ref": branch, "commits": [
                { "id": "a1b2c3d4", "message": "fix: login timeout" },
                { "id": "e5f6a7b8", "message": "Revert \"fix: login timeout\"\n\nThis reverts commit a1b2c3d4." },
                { "id": "not-hex!", "message": "Revert \"x\"" },
            ]}})
        };
        let (found, commits) = reverts(&push("push", branch.clone())).unwrap();
        assert_eq!(found, "SUN-12".parse().unwrap());
        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0].0, "e5f6a7b8");
        assert!(reverts(&push("Push Hook", branch.clone())).is_some());
        assert!(reverts(&push("push", "refs/heads/main".into())).is_none());
        assert!(reverts(&push("pull_request", branch)).is_none());
        assert!(reverts(&push("push", "refs/heads/sun/skill-tdd".into())).is_none());
        assert!(
            reverts(&push(
                "push",
                "refs/heads/sun/01J9X8K2M3N4P5Q6R7S8T9V0WX".into()
            ))
            .is_none()
        );
    }
}
