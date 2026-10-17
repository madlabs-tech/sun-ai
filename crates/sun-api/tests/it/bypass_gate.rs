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

/// BLO-164: the owner's bypass policy at `/settings/bypass`, and `can_bypass`
/// on `/me`: off for everyone while sun.yaml `allow_bypass` is off; then the
/// owner always, members per the policy. An eval's bypass candidate needs it
/// too, since nobody could cap it later.
#[tokio::test]
async fn the_owners_bypass_policy_decides_who_can_bypass() {
    let k = kernel().await;
    let bypass_ok = sun_api::router(k.api.clone().with_allow_bypass(true));
    let ann = member_token(&k.store, "ann", Role::Admin).await;
    let bob = member_token(&k.store, "bob", Role::Member).await;
    let (ann, bob) = (ann.as_str(), bob.as_str());

    let (status, policy) = call(&k.app, "GET", "/api/v1/settings/bypass", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        policy,
        json!({ "mode": "off", "member_ids": [], "allow_bypass": false })
    );
    assert!(
        !can(&k.app, None).await,
        "allow_bypass off: not even the owner"
    );
    assert!(can(&bypass_ok, None).await, "the owner, while off");
    assert!(!can(&bypass_ok, Some(ann)).await && !can(&bypass_ok, Some(bob)).await);

    let (_, me) = call_as(&bypass_ok, bob, "GET", "/api/v1/me", None).await;
    let bob_id = me["memberships"][0]["member_id"].clone();
    let (status, saved) = put(
        &bypass_ok,
        json!({ "mode": "members", "member_ids": [bob_id] }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert_eq!(saved["allow_bypass"], true);
    assert!(can(&bypass_ok, Some(bob)).await, "listed");
    assert!(!can(&bypass_ok, Some(ann)).await, "an admin not listed");
    let ghost = json!({ "mode": "members", "member_ids": ["00000000000000000000000009"] });
    let (status, _) = put(&bypass_ok, ghost).await;
    assert!(status.is_client_error(), "{status}");
    let (status, _) = put(&bypass_ok, json!({ "mode": "sometimes" })).await;
    assert!(status.is_client_error(), "{status}");
    // Only an owner sets it, a listed member included.
    let (status, _) = call_as(
        &bypass_ok,
        bob,
        "PUT",
        "/api/v1/settings/bypass",
        Some(json!({ "mode": "everyone" })),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // An admin may save a bypass workflow, but not run an eval of one until allowed.
    let task = new_task(&k.app, APPROVAL).await;
    let id = task["id"].as_str().unwrap().to_owned();
    until(&k.app, &id, "needs_you").await;
    let reject = format!("/api/v1/tasks/{id}/reject");
    let (status, _) = call(&k.app, "POST", &reject, Some(json!({ "reason": "no" }))).await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = call_as(
        &bypass_ok,
        ann,
        "POST",
        "/api/v1/evals",
        eval(&id, "bypass_permissions"),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert!(body.to_string().contains("Settings → Security"), "{body}");
    let (status, _) = put(&bypass_ok, json!({ "mode": "everyone" })).await;
    assert_eq!(status, StatusCode::OK);
    assert!(can(&bypass_ok, Some(ann)).await);
    let (status, body) = call_as(
        &bypass_ok,
        ann,
        "POST",
        "/api/v1/evals",
        eval(&id, "bypass_permissions"),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
}

/// `/me`'s `can_bypass` for `token` (`None`: the token holder).
async fn can(app: &axum::Router, token: Option<&str>) -> bool {
    let (status, me) = match token {
        Some(t) => call_as(app, t, "GET", "/api/v1/me", None).await,
        None => call(app, "GET", "/api/v1/me", None).await,
    };
    assert_eq!(status, StatusCode::OK, "{me}");
    me["can_bypass"] == true
}

/// `PUT /settings/bypass` as the owner.
async fn put(app: &axum::Router, body: Value) -> (StatusCode, Value) {
    call(app, "PUT", "/api/v1/settings/bypass", Some(body)).await
}

/// BLO-164 (review of #53): `POST /workflows/{id}/trigger` queues its run as
/// the caller, so a member's webhook run of a bypass workflow is capped like
/// any of theirs (the cap itself: `sun_engine`'s
/// `bypass::tests::a_webhook_run_acts_for_its_caller`). The token holder's
/// run acts for the deployment.
#[tokio::test]
async fn a_webhook_run_is_queued_as_its_caller() {
    let k = kernel().await;
    let bypass_ok = sun_api::router(k.api.clone().with_allow_bypass(true));
    let bob = member_token(&k.store, "bob", Role::Member).await;
    let yaml = "name: hooked\non: [webhook]\nsteps:\n  \
                - {id: implement, type: agent, prompt: p, permission_mode: bypass_permissions}\n";
    let save = json!({ "source_yaml": yaml, "workspace_id": MAIN });
    let (status, workflow) = call(&bypass_ok, "POST", "/api/v1/workflows", Some(save)).await;
    assert!(status.is_success(), "{workflow}");
    let trigger = format!(
        "/api/v1/workflows/{}/trigger",
        workflow["id"].as_str().unwrap()
    );
    let (_, me) = call_as(&bypass_ok, &bob, "GET", "/api/v1/me", None).await;
    let bob_id = me["memberships"][0]["member_id"].clone();

    let hook = || Some(json!({ "title": "deploy failed" }));
    let (status, task) = call_as(&bypass_ok, &bob, "POST", &trigger, hook()).await;
    assert_eq!(status, StatusCode::CREATED, "{task}");
    assert_eq!(queued_by(&k.app, &task).await, bob_id);
    let (status, task) = call(&bypass_ok, "POST", &trigger, hook()).await;
    assert_eq!(status, StatusCode::CREATED, "{task}");
    assert_eq!(queued_by(&k.app, &task).await, "token");
}

/// The `by` of the first `run_status` event of `task`'s run (`null`: none).
async fn queued_by(app: &axum::Router, task: &Value) -> Value {
    let run = task["run"]["id"].as_str().unwrap_or_default();
    let (_, events) = call(app, "GET", &format!("/api/v1/runs/{run}/events"), None).await;
    let first = events.as_array().into_iter().flatten();
    first
        .filter(|e| e["kind"] == "run_status")
        .map(|e| e["payload"]["by"].clone())
        .next()
        .unwrap_or_default()
}
