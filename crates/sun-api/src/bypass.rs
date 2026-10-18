//! Settings → Security → Bypass permissions (BLO-164): the owner's
//! [`BypassPolicy`] at `/api/v1/settings/bypass`, and whether the caller may
//! start bypass sessions (`GET /me` `can_bypass`). The engine caps every
//! session at its start by the same rule ([`BypassPolicy::allows`]), so what
//! a client claims about itself changes nothing.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde_json::{Value, json};
use sun_core::model::BypassPolicy;

use crate::Api;
use crate::auth::Caller;
use crate::error::{ApiError, ApiResult};
use crate::routes::Body;

/// May `caller` start bypass sessions: sun.yaml `runtimes.allow_bypass`, then
/// the owner's policy (an owner always may).
pub(crate) async fn can_bypass(api: &Api, caller: &Caller) -> ApiResult<bool> {
    Ok(api.allow_bypass
        && api
            .engine
            .may_bypass(caller.is_owner, caller.member_id)
            .await?)
}

/// `403` unless `caller` may start bypass sessions ([`can_bypass`]); for what
/// runs as its caller at once, such as an eval's candidate.
pub(crate) async fn check_can_bypass(api: &Api, caller: &Caller) -> ApiResult<()> {
    match can_bypass(api, caller).await? {
        true => Ok(()),
        false => Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "forbidden",
            "bypass permissions is not enabled for you. The owner can enable it in Settings → \
             Security.",
        )),
    }
}

/// The policy with the operator's ceiling: `{mode, member_ids, allow_bypass}`.
fn view(api: &Api, policy: BypassPolicy) -> Value {
    json!({
        "mode": policy.mode,
        "member_ids": policy.member_ids,
        "allow_bypass": api.allow_bypass,
    })
}

/// `GET /api/v1/settings/bypass` (deployment owner).
pub(crate) async fn get_policy(State(api): State<Api>) -> ApiResult<Json<Value>> {
    let policy = api.engine.bypass_policy().await?;
    Ok(Json(view(&api, policy)))
}

/// `PUT /api/v1/settings/bypass {mode, member_ids}` (deployment owner):
/// saves it, audited; every member named must exist.
pub(crate) async fn put_policy(
    State(api): State<Api>,
    axum::Extension(caller): axum::Extension<Caller>,
    Body(policy): Body<BypassPolicy>,
) -> ApiResult<Json<Value>> {
    let policy = api
        .engine
        .set_bypass_policy(policy, &caller.actor())
        .await?;
    Ok(Json(view(&api, policy)))
}
