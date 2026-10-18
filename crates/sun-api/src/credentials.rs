//! `/api/v1/credentials` (TRD BLO-117 §6, item 7): a workspace's API keys
//! and Sun credits, which chain entries name (`credential: ollama-qwen`).
//! Any member reads them; `manage_runtimes` there adds, edits and deletes
//! them. `key_secret` (the name of the workspace secret holding the key) is
//! write-only: a response says only whether one is set
//! (`key_secret_set`), and never carries key material.

use std::collections::BTreeMap;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use serde_json::{Value, json};
use sun_core::model::{Credential, RuntimeSource};
use sun_core::{ByoKey, ByoKeyKind, CredentialId, Timestamp, WorkspaceId};
use sun_engine::CredentialRemoval;

use crate::error::{ApiError, ApiResult};
use crate::hosts::online_adapters;
use crate::routes::Body;
use crate::workspace_access::{InWorkspace, listed_workspace, named_workspace};
use crate::{Api, auth};

/// The deployment sells Sun credits. Off for the MVP: no new `credits`
/// credential is accepted, even with Sun Cloud connected.
// ponytail: a constant until credits are sold; then the `sun_cloud` gate alone.
const CREDITS_ON_SALE: bool = false;

/// Body of `POST /credentials` and `PUT /credentials/{name}`. A raw key is
/// refused (unknown field; `key_secret` names a secret). `default_model` is
/// the key's model (`byo_key`) or OpenCode's gateway model (`credits`).
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CredentialInput {
    #[serde(default)]
    name: String,
    source: RuntimeSource,
    #[serde(default)]
    default_tool: Option<String>,
    #[serde(default)]
    family: Option<ByoKeyKind>,
    #[serde(default)]
    endpoint: Option<String>,
    #[serde(default)]
    default_model: Option<String>,
    /// Absent on `PUT`: keep the stored one; `""`: none (a keyless endpoint).
    #[serde(default)]
    key_secret: Option<String>,
}

/// `GET /api/v1/credentials?workspace=`: the workspace's credentials by
/// name, each with its `state` (only a deployment owner may leave out
/// `?workspace=`).
pub(crate) async fn list(
    State(api): State<Api>,
    axum::Extension(caller): axum::Extension<auth::Caller>,
    Query(query): Query<InWorkspace>,
) -> ApiResult<Json<Vec<Value>>> {
    let workspace = listed_workspace(&caller, query.workspace.as_deref())?;
    let online = online_adapters(&api, workspace).await?;
    let cooling = api.engine.cooldowns().await?;
    let mut views = Vec::new();
    for credential in api.engine.credentials(workspace).await? {
        views.push(view(&api, &credential, &online, &cooling).await?);
    }
    Ok(Json(views))
}

/// `GET /api/v1/credentials/{name}?workspace=`: one credential.
pub(crate) async fn get(
    State(api): State<Api>,
    axum::Extension(caller): axum::Extension<auth::Caller>,
    Path(name): Path<String>,
    Query(query): Query<InWorkspace>,
) -> ApiResult<Json<Value>> {
    let workspace = named_workspace(&caller, query.workspace.as_deref())?;
    let credential = api.engine.credential(workspace, &name).await?;
    let online = online_adapters(&api, Some(workspace)).await?;
    let cooling = api.engine.cooldowns().await?;
    Ok(Json(view(&api, &credential, &online, &cooling).await?))
}

/// `POST /api/v1/credentials?workspace=` (`manage_runtimes` there): adds a
/// `byo_key` credential, or with Sun credits on sale a `credits` one;
/// `409` when the workspace has the name. Audited.
pub(crate) async fn create(
    State(api): State<Api>,
    axum::Extension(caller): axum::Extension<auth::Caller>,
    Query(query): Query<InWorkspace>,
    Body(input): Body<CredentialInput>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let workspace = named_workspace(&caller, query.workspace.as_deref())?;
    let name = input.name.clone();
    let (credential, model) = credential_of(&api, &caller, workspace, name, input, None)?;
    let plan = api.entitlements.for_workspace(workspace).await;
    let saved = api
        .engine
        .save_credential(&credential, model.as_deref(), false, &caller.actor(), &plan)
        .await?;
    if !saved {
        return Err(ApiError::conflict(format!(
            "name: a credential named `{}` already exists.",
            credential.name
        )));
    }
    let stored = api.engine.credential(workspace, &credential.name).await?;
    Ok((StatusCode::CREATED, Json(fresh_view(&api, &stored).await?)))
}

/// `PUT /api/v1/credentials/{name}?workspace=` (`manage_runtimes` there):
/// replaces its settings; a credential is never renamed (chains name it).
/// Without `key_secret` the stored one is kept. Audited.
pub(crate) async fn update(
    State(api): State<Api>,
    axum::Extension(caller): axum::Extension<auth::Caller>,
    Path(name): Path<String>,
    Query(query): Query<InWorkspace>,
    Body(input): Body<CredentialInput>,
) -> ApiResult<Json<Value>> {
    if !input.name.is_empty() && input.name != name {
        return Err(ApiError::invalid(
            "name: a credential can't be renamed; chains name it.",
        ));
    }
    let workspace = named_workspace(&caller, query.workspace.as_deref())?;
    let before = api.engine.credential(workspace, &name).await?;
    let (credential, model) = credential_of(&api, &caller, workspace, name, input, Some(&before))?;
    let plan = api.entitlements.for_workspace(workspace).await;
    if !api
        .engine
        .save_credential(&credential, model.as_deref(), true, &caller.actor(), &plan)
        .await?
    {
        return Err(sun_core::Error::NotFound(format!("credential {}", credential.name)).into());
    }
    let stored = api.engine.credential(workspace, &credential.name).await?;
    Ok(Json(fresh_view(&api, &stored).await?))
}

