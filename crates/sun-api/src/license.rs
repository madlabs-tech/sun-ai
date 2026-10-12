//! The plan badge and license install: `GET /entitlement` resolves
//! `<data_dir>/license` on every read (cheap, and a renewed file is picked up
//! without a restart); `PUT|DELETE /license` (admin) replace or remove it.

use std::path::PathBuf;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde::Deserialize;
use sun_core::entitlement::{Entitlement, EntitlementStatus, Quota, verify_license};

use crate::Api;
use crate::error::{ApiError, ApiResult};
use crate::routes::Body;

/// Where the license token lives and the key it must verify against.
#[derive(Debug, Clone)]
pub struct Licensing {
    /// `<data_dir>/license` (the token text; not a secret).
    pub path: PathBuf,
    /// Sun's ed25519 license public key.
    pub public_key: [u8; 32],
}

impl Licensing {
    /// The effective entitlement now; never fails (a bad file is Community).
    pub fn resolve(&self) -> Entitlement {
        let token = std::fs::read_to_string(&self.path).ok();
        Entitlement::resolve(token.as_deref(), &self.public_key, chrono::Utc::now())
    }
}

fn licensing(api: &Api) -> ApiResult<&Licensing> {
    api.license.as_deref().ok_or_else(|| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "unavailable",
            "This kernel has no license file configured.",
        )
    })
}

/// `GET /entitlement`: plan, limits, status and the license (no token).
pub(crate) async fn get(State(api): State<Api>) -> Json<Entitlement> {
    Json(match api.license.as_deref() {
        Some(licensing) => licensing.resolve(),
        None => Entitlement::community(EntitlementStatus::Unlicensed),
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Install {
    token: String,
}

/// `PUT /license {token}`: verifies, then replaces the license file. A token
/// that fails to verify is 422 with the reason and changes nothing.
pub(crate) async fn put(
    State(api): State<Api>,
    Body(input): Body<Install>,
) -> ApiResult<Json<Entitlement>> {
    let licensing = licensing(&api)?;
    let token = input.token.trim();
    verify_license(token, &licensing.public_key)?;
    write(&licensing.path, token)?;
    Ok(Json(licensing.resolve()))
}

/// `DELETE /license`: back to Community (204; also when none was installed).
pub(crate) async fn remove(State(api): State<Api>) -> ApiResult<StatusCode> {
    let licensing = licensing(&api)?;
    match std::fs::remove_file(&licensing.path) {
        Ok(()) => Ok(StatusCode::NO_CONTENT),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(StatusCode::NO_CONTENT),
        Err(e) => Err(io_error(e)),
    }
}

/// Temp file + rename, so a reader never sees half a token.
fn write(path: &std::path::Path, token: &str) -> ApiResult<()> {
    let temp = path.with_extension("tmp");
    std::fs::write(&temp, token)
        .and_then(|()| std::fs::rename(&temp, path))
        .map_err(io_error)
}

fn io_error(e: std::io::Error) -> ApiError {
    ApiError::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        "internal",
        format!("Could not update the license file: {e}"),
    )
}

/// Refuses one more `what` (a [`Quota`]) when `current` already reach the
/// plan's cap, with a message saying how to get unstuck. No license
/// file or no cap: always allowed (self-hosting has no limits).
pub(crate) fn within_plan(api: &Api, quota: Quota, what: &str, current: usize) -> ApiResult<()> {
    let Some(licensing) = api.license.as_deref() else {
        return Ok(());
    };
    let entitlement = licensing.resolve();
    match entitlement.limits.max(quota) {
        Some(max) if current >= usize::try_from(max).unwrap_or(usize::MAX) => Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "plan_limit",
            format!(
                "Plan limit reached ({what}: {max} on the {} plan). Remove one or upgrade the plan.",
                entitlement.plan
            ),
        )),
        _ => Ok(()),
    }
}
