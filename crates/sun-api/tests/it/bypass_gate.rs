//! BLO-139 (TRD BLO-117 item 15, case 3): every way to run a step with
//! `permission_mode: bypass_permissions` needs what saving one needs —
//! `manage_runtimes` on the workspace and the operator's
//! `runtimes.allow_bypass`. Saving a workflow and setting an agent's mode are
//! gated (`agents::an_agents_permission_mode_inherits_and_bypass_needs_manage_runtimes`);
//! an eval's candidate workflow runs too, for real with a named runtime.

use axum::http::StatusCode;
use serde_json::{Value, json};
use sun_core::model::Role;

use crate::api::new_task;
use crate::common::kernel;
use crate::route_matrix::until;
use crate::workspaces::{call, call_as, member_token};

const MAIN: &str = "00000000000000000000000001";

const APPROVAL: &str = "
name: gatekept
steps:
  - id: approve
    type: approval
    who: [role:admin]
";

/// An eval of `task` whose candidate is one agent step in `mode`, run for real
/// on `claude-code`.
fn eval(task: &str, mode: &str) -> Option<Value> {
    let candidate = format!(
        "name: gatekept\nsteps:\n  - {{id: approve, type: agent, prompt: p, permission_mode: {mode}}}\n"
    );
    Some(json!({
        "name": format!("candidate {mode}"),
        "pick": { "by": "tasks", "task_ids": [task] },
        "candidate": { "workflow": candidate },
        "runtime": "claude-code",
        "limit": 1,
    }))
}

#[tokio::test]
async fn an_eval_candidate_with_a_bypass_step_needs_the_bypass_gate() {
    let k = kernel().await;
    let bypass_ok = sun_api::router(k.api.clone().with_allow_bypass(true));
    // A past task with a finished run to replay.
    let task = new_task(&k.app, APPROVAL).await;
    let id = task["id"].as_str().unwrap().to_owned();
    until(&k.app, &id, "needs_you").await;
    let reason = json!({ "reason": "no" });
    let (status, _) = call(
        &k.app,
        "POST",
        &format!("/api/v1/tasks/{id}/reject"),
        Some(reason),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let bob = member_token(&k.store, "bob", Role::Member).await;
    // Control: Bob may run evals, and a non-bypass candidate is accepted.
    let (status, body) =
        call_as(&bypass_ok, &bob, "POST", "/api/v1/evals", eval(&id, "plan")).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    // Bob may not save this workflow (403), so may not run it either.
    let bypass = eval(&id, "bypass_permissions").unwrap();
    let save = json!({ "source_yaml": bypass["candidate"]["workflow"], "workspace_id": MAIN });
    let (status, body) = call_as(&bypass_ok, &bob, "POST", "/api/v1/workflows", Some(save)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "saving it: {body}");
    let (status, body) = call_as(
        &bypass_ok,
        &bob,
        "POST",
        "/api/v1/evals",
        eval(&id, "bypass_permissions"),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "a member without manage_runtimes ran a bypass candidate: {body}"
    );
    // Without the operator's allow_bypass, nobody runs one, the owner included.
    let (status, body) = call(
        &k.app,
        "POST",
        "/api/v1/evals",
        eval(&id, "bypass_permissions"),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "a bypass candidate ran with allow_bypass off: {body}"
    );
}