/// `DELETE /api/v1/credentials/{name}?workspace=` (`manage_runtimes`
/// there): `204`; `409` while an active agent's chain names it. Audited.
pub(crate) async fn remove(
    State(api): State<Api>,
    axum::Extension(caller): axum::Extension<auth::Caller>,
    Path(name): Path<String>,
    Query(query): Query<InWorkspace>,
) -> ApiResult<StatusCode> {
    let workspace = named_workspace(&caller, query.workspace.as_deref())?;
    match api
        .engine
        .delete_credential(workspace, &name, &caller.actor())
        .await?
    {
        CredentialRemoval::Removed => Ok(StatusCode::NO_CONTENT),
        CredentialRemoval::Missing => {
            Err(sun_core::Error::NotFound(format!("credential {name}")).into())
        }
        CredentialRemoval::NamedBy(agents) => Err(ApiError::conflict(format!(
            "`{name}` is in the chain of {}; take it out there first.",
            agents.join(", ")
        ))),
    }
}

/// The credential `input` describes for `caller` in `workspace`, checked by
/// [`Credential::validate`], and the credits gateway model. `before` is the
/// stored one on `PUT` (its id and, unless `key_secret` is sent, its key
/// secret are kept). The 422 message starts with the field to fix.
fn credential_of(
    api: &Api,
    caller: &auth::Caller,
    workspace: WorkspaceId,
    name: String,
    input: CredentialInput,
    before: Option<&Credential>,
) -> ApiResult<(Credential, Option<String>)> {
    match input.source {
        RuntimeSource::ByoKey => {}
        RuntimeSource::Credits if CREDITS_ON_SALE && api.sun_cloud => {}
        RuntimeSource::Credits => {
            return Err(ApiError::invalid(
                "source: Sun credits are not available on this deployment yet; bring your own \
                 key or runtime.",
            ));
        }
        RuntimeSource::ByoRuntime => {
            return Err(ApiError::invalid(
                "source: byo_key or credits; a CLI's own login needs no credential.",
            ));
        }
    }
    let kept_secret = before
        .and_then(|b| b.byo_key.as_ref())
        .and_then(|b| b.key_secret.clone());
    let key_secret = match input.key_secret {
        None => kept_secret,
        Some(name) if name.is_empty() => None,
        Some(name) => Some(name),
    };
    let (byo_key, model) = match input.source {
        RuntimeSource::ByoKey => {
            let byo = ByoKey {
                kind: input.family.unwrap_or_default(),
                endpoint: input.endpoint.unwrap_or_default(),
                model: input.default_model.unwrap_or_default(),
                key_secret,
            };
            (Some(byo), None)
        }
        _ if input.family.is_some() || input.endpoint.is_some() || key_secret.is_some() => {
            return Err(ApiError::invalid(
                "source: only a byo_key credential takes a family, endpoint or key_secret.",
            ));
        }
        _ => (None, input.default_model),
    };
    let credential = Credential {
        id: before.map_or_else(CredentialId::generate, |b| b.id),
        workspace_id: workspace,
        name,
        source: input.source,
        default_tool: input.default_tool.unwrap_or_else(|| "opencode".into()),
        byo_key,
        created_by: before.map_or(caller.member_id, |b| b.created_by),
    };
    match credential.validate() {
        Ok(()) => Ok((credential, model)),
        // `ByoKey` names its model `model`; here it is `default_model`.
        Err(sun_core::Error::InvalidInput(why)) => {
            Err(ApiError::invalid(match why.strip_prefix("model:") {
                Some(rest) => format!("default_model:{rest}"),
                None => why,
            }))
        }
        Err(e) => Err(e.into()),
    }
}

/// [`view`] of `credential`, read now.
async fn fresh_view(api: &Api, credential: &Credential) -> ApiResult<Value> {
    let online = online_adapters(api, Some(credential.workspace_id)).await?;
    let cooling = api.engine.cooldowns().await?;
    view(api, credential, &online, &cooling).await
}

/// What a response says of `credential`: its settings without the key
/// secret's name, and its `state` ([`crate::runtime_list::credential_state`]).
async fn view(
    api: &Api,
    credential: &Credential,
    online: &BTreeMap<String, (bool, bool)>,
    cooling: &BTreeMap<String, Timestamp>,
) -> ApiResult<Value> {
    let byo = credential.byo_key.as_ref();
    let default_model = match (byo, credential.source) {
        (Some(byo), _) => Some(byo.model.clone()),
        (None, RuntimeSource::Credits) => api.engine.credits_model(credential.id).await?,
        (None, _) => None,
    };
    let mut entry = json!({
        "id": credential.id,
        "name": credential.name,
        "workspace_id": credential.workspace_id,
        "source": credential.source,
        "family": credential.family(),
        "default_tool": credential.default_tool,
        "endpoint": byo.map(|b| b.endpoint.as_str()).filter(|e| !e.is_empty()),
        "default_model": default_model,
        "key_secret_set": byo.is_some_and(|b| b.key_secret.is_some()),
        "created_by": credential.created_by,
    });
    let state = crate::runtime_list::credential_state(api, credential, online, cooling).await?;
    if let (Some(entry), Value::Object(state)) = (entry.as_object_mut(), state) {
        entry.extend(state);
    }
    Ok(entry)
}
