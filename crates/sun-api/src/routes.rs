//! REST handlers under `/api/v1`.

use std::str::FromStr;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, patch, post, put};
use axum::{Extension, Json, Router, middleware};
use serde::Deserialize;
use serde_json::{Value, json};
use sun_core::model::{
    Artifact, Assignee, ModKind, OutboxStatus, Permission, Priority, Run, RuntimeSource, Task,
};
use sun_core::{
    Error, NoteId, OutboxId, ProjectId, RunId, RunStatus, StateMachine, StepKey, StepStatus,
    TaskId, TaskStatus, WorkflowId, WorkspaceId,
};
use sun_engine::{NewTask, OnWorkspace, Resolution, TaskChange, WorkflowPick};

use crate::auth::Caller;
use crate::error::{ApiError, ApiResult};
use crate::wire::{self, TaskView, Workflows};
use crate::{Api, auth, sse, workspace_access};

pub(crate) const MAX_BODY: usize = 64 * 1024;
const MAX_WORKFLOW: usize = 256 * 1024;
const MAX_REASON: usize = 2000;
/// Events per JSON page.
const PAGE: u32 = 500;

pub(crate) fn router(api: Api) -> Router {
    let protected = Router::new()
        .route("/workspaces", get(crate::workspaces::list))
        .route("/workspaces/{id}/logo", get(crate::workspaces::get_logo))
        .route(
            "/workspaces/{id}/projects",
            get(crate::projects::list).post(crate::projects::create),
        )
        .route(
            "/projects/{id}",
            patch(crate::projects::update).delete(crate::projects::remove),
        )
        .route("/tasks", get(list_tasks).post(create_task))
        .route("/tasks/{id}", get(get_task).patch(crate::tasks::update))
        .route("/tasks/{id}/comments", post(crate::tasks::comment))
        .route("/chats", get(crate::chats::list).post(crate::chats::create))
        .route("/chats/{id}", patch(crate::chats::update))
        .route("/tasks/{id}/activity", get(crate::tasks::activity))
        .route("/tasks/{id}/runs", get(crate::tasks::runs))
        .route("/tasks/{id}/interrupt", post(crate::tasks::interrupt))
        .route("/tasks/{id}/cancel", post(cancel))
        .route("/tasks/{id}/pause", post(pause))
        .route("/tasks/{id}/resume", post(resume))
        .route("/tasks/{id}/bump", post(bump))
        // Linked pull requests: reads for whoever sees the task, writes
        // (link, remove, the merge rule) for `run`.
        .route(
            "/tasks/{id}/pull_requests",
            get(crate::pull_requests::list).post(crate::pull_requests::link),
        )
        .route(
            "/tasks/{id}/pull_requests/{pr_id}",
            delete(crate::pull_requests::unlink),
        )
        .route(
            "/tasks/{id}/pr_auto_complete",
            put(crate::pull_requests::set_auto_complete),
        )
        .route("/runs/{id}", get(get_run))
        .route("/runs/{id}/retry", post(retry))
        .route("/runs/{id}/messages", post(send_message))
        .route("/runs/{id}/steps/{key}/switch", post(switch_runtime))
        .route("/runs/{id}/steps/{key}/retry", post(retry_step))
        .route("/outbox/{id}/resolve", post(resolve_outbox))
        .route(
            "/tasks/{id}/on_limit",
            get(get_task_on_limit).put(put_task_on_limit),
        )
        .route(
            "/tasks/{id}/budget",
            get(get_task_budget).put(put_task_budget),
        )
        .route(
            "/runs/{id}/questions/{question}/answer",
            post(answer_question),
        )
        .route("/runs/{id}/events", get(events))
        .route("/runs/{id}/screenshots/{blob}", get(device_screenshot))
        .route("/workflows", get(list_workflows))
        .route("/workflows/{id}", get(get_workflow))
        .route("/workflows/{id}/trigger", post(trigger_workflow))
        .route("/templates", get(list_templates))
        .route("/entitlement", get(crate::entitlement::get))
        .route("/workspaces/{id}/repos", get(crate::vcs::list_repos))
        .route("/workspaces/{id}/vcs", get(crate::vcs::list))
        .route("/tasks/{id}/artifacts", get(list_artifacts))
        .route("/tasks/{id}/corrections", get(crate::corrections::of_task))
        .route("/tasks/{id}/decision", post(crate::corrections::decide))
        .route("/evals", get(crate::evals::list).post(crate::evals::start))
        .route("/evals/{id}", get(crate::evals::get))
        .route(
            "/eval_sets",
            get(crate::evals::sets).post(crate::evals::save_set),
        )
        .route("/tasks/{id}/knowledge_flags", get(knowledge_flags))
        .route("/tasks/{id}/quality", get(crate::quality::get))
        .route("/tasks/{id}/quality/check", post(crate::quality::check))
        .route(
            "/tasks/{id}/previews",
            get(crate::preview::list)
                .post(crate::preview::start)
                .delete(crate::preview::stop),
        )
        .route("/tasks/{id}/device", get(crate::device::live))
        .route(
            "/tasks/{id}/pins",
            get(crate::pins::list).post(crate::pins::create),
        )
        .route(
            "/tasks/{id}/pins/{pin}",
            patch(crate::pins::update).delete(crate::pins::remove),
        )
        .route("/tasks/{id}/pins/{pin}/commit", post(crate::pins::commit))
        .route(
            "/tasks/{id}/pins/{pin}/crops/{side}",
            get(crate::pins::crop),
        )
        .route("/tasks/{id}/screens", post(crate::pins::attach_screens))
        .route("/tasks/{id}/design", get(crate::pins::design))
        .route("/artifacts/{id}", get(download_artifact))
        .route(
            "/tasks/{id}/mcp",
            get(crate::catalog::task_mcp).put(crate::catalog::set_task_mcp),
        )
        .route("/me", get(auth::me))
        .route("/features", get(crate::features::get))
        // A workspace's agents, runtimes and memory, for any member there
        // (`?workspace=`; the guard keeps everyone else out).
        .route("/agents", get(crate::agents::list))
        .route("/runtimes", get(crate::hosts::list_runtimes))
        .route("/runtimes/{id}/readiness", get(crate::hosts::readiness))
        // Gone for every caller: no owner-only `403` in front of the `410`.
        .route(
            "/runtimes/{id}/policy",
            put(crate::hosts::runtime_policy_gone),
        )
        .route("/memory", get(crate::memory::list))
        .route("/memory/metrics", get(crate::memory::metrics))
        // Its hosts, catalog installs and MCP servers.
        .route("/hosts", get(crate::hosts::list_hosts))
        .route("/catalog", get(crate::catalog::list))
        .route("/catalog/{id}/files", get(crate::catalog::files))
        .route(
            "/catalog/{id}/repos/{repo_id}",
            get(crate::catalog::repo_install),
        )
        .route("/mcp_servers", get(crate::catalog::mcp_servers))
        .route(
            "/mcp_servers/{name}/tools",
            get(crate::catalog::mcp_server_tools),
        )
        .merge(knowledge_routes())
        .route_layer(middleware::from_fn(auth::require_by_method));
    // A workspace's agents and runtimes, by the caller's role there.
    let runtime_manage = Router::new()
        .route("/agents", post(crate::agents::create))
        .route(
            "/agents/{id}",
            patch(crate::agents::update).delete(crate::agents::archive),
        )
        .route("/runtimes", post(crate::hosts::create_runtime))
        .route("/runtimes/{id}", put(crate::hosts::update_runtime))
        .route(
            "/workspaces/{id}/hosts/tokens",
            post(crate::hosts::create_token),
        )
        .route(
            "/workspaces/{id}/hosts/{host}",
            put(crate::hosts::attach_host).delete(crate::hosts::detach_host),
        )
        .route(
            "/workspaces/{id}/hosts/{host}/reprobe",
            post(crate::hosts::reprobe_host),
        )
        .route_layer(middleware::from_fn(auth::require(&[
            Permission::ManageRuntimes,
        ])));
    // A workspace's catalog installs and MCP servers.
    let plugin_manage = Router::new()
        .route("/catalog/{id}/install", post(crate::catalog::install))
        .route("/catalog/{id}", delete(crate::catalog::uninstall))
        .route("/mcp_servers", post(crate::catalog::add_mcp_server))
        .route(
            "/mcp_servers/{name}",
            delete(crate::catalog::remove_mcp_server),
        )
        .route_layer(middleware::from_fn(auth::require(&[
            Permission::ManagePlugins,
        ])));
    // A workspace's secrets.
    let secret_manage = Router::new()
        .route("/secrets", get(crate::secrets::list))
        .route(
            "/secrets/workspace/{id}/{name}",
            put(crate::secrets::put_in_workspace).delete(crate::secrets::remove_in_workspace),
        )
        .route_layer(middleware::from_fn(auth::require(&[
            Permission::ManageSecrets,
        ])));
    // A workspace's memory: reviews and the stars that propose it.
    let memory_manage = Router::new()
        .route("/memory/{id}/review", post(crate::memory::review))
        .route(
            "/corrections/{id}/star",
            post(crate::memory::star_correction),
        )
        .route("/notes/{id}/star", post(crate::memory::star_note))
        .route_layer(middleware::from_fn(auth::require(&[
            Permission::ManageSettings,
        ])));
    let workflow_create = Router::new()
        .route("/workflows", post(create_workflow))
        .route("/templates/{id}/install", post(install_template))
        .route_layer(middleware::from_fn(auth::require(&[
            Permission::CreateWorkflow,
        ])));
    let workflow_edit = Router::new()
        .route("/workflows/{id}/versions", post(save_workflow_version))
        .route_layer(middleware::from_fn(auth::require(&[
            Permission::EditWorkflow,
        ])));
    let workspace_edit = Router::new()
        .route("/workspaces/{id}", patch(crate::workspaces::update))
        .route(
            "/workspaces/{id}/logo",
            put(crate::workspaces::put_logo).delete(crate::workspaces::delete_logo),
        )
        .route(
            "/workspaces/{id}/prefix",
            post(crate::workspaces::set_prefix),
        )
        .route_layer(middleware::from_fn(auth::require(&[
            Permission::EditWorkspace,
        ])));
    // A workspace's repositories: add, edit, remove, pick from a connection.
    let repo_edit = Router::new()
        .route("/workspaces/{id}/repos", post(crate::vcs::add_repo))
        .route("/repos/{id}", patch(crate::vcs::update_repo))
        .route(
            "/workspaces/{id}/repos/{repo_id}",
            delete(crate::workspaces::remove_repo),
        )
        .route(
            "/workspaces/{id}/vcs/{connection_id}/repos",
            get(crate::vcs::remote_repos),
        )
        .route_layer(middleware::from_fn(auth::require(&[
            Permission::EditWorkspace,
        ])));
    // Git connections hold tokens: both permissions.
    let vcs_manage = Router::new()
        .route(
            "/workspaces/{id}/vcs/github/connect",
            get(crate::vcs::github_connect),
        )
        .route(
            "/workspaces/{id}/vcs/gitlab/connect",
            get(crate::vcs::gitlab_connect),
        )
        .route("/vcs/connections/{id}", delete(crate::vcs::disconnect))
        .route(
            "/vcs/connections/{id}/rotate-webhook",
            post(crate::vcs::rotate_webhook),
        )
        .route_layer(middleware::from_fn(auth::require(&[
            Permission::EditWorkspace,
        ])))
        .route_layer(middleware::from_fn(auth::require(&[
            Permission::ManageSecrets,
        ])));
    let workspace_delete = Router::new()
        .route("/workspaces/{id}", delete(crate::workspaces::remove))
        .route_layer(middleware::from_fn(auth::require(&[
            Permission::DeleteWorkspace,
        ])));
    // A workspace's members and invitations, by the caller's role there.
    let member_edit = Router::new()
        .route(
            "/workspaces/{id}/members/{member}",
            put(auth::set_workspace_role).delete(auth::remove_workspace_member),
        )
        .route_layer(middleware::from_fn(auth::require(&[
            Permission::EditMembers,
        ])));
    let invite = Router::new()
        .route(
            "/workspaces/{id}/invitations",
            get(auth::list_invitations).post(auth::create_invitation),
        )
        .route(
            "/workspaces/{id}/invitations/{invitation}",
            delete(auth::revoke_invitation),
        )
        .route_layer(middleware::from_fn(auth::require(&[
            Permission::InviteMembers,
        ])));
    // Importing a knowledge source needs `manage_settings` in both workspaces.
    let knowledge_import = Router::new()
        .route(
            "/workspaces/{id}/knowledge_sources/{source_id}",
            post(crate::workspaces::import_knowledge_source)
                .delete(crate::workspaces::detach_knowledge_source),
        )
        .route_layer(middleware::from_fn(auth::require(&[
            Permission::ManageSettings,
        ])));
    // The deployment itself, for deployment owners only: its accounts, mail,
    // sign-in, security, audit, the kernel's own resources and sessions,
    // mods, ecosystem and mod secrets, limits, dismissed catalog suggestions,
    // usage and marketplace.
    let deployment = Router::new()
        .route(
            "/members",
            get(auth::list_members).post(auth::invite_member),
        )
        .route("/members/{id}/password", put(auth::set_member_password))
        .route("/members/{id}/invite", post(auth::reinvite_member))
        .route(
            "/members/{id}/two_step",
            delete(auth::reset_member_two_step),
        )
        .route("/members/{id}/deactivate", post(auth::deactivate_member))
        .route("/members/{id}/activate", post(auth::activate_member))
        .route("/members/{id}/owner", put(auth::set_owner))
        .route("/kernel", get(crate::hosts::kernel))
        .route("/sessions/{id}/kill", post(crate::hosts::kill_session))
        .route("/marketplace/listings", get(crate::marketplace::listings))
        .route(
            "/marketplace/leases",
            get(crate::marketplace::leases).post(crate::marketplace::lease),
        )
        .route(
            "/marketplace/policy",
            get(crate::marketplace::policy).put(crate::marketplace::set_policy),
        )
        .route(
            "/marketplace/listings/mine",
            get(crate::marketplace::my_listings),
        )
        .route(
            "/marketplace/listings/{id}/status",
            post(crate::marketplace::set_listing_status),
        )
        .route(
            "/marketplace/payouts",
            get(crate::marketplace::payouts).post(crate::marketplace::request_payout),
        )
        .route(
            "/marketplace/leases/{id}/rating",
            post(crate::marketplace::rate_lease),
        )
        .route(
            "/marketplace/leases/{id}/accept",
            post(crate::marketplace::accept),
        )
        .route(
            "/marketplace/leases/{id}/dispute",
            post(crate::marketplace::dispute),
        )
        .route("/smtp", get(auth::get_smtp).put(auth::put_smtp))
        .route("/smtp/test", post(auth::test_smtp))
        .route(
            "/settings/security",
            get(auth::get_security_policy).put(auth::put_security_policy),
        )
        .route(
            "/settings/bypass",
            get(crate::bypass::get_policy).put(crate::bypass::put_policy),
        )
        .route("/settings/sso", get(auth::list_sso))
        .route(
            "/settings/sso/{id}",
            put(auth::put_sso).delete(auth::delete_sso),
        )
        .route("/audit", get(auth::audit_log))
        .route(
            "/settings/embeddings",
            get(crate::knowledge::get_embeddings).put(crate::knowledge::put_embeddings),
        )
        .route("/settings/on_limit", get(get_on_limit).put(put_on_limit))
        .route("/settings/budget", get(get_spend_cap).put(put_spend_cap))
        .route("/decisions/export", get(crate::corrections::export))
        .route("/decisions/replay", post(crate::corrections::replay))
        .route(
            "/secrets/{kind}/{id}/{name}",
            put(crate::secrets::put).delete(crate::secrets::remove),
        )
        .route("/mods/{name}/install", post(install_mod))
        .route("/mods/{name}", put(update_mod).delete(remove_mod))
        .route("/mods", get(list_mods))
        // Dismissed suggestions are the deployment's.
        .route(
            "/catalog/{id}/dismissed",
            put(crate::catalog::dismiss).delete(crate::catalog::restore),
        )
        // Deployment-wide reads, until they are per workspace.
        .route("/usage", get(usage))
        .route("/quality/metrics", get(crate::quality::metrics))
        .route("/corrections", get(crate::corrections::list))
        .route("/outbox", get(list_outbox))
        .route_layer(middleware::from_fn(auth::require_owner));
    let approvals = Router::new()
        .route("/tasks/{id}/approve", post(approve))
        .route("/tasks/{id}/reject", post(reject))
        .route_layer(middleware::from_fn(auth::require(&[Permission::Approve])));
    let protected = protected
        .merge(workflow_create)
        .merge(workflow_edit)
        .merge(workspace_edit)
        .merge(workspace_delete)
        .merge(repo_edit)
        .merge(vcs_manage)
        .merge(member_edit)
        .merge(invite)
        .merge(knowledge_import)
        .merge(runtime_manage)
        .merge(plugin_manage)
        .merge(secret_manage)
        .merge(memory_manage)
        .merge(deployment)
        .merge(approvals)
        // Any signed-in member creates a workspace (the handler applies
        // `SUN_DISABLE_WORKSPACE_CREATION`).
        .route("/workspaces", post(crate::workspaces::create))
        // Any member of a workspace sees its members and may leave it (the
        // guard keeps everyone else out).
        .route("/workspaces/{id}/members", get(auth::workspace_members))
        .route("/workspaces/{id}/leave", post(auth::leave_workspace))
        // The caller's own invitations.
        .route("/me/invitations", get(auth::my_invitations))
        .route("/invitations/{id}/accept", post(auth::accept_invitation))
        .route("/invitations/{id}/decline", post(auth::decline_invitation))
        // The builder's checks change nothing: any signed-in member.
        .route("/workflows/validate", post(validate_workflow))
        .route("/workflows/dry-run", post(dry_run_workflow))
        // Any signed-in member sets their own view.
        .route("/me/view_role", put(auth::set_view_role))
        // Any signed-in member sets their own password, and an email once.
        .route("/me/password", put(auth::set_my_password))
        .route("/me/email", put(auth::set_my_email))
        // Any signed-in member manages their own second steps.
        .route("/me/two_step", get(auth::my_two_step))
        .route("/me/two_step/totp", post(auth::start_my_totp))
        .route("/me/two_step/totp/confirm", post(auth::confirm_my_totp))
        .route("/me/two_step/email", post(auth::add_my_two_step_email))
        .route("/me/two_step/passkey", post(auth::start_my_passkey))
        .route(
            "/me/two_step/passkey/confirm",
            post(auth::confirm_my_passkey),
        )
        .route("/me/two_step/challenge", post(auth::my_two_step_challenge))
        .route("/me/two_step/{id}", delete(auth::remove_my_two_step))
        // Plans and wallets: each handler checks the person and their role.
        .merge(crate::billing::routes())
        // Support from people at Sun: any signed-in person; Sun Cloud says
        // who pays for it.
        .route(
            "/support",
            get(crate::billing::tickets).post(crate::billing::open_ticket),
        )
        // Runs after `authenticate`: 404 for anything on a hidden workspace.
        .route_layer(middleware::from_fn_with_state(
            api.clone(),
            crate::workspace_access::guard,
        ))
        .route_layer(middleware::from_fn_with_state(
            api.clone(),
            auth::authenticate,
        ));
    let open = Router::new()
        // Sun Cloud asks with its own token (no session).
        .route(
            "/internal/workspaces/{id}/seat_count",
            get(crate::billing::seat_count),
        )
        // Payment providers post here; the cloud verifies the signature.
        .route(
            "/payments/webhooks/{provider}",
            post(crate::billing::payment_webhook).layer(axum::extract::DefaultBodyLimit::max(
                crate::billing::MAX_WEBHOOK,
            )),
        )
        .route("/session", post(auth::sign_in).delete(auth::sign_out))
        .route("/session/two_step", post(auth::sign_in_two_step))
        .route("/session/two_step/email", post(auth::mail_two_step_code))
        .route("/signup", post(auth::signup))
        // The invitation link's page: the token is the credential.
        .route("/invitations/{id}", get(auth::look_up_invitation))
        .route("/auth", get(auth::providers))
        .route("/auth/sso/{id}/login", get(auth::sso_login))
        .route("/auth/sso/{id}/callback", get(auth::sso_callback))
        .route(
            "/webhooks/{mod_name}",
            post(crate::webhooks::receive)
                .head(|| async { StatusCode::OK })
                .layer(axum::extract::DefaultBodyLimit::max(
                    crate::webhooks::MAX_BODY,
                )),
        )
        .route(
            "/webhooks/vcs/{connection_id}",
            post(crate::webhooks::receive_vcs).layer(axum::extract::DefaultBodyLimit::max(
                crate::webhooks::MAX_BODY,
            )),
        )
        // The git host sends the browser back here; the signed state and its
        // cookie prove who started it.
        .route("/vcs/github/setup", get(crate::vcs::github_setup))
        .route("/vcs/gitlab/callback", get(crate::vcs::gitlab_callback));
    Router::new()
        .nest("/api/v1", protected.merge(open))
        .with_state(api)
}

/// Knowledge sources, search and task context.
fn knowledge_routes() -> Router<Api> {
    use crate::knowledge as k;
    Router::new()
        .route(
            "/knowledge/sources",
            get(k::list_sources).post(k::add_source),
        )
        .route("/knowledge/sources/{id}", delete(k::delete_source))
        .route(
            "/knowledge/sources/{id}/docs",
            // Base64 files (≤ 8 MB each) outgrow the 2 MB default.
            get(k::list_docs)
                .post(k::upload_docs)
                .layer(axum::extract::DefaultBodyLimit::max(24 * 1024 * 1024)),
        )
        .route("/knowledge/sources/{id}/reindex", post(k::reindex))
        .route("/knowledge/docs/{id}", get(k::get_doc))
        .route("/knowledge/search", get(k::search))
        .route(
            "/tasks/{id}/context",
            get(k::get_context).put(k::put_context),
        )
        .route("/tasks/{id}/context/preview", post(k::preview_context))
}

/// Parses a path or body id; a bad one is 422, not 500.
pub(crate) fn id<T: FromStr>(text: &str, what: &str) -> ApiResult<T> {
    text.parse()
        .map_err(|_| ApiError::invalid(format!("`{text}` is not a valid {what} id.")))
}

/// JSON body with a plain-language 422 instead of axum's text rejection.
pub(crate) struct Body<T>(pub(crate) T);

impl<T, S> axum::extract::FromRequest<S> for Body<T>
where
    T: serde::de::DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request(request: axum::extract::Request, state: &S) -> ApiResult<Self> {
        Json::<T>::from_request(request, state)
            .await
            .map(|Json(value)| Body(value))
            .map_err(|e| {
                ApiError::invalid(format!("The request body is not valid: {}", e.body_text()))
            })
    }
}

fn installed_view(m: &sun_core::model::ModInstall) -> Value {
    json!({
        "name": m.name, "kind": m.kind, "version": m.version,
        "installed": true,
        "permissions": m.granted_permissions,
        "signed": m.signed_by.is_some(),
    })
}

/// `GET /api/v1/mods`: installed mods with the permissions granted, the
/// extensions that need each (`needed_by`) and a newer installable version
/// (`update`: its version and permissions, or null), then the mod registry's
/// ones not installed yet with the permissions they ask for, their tier
/// (`official` | `community`), repo and description.
async fn list_mods(State(api): State<Api>) -> ApiResult<Json<Vec<Value>>> {
    let installed = api.engine.mod_installs().await?;
    let available = match &api.mod_installer {
        Some(installer) => installer.available().await?,
        None => Vec::new(),
    };
    let installed_views = installed.iter().map(|m| {
        let mut view = installed_view(m);
        let needed_by: Vec<&str> = installed
            .iter()
            .filter(|other| other.needs_plugin(&m.name))
            .map(|other| other.name.as_str())
            .collect();
        let update = available
            .iter()
            .find(|a| a.name == m.name && a.version != m.version)
            .map(|a| json!({ "version": a.version, "permissions": a.permissions }));
        view["needed_by"] = json!(needed_by);
        view["update"] = json!(update);
        view
    });
    let available = available
        .iter()
        .filter(|a| !installed.iter().any(|m| m.name == a.name))
        .map(|a| {
            json!({
                "name": a.name, "kind": a.kind, "version": a.version,
                "installed": false,
                "permissions": a.permissions,
                "signed": a.signed,
                "tier": a.tier,
                "repo": a.repo,
                "description": a.description,
            })
        });
    Ok(Json(installed_views.chain(available).collect()))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InstallMod {
    /// Exactly the permissions the user was shown and approved.
    permissions: Vec<String>,
}

/// `POST /api/v1/mods/{name}/install` (deployment owner): installs a mod of
/// the mod registry from its GitHub Release with the approved permissions
/// and loads it at once.
async fn install_mod(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Path(name): Path<String>,
    Body(input): Body<InstallMod>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let installer = mod_installer(&api, &name)?;
    if input.permissions.len() > 256 || input.permissions.iter().any(|p| p.len() > 512) {
        return Err(ApiError::invalid("Too many or too long permissions."));
    }
    let record = installer
        .install(&name, &input.permissions, &caller.actor())
        .await?;
    Ok((StatusCode::CREATED, Json(installed_view(&record))))
}

/// `PUT /api/v1/mods/{name}`: updates an installed mod to the version
/// `GET /mods` offers as its `update`, with that version's approved permissions.
async fn update_mod(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Path(name): Path<String>,
    body: Body<InstallMod>,
) -> ApiResult<Json<Value>> {
    installed_kind(&api, &name).await?;
    let (_, view) = install_mod(State(api), Extension(caller), Path(name), body).await?;
    Ok(view)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RemoveMod {
    /// Also delete its data and secrets.
    #[serde(default)]
    purge: bool,
}

/// `DELETE /api/v1/mods/{name}[?purge=true]`: stops and uninstalls it; its
/// data and secrets stay unless `purge`. `needed_by` names the extensions
/// that declared it (they stay installed, without it).
async fn remove_mod(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Path(name): Path<String>,
    Query(query): Query<RemoveMod>,
) -> ApiResult<Json<Value>> {
    let installer = mod_installer(&api, &name)?;
    installed_kind(&api, &name).await?;
    let needed_by = installer
        .remove(&name, query.purge, &caller.actor())
        .await?;
    Ok(Json(json!({ "removed": name, "needed_by": needed_by })))
}

/// The kernel's installer, for a valid mod `name`.
fn mod_installer<'a>(api: &'a Api, name: &str) -> ApiResult<&'a dyn sun_core::ModInstaller> {
    let valid_name = !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if !valid_name {
        return Err(ApiError::invalid(format!("`{name}` is not a mod name.")));
    }
    api.mod_installer.as_deref().ok_or_else(|| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "unavailable",
            "This kernel can’t change mods from the UI. Use `sun plugin add|update|remove`.",
        )
    })
}

/// 404 unless mod `name` is installed.
async fn installed_kind(api: &Api, name: &str) -> ApiResult<ModKind> {
    let installs = api.engine.mod_installs().await?;
    match installs.into_iter().find(|m| m.name == name) {
        Some(m) => Ok(m.kind),
        None => Err(sun_core::Error::NotFound(format!("mod {name} is not installed")).into()),
    }
}

#[derive(Deserialize)]
struct TaskFilter {
    /// A task status, e.g. `in_progress`.
    status: Option<String>,
    /// Only this workspace's tasks.
    workspace: Option<String>,
    /// Only this project's tasks.
    project: Option<String>,
    /// Only tasks assigned to this agent.
    assignee: Option<String>,
}

/// `?workspace=<id>`: one workspace (404 if missing); absent: every workspace.
#[derive(Deserialize)]
pub(crate) struct WorkspaceFilter {
    pub(crate) workspace: Option<String>,
}

pub(crate) fn workspace_param(workspace: Option<&str>) -> ApiResult<Option<WorkspaceId>> {
    workspace.map(|b| id(b, "workspace")).transpose()
}

/// `GET /tasks?workspace=&status=&project=&assignee=`.
async fn list_tasks(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Query(filter): Query<TaskFilter>,
) -> ApiResult<Json<Vec<TaskView>>> {
    let status = filter
        .status
        .as_deref()
        .map(|s| {
            s.parse::<TaskStatus>()
                .map_err(|_| ApiError::invalid(format!("`{s}` is not a task status.")))
        })
        .transpose()?;
    let project = filter
        .project
        .as_deref()
        .map(|p| id::<ProjectId>(p, "project"))
        .transpose()?;
    let workflows = Workflows::load(&api.engine).await?;
    let mut tasks = match workspace_param(filter.workspace.as_deref())? {
        Some(workspace) => api.engine.tasks_of_workspace(workspace).await?,
        None => api.engine.tasks().await?,
    };
    tasks.retain(|t| {
        caller.sees(t.workspace_id)
            && status.is_none_or(|s| t.status == s)
            && project.is_none_or(|p| t.project_id == Some(p))
            && filter
                .assignee
                .as_deref()
                .is_none_or(|a| t.assignee_agent_id.is_some_and(|id| id.to_string() == a))
    });
    let mut out = Vec::with_capacity(tasks.len());
    for task in tasks {
        out.push(wire::task_view(&api.engine, &workflows, task, false).await?);
    }
    Ok(Json(out))
}

async fn get_task(State(api): State<Api>, Path(task): Path<String>) -> ApiResult<Json<TaskView>> {
    let task = api.engine.task(id(&task, "task")?).await?;
    Ok(Json(detail(&api, task).await?))
}

/// `GET /runs/{id}`: any run of a task (not only the latest) with its steps
/// and gates.
async fn get_run(
    State(api): State<Api>,
    Path(run): Path<String>,
) -> ApiResult<Json<wire::RunDetailView>> {
    Ok(Json(wire::run_detail(&api.engine, id(&run, "run")?).await?))
}

pub(crate) async fn summary(api: &Api, task_id: TaskId) -> ApiResult<Json<TaskView>> {
    let task = api.engine.task(task_id).await?;
    let workflows = Workflows::load(&api.engine).await?;
    Ok(Json(
        wire::task_view(&api.engine, &workflows, task, false).await?,
    ))
}

async fn detail(api: &Api, task: Task) -> ApiResult<TaskView> {
    let workflows = Workflows::load(&api.engine).await?;
    Ok(wire::task_view(&api.engine, &workflows, task, true).await?)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateTask {
    title: String,
    #[serde(default)]
    body: String,
    /// Required without a workflow (or with `default`); else the workflow's.
    #[serde(default)]
    workspace_id: Option<String>,
    /// A workflow id, `default` (the workspace's default workflow), or
    /// absent: a plain task that never runs.
    #[serde(default, alias = "workflow")]
    workflow_id: Option<String>,
    /// Overrides the workflow's `exec_env:`.
    #[serde(default)]
    exec_env: Option<sun_core::ExecEnvSpec>,
    /// Default `none`.
    #[serde(default)]
    priority: Option<Priority>,
    /// `backlog` or `todo` (default): only a `todo` task with a workflow runs.
    #[serde(default)]
    status: Option<TaskStatus>,
    #[serde(default)]
    project_id: Option<String>,
    /// `{kind: "agent" | "workflow", id}`; instead of `workflow_id`.
    #[serde(default)]
    assignee: Option<Assignee>,
}

/// The range the old integer `priority` may take (`bump`).
fn check_priority(priority: i32) -> ApiResult<()> {
    if (-1000..=1000).contains(&priority) {
        Ok(())
    } else {
        Err(ApiError::invalid(
            "Priority must be between -1000 and 1000.",
        ))
    }
}

/// Creates a task, and queues its run when it has a workflow and is `todo`
/// (the workspace's "New task").
async fn create_task(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Body(input): Body<CreateTask>,
) -> ApiResult<(StatusCode, Json<TaskView>)> {
    if input.body.len() > MAX_BODY {
        return Err(ApiError::invalid(
            "The task description is longer than 64 KB.",
        ));
    }
    let workflow = match (input.assignee, input.workflow_id.as_deref()) {
        (Some(_), Some(_)) => {
            return Err(ApiError::invalid(
                "Give the task an assignee or a workflow_id, not both.",
            ));
        }
        (Some(Assignee::Agent { id }), None) => WorkflowPick::Agent(id),
        (Some(Assignee::Workflow { id }), None) => WorkflowPick::Id(id),
        (None, None) => WorkflowPick::None,
        (None, Some("default")) => WorkflowPick::Default,
        (None, Some(workflow)) => WorkflowPick::Id(id(workflow, "workflow")?),
    };
    let new = NewTask {
        workspace_id: workspace_param(input.workspace_id.as_deref())?,
        title: input.title,
        body: input.body,
        workflow,
        exec_env: input.exec_env,
        priority: input.priority.unwrap_or(Priority::None),
        status: input.status.unwrap_or(TaskStatus::Todo),
        project_id: input
            .project_id
            .as_deref()
            .map(|p| id(p, "project"))
            .transpose()?,
        actor: Some(caller.task_actor()),
    };
    let task = api.engine.create_task(new).await?;
    let Json(view) = summary(&api, task.id).await?;
    Ok((StatusCode::CREATED, Json(view)))
}

/// A webhook: creates and queues a task on a workflow whose `on:`
/// has `webhook`. Body `{title?, text?, url?, repo?, key?, ..}`; a repeated
/// `key` answers 200 with the task it made the first time. The run acts
/// for the caller, so their bypass policy caps it.
async fn trigger_workflow(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Path(workflow): Path<String>,
    Body(payload): Body<Value>,
) -> ApiResult<(StatusCode, Json<TaskView>)> {
    if !payload.is_object() {
        return Err(ApiError::invalid("Send a JSON object."));
    }
    if payload.to_string().len() > MAX_BODY {
        return Err(ApiError::invalid("The payload is longer than 64 KB."));
    }
    let workflow_id: WorkflowId = id(&workflow, "workflow")?;
    let by = caller.task_actor();
    let (task_id, created) = api
        .engine
        .trigger_webhook(workflow_id, &payload, Some(&by))
        .await?;
    let Json(view) = summary(&api, task_id).await?;
    let status = match created {
        true => StatusCode::CREATED,
        false => StatusCode::OK,
    };
    Ok((status, Json(view)))
}

/// The task's latest run, or 409 when it never ran.
pub(crate) async fn latest_run(api: &Api, task_id: TaskId) -> ApiResult<Run> {
    api.engine
        .runs_of_task(task_id)
        .await?
        .pop()
        .ok_or_else(|| ApiError::conflict("This task has not started yet."))
}

/// Approves (`rejected: None`) or rejects the waiting approval step.
async fn decide(
    api: &Api,
    by: Caller,
    task: &str,
    rejected: Option<String>,
) -> ApiResult<Json<TaskView>> {
    let task_id = id(task, "task")?;
    let run = latest_run(api, task_id).await?;
    let step = api
        .engine
        .steps(run.id)
        .await?
        .into_iter()
        .find(|s| s.status == StepStatus::WaitingApproval)
        .ok_or_else(|| ApiError::conflict("This task is not waiting for approval."))?;
    let by = by.principal_in(api.engine.workspace_of_task(task_id).await?);
    match rejected {
        None => api.engine.decide(run.id, step.step_key, true, by).await?,
        Some(reason) if reason.is_empty() => {
            api.engine.decide(run.id, step.step_key, false, by).await?
        }
        Some(reason) => api.engine.reject(run.id, step.step_key, by, reason).await?,
    }
    summary(api, task_id).await
}

async fn approve(
    State(api): State<Api>,
    Extension(by): Extension<Caller>,
    Path(task): Path<String>,
) -> ApiResult<Json<TaskView>> {
    decide(&api, by, &task, None).await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Reject {
    #[serde(default)]
    reason: String,
}

async fn reject(
    State(api): State<Api>,
    Extension(by): Extension<Caller>,
    Path(task): Path<String>,
    Body(input): Body<Reject>,
) -> ApiResult<Json<TaskView>> {
    let reason = input.reason.trim().to_owned();
    if reason.len() > MAX_REASON {
        return Err(ApiError::invalid(format!(
            "Keep the reason under {MAX_REASON} characters."
        )));
    }
    decide(&api, by, &task, Some(reason)).await
}

/// Pauses the task's running run: its agents stop, nothing new starts.
async fn pause(State(api): State<Api>, Path(task): Path<String>) -> ApiResult<Json<TaskView>> {
    let task_id = id(&task, "task")?;
    let run = latest_run(&api, task_id).await?;
    if run.status != RunStatus::Running {
        return Err(ApiError::conflict("Only a running task can be paused."));
    }
    api.engine.pause(run.id).await?;
    summary(&api, task_id).await
}

/// Resumes the task's paused run, as the caller.
async fn resume(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Path(task): Path<String>,
) -> ApiResult<Json<TaskView>> {
    let task_id = id(&task, "task")?;
    let run = latest_run(&api, task_id).await?;
    if run.status != RunStatus::Paused {
        return Err(ApiError::conflict("This task is not paused."));
    }
    api.engine.resume(run.id, caller.task_actor()).await?;
    summary(&api, task_id).await
}

/// Cancels the task and its run, if any: a task that never ran too.
async fn cancel(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Path(task): Path<String>,
) -> ApiResult<Json<TaskView>> {
    let task_id = id(&task, "task")?;
    crate::tasks::move_to(&api, task_id, TaskStatus::Cancelled, caller.task_actor()).await?;
    summary(&api, task_id).await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Bump {
    priority: i32,
}

/// The old integer priority (above 0 high, below 0 low, 0 none). Kept for
/// one release; `PATCH /tasks/{id} {priority}` replaces it.
async fn bump(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Path(task): Path<String>,
    Body(input): Body<Bump>,
) -> ApiResult<Json<TaskView>> {
    let task_id = id(&task, "task")?;
    check_priority(input.priority)?;
    let change = TaskChange {
        priority: Some(Priority::from_int(input.priority)),
        ..TaskChange::default()
    };
    api.engine
        .update_task(task_id, change, Some(caller.task_actor()))
        .await?;
    summary(&api, task_id).await
}

/// Queues a new run of a finished run's task, as the caller.
async fn retry(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Path(run): Path<String>,
) -> ApiResult<Json<TaskView>> {
    let run = api.engine.run(id(&run, "run")?).await?;
    if !run.status.is_terminal() {
        return Err(ApiError::conflict(
            "This run is still going; cancel it first.",
        ));
    }
    let by = caller.task_actor();
    api.queue.enqueue_by(run.task_id, Some(&by)).await?;
    summary(&api, run.task_id).await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Message {
    body: String,
    /// Leave it for the run's next step instead of steering now.
    #[serde(default)]
    queue: bool,
}

/// Steers the run's live agent, or with `queue: true` leaves the
/// message for its next step, as the caller.
async fn send_message(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Path(run): Path<String>,
    Body(input): Body<Message>,
) -> ApiResult<Json<wire::EventView>> {
    let run_id: RunId = id(&run, "run")?;
    let text = input.body.trim();
    if text.is_empty() || text.len() > MAX_BODY {
        return Err(ApiError::invalid("Write a message of up to 64 KB first."));
    }
    let event = match input.queue {
        true => {
            let by = caller.task_actor();
            api.engine
                .queue_message(run_id, text.to_string(), by)
                .await?
        }
        false => {
            let by = caller.task_actor();
            api.engine.steer(run_id, text.to_string(), by).await?
        }
    };
    Ok(Json(wire::event_view(event)))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Switch {
    to: String,
}

/// Moves a running or limit-hit agent step to another runtime of its chain,
/// with a hand-off pack, in the same worktree, as the caller.
async fn switch_runtime(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Path((run, key)): Path<(String, String)>,
    Body(input): Body<Switch>,
) -> ApiResult<Json<TaskView>> {
    let run_id: RunId = id(&run, "run")?;
    let step_key = StepKey::new(key.as_str())
        .map_err(|_| ApiError::invalid(format!("`{key}` is not a valid step key.")))?;
    let to = input.to.trim();
    if to.is_empty() || to.len() > 64 {
        return Err(ApiError::invalid("Pick a runtime to switch to."));
    }
    let run = api.engine.run(run_id).await?;
    api.engine
        .switch_runtime(run_id, step_key, to.to_owned(), caller.task_actor())
        .await?;
    summary(&api, run.task_id).await
}

/// Retries a failed step from where it failed: same worktree,
/// succeeded steps kept; a run that ended badly reopens.
async fn retry_step(
    State(api): State<Api>,
    Extension(by): Extension<Caller>,
    Path((run, key)): Path<(String, String)>,
) -> ApiResult<Json<TaskView>> {
    let run_id: RunId = id(&run, "run")?;
    let step_key = StepKey::new(key.as_str())
        .map_err(|_| ApiError::invalid(format!("`{key}` is not a valid step key.")))?;
    let run = api.engine.run(run_id).await?;
    api.engine
        .retry_step(run_id, step_key, by.task_actor())
        .await?;
    summary(&api, run.task_id).await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OutboxQuery {
    status: String,
}

/// Side effects in one state, e.g. `?status=unknown`: those waiting for a
/// human decision.
async fn list_outbox(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Query(query): Query<OutboxQuery>,
) -> ApiResult<Json<Vec<wire::OutboxView>>> {
    let status = OutboxStatus::from_str(&query.status).map_err(|_| {
        ApiError::invalid("`status` must be pending, delivered, unknown or failed.")
    })?;
    let mut views = Vec::new();
    for row in api.engine.outbox(status).await? {
        // A side effect outside any run belongs to no workspace.
        if let Some(run) = row.run_id
            && !workspace_access::visible(&api, &caller, OnWorkspace::Run(run)).await?
        {
            continue;
        }
        views.push(wire::outbox_view(&row));
    }
    Ok(Json(views))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Resolve {
    resolution: Resolution,
}

/// A human says what happened to an `unknown` side effect: `delivered`
/// (record it) or `resend` (send it once more, same idem key). The task's
/// summary, or `204` for a side effect outside any run.
async fn resolve_outbox(
    State(api): State<Api>,
    Extension(by): Extension<Caller>,
    Path(item): Path<String>,
    Body(input): Body<Resolve>,
) -> ApiResult<Response> {
    let item_id: OutboxId = id(&item, "outbox item")?;
    let row = api
        .engine
        .outbox(OutboxStatus::Unknown)
        .await?
        .into_iter()
        .find(|row| row.id == item_id)
        .ok_or_else(|| ApiError::conflict("This side effect is not waiting for a decision."))?;
    let task_id = match row.run_id {
        Some(run) => Some(api.engine.run(run).await?.task_id),
        None => None,
    };
    api.engine
        .resolve_outbox(item_id, input.resolution, by.principal.user)
        .await?;
    match task_id {
        Some(task_id) => Ok(summary(&api, task_id).await?.into_response()),
        None => Ok(StatusCode::NO_CONTENT.into_response()),
    }
}

/// `{on_limit}`: `switch`, `pause_and_notify`, `wait_until_reset`, or
/// `null` to clear.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OnLimitInput {
    on_limit: Option<sun_engine::OnLimit>,
}

/// `PUT /tasks/{id}/on_limit`: this task's limit policy; it beats the
/// workflow's `runtime.on_limit` and the ecosystem default.
async fn put_task_on_limit(
    State(api): State<Api>,
    Path(task): Path<String>,
    Body(input): Body<OnLimitInput>,
) -> ApiResult<Json<Value>> {
    let task: TaskId = id(&task, "task")?;
    api.engine.set_on_limit(Some(task), input.on_limit).await?;
    Ok(Json(json!({ "on_limit": input.on_limit })))
}

/// `GET /tasks/{id}/on_limit`: this task's own limit policy (`null`: the
/// workflow's or the ecosystem's applies).
async fn get_task_on_limit(
    State(api): State<Api>,
    Path(task): Path<String>,
) -> ApiResult<Json<Value>> {
    let task: TaskId = id(&task, "task")?;
    let on_limit = api.engine.on_limit(Some(task)).await?;
    Ok(Json(json!({ "on_limit": on_limit })))
}

/// `GET /settings/on_limit`: the ecosystem's limit policy (`null`: `switch`).
async fn get_on_limit(State(api): State<Api>) -> ApiResult<Json<Value>> {
    let on_limit = api.engine.on_limit(None).await?;
    Ok(Json(json!({ "on_limit": on_limit })))
}

/// `PUT /settings/on_limit` (deployment owner): the ecosystem's limit policy, used when
/// neither the task nor its workflow sets one.
async fn put_on_limit(
    State(api): State<Api>,
    Body(input): Body<OnLimitInput>,
) -> ApiResult<Json<Value>> {
    api.engine.set_on_limit(None, input.on_limit).await?;
    Ok(Json(json!({ "on_limit": input.on_limit })))
}

/// `{monthly_usd}`: the ecosystem's monthly spend cap in USD, `null` = none.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SpendCapInput {
    monthly_usd: Option<f64>,
}

/// `GET /settings/budget`: `{monthly_usd, spent_usd, since}`, this month's
/// spend (from the start of the month, UTC) against the cap.
async fn get_spend_cap(State(api): State<Api>) -> ApiResult<Json<sun_engine::Spend>> {
    Ok(Json(api.engine.spend().await?))
}

/// `PUT /settings/budget` (deployment owner): sets or clears the monthly
/// cap. Reaching it holds new agent runs in the queue and ends running agent
/// steps `budget_exceeded`; raising it lets held runs start.
async fn put_spend_cap(
    State(api): State<Api>,
    Body(input): Body<SpendCapInput>,
) -> ApiResult<Json<sun_engine::Spend>> {
    api.engine.set_spend_cap(input.monthly_usd).await?;
    api.queue.wake();
    Ok(Json(api.engine.spend().await?))
}

/// `{cost_usd}`: a task's own cost budget per run in USD, `null` = none.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskBudgetInput {
    cost_usd: Option<f64>,
}

/// `GET /tasks/{id}/budget`: the task's own budget (`null`: the workflow's
/// `budget.cost_usd` alone applies).
async fn get_task_budget(
    State(api): State<Api>,
    Path(task): Path<String>,
) -> ApiResult<Json<Value>> {
    let task: TaskId = id(&task, "task")?;
    let cost_usd = api.engine.task_budget(task).await?;
    Ok(Json(json!({ "cost_usd": cost_usd })))
}

/// `PUT /tasks/{id}/budget`: the task's cost budget per run; the smaller of
/// it and the workflow's wins, from the task's next agent step.
async fn put_task_budget(
    State(api): State<Api>,
    Path(task): Path<String>,
    Body(input): Body<TaskBudgetInput>,
) -> ApiResult<Json<Value>> {
    let task: TaskId = id(&task, "task")?;
    api.engine.set_task_budget(task, input.cost_usd).await?;
    Ok(Json(json!({ "cost_usd": input.cost_usd })))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AnswerInput {
    answer: String,
}

/// Answers an agent's `sun.ask` question as the caller; its step runs
/// again. `409` when the caller is capped and the session runs in bypass.
async fn answer_question(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Path((run, question)): Path<(String, String)>,
    Body(input): Body<AnswerInput>,
) -> ApiResult<Json<TaskView>> {
    let run_id: RunId = id(&run, "run")?;
    let question_id: NoteId = id(&question, "question")?;
    let answer = input.answer.trim();
    if answer.is_empty() || answer.len() > 4000 {
        return Err(ApiError::invalid(
            "Write an answer (up to 4000 characters).",
        ));
    }
    let run = api.engine.run(run_id).await?;
    api.engine
        .answer(run_id, question_id, answer.to_owned(), caller.task_actor())
        .await
        .map_err(|e| match e {
            Error::Forbidden(why) => ApiError::conflict(why),
            e => e.into(),
        })?;
    summary(&api, run.task_id).await
}

#[derive(Deserialize)]
struct After {
    after: Option<i64>,
    /// Only events of this kind (timeline filter).
    kind: Option<String>,
    /// Only events whose payload contains this text, any case.
    q: Option<String>,
}

/// The run's events after `after`: JSON (one page) or, with
/// `Accept: text/event-stream`, a live SSE stream. `kind` and `q` filter
/// the JSON page (and make the answer JSON).
async fn events(
    State(api): State<Api>,
    Path(run): Path<String>,
    Query(query): Query<After>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let run_id: RunId = id(&run, "run")?;
    api.engine.run(run_id).await?;
    // `Last-Event-ID` (an EventSource reconnect) wins over `?after=`.
    let after = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
        .or(query.after)
        .unwrap_or(0)
        .max(0);
    let kind = query.kind.as_deref().filter(|k| !k.is_empty());
    let text = query.q.as_deref().map(str::trim).filter(|q| !q.is_empty());
    if kind.is_some() || text.is_some() {
        let bad_kind = kind.is_some_and(|k| {
            k.len() > 64 || !k.chars().all(|c| c.is_ascii_lowercase() || c == '_')
        });
        if bad_kind || text.is_some_and(|q| q.chars().count() > 200) {
            return Err(ApiError::invalid(
                "Filter by an event kind like `tool_call` and up to 200 characters of text.",
            ));
        }
        let page = api
            .engine
            .search_events(run_id, kind, text, after, PAGE)
            .await?;
        return Ok(
            Json(page.into_iter().map(wire::event_view).collect::<Vec<_>>()).into_response(),
        );
    }
    let wants_stream = headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("text/event-stream"));
    if wants_stream {
        let feed = sse::RunFeed {
            engine: api.engine.clone(),
            run_id,
        };
        return Ok(sse::stream(feed, api.engine.subscribe(), after).into_response());
    }
    let page = api.engine.events(run_id, after, PAGE).await?;
    Ok(Json(page.into_iter().map(wire::event_view).collect::<Vec<_>>()).into_response())
}

async fn list_workflows(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Query(filter): Query<WorkspaceFilter>,
) -> ApiResult<Json<Vec<wire::WorkflowRef>>> {
    let workflows = match workspace_param(filter.workspace.as_deref())? {
        Some(workspace) => Workflows::of(api.engine.workflows_of_workspace(workspace).await?),
        None => Workflows::load(&api.engine).await?,
    };
    let mut all = workflows.all();
    all.retain(|w| caller.sees_text(&w.workspace_id));
    Ok(Json(all))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SaveWorkflow {
    source_yaml: String,
    /// The workspace a new workflow goes on (default Main); a new version keeps
    /// the workflow's workspace, so this must be absent or that workspace.
    #[serde(default)]
    workspace_id: Option<String>,
}

fn check_size(input: &SaveWorkflow) -> ApiResult<()> {
    if input.source_yaml.len() > MAX_WORKFLOW {
        return Err(ApiError::invalid(
            "The workflow file is longer than 256 KB.",
        ));
    }
    Ok(())
}

/// Saves `input` as a new workflow or the next version of `workflow_id`. A
/// step with `permission_mode: bypass_permissions` also needs
/// `manage_runtimes` on the workflow's workspace and `runtimes.allow_bypass`.
async fn save(
    api: &Api,
    caller: &Caller,
    workflow_id: Option<WorkflowId>,
    input: SaveWorkflow,
) -> ApiResult<Json<Value>> {
    check_size(&input)?;
    let workspace = workspace_param(input.workspace_id.as_deref())?;
    let target = match workflow_id {
        Some(workflow_id) => api.engine.workspace_of_workflow(workflow_id).await?,
        None => workspace.unwrap_or(WorkspaceId::MAIN),
    };
    if workspace.is_some_and(|w| w != target) {
        return Err(ApiError::invalid(
            "A workflow stays on its workspace; save it without `workspace_id`.",
        ));
    }
    if sun_engine::sets_bypass(&input.source_yaml) {
        if !caller.can_in(target, Permission::ManageRuntimes) {
            return Err(auth::denied(Permission::ManageRuntimes));
        }
        crate::hosts::check_bypass_allowed(api, true)?;
    }
    let (workflow, version) = api
        .engine
        .save_workflow_on(target, workflow_id, &input.source_yaml)
        .await?;
    Ok(Json(json!({
        "id": workflow.id,
        "name": workflow.name,
        "workspace_id": workflow.workspace_id,
        "version": version.version,
    })))
}

/// Creates a workflow from YAML (its version 1).
async fn create_workflow(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Body(input): Body<SaveWorkflow>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    Ok((StatusCode::CREATED, save(&api, &caller, None, input).await?))
}

/// Saves the next version of a workflow.
async fn save_workflow_version(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Path(workflow): Path<String>,
    Body(input): Body<SaveWorkflow>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let workflow_id = id(&workflow, "workflow")?;
    Ok((
        StatusCode::CREATED,
        save(&api, &caller, Some(workflow_id), input).await?,
    ))
}

/// A workflow's latest version with its YAML (the builder loads this).
async fn get_workflow(
    State(api): State<Api>,
    Path(workflow): Path<String>,
) -> ApiResult<Json<Value>> {
    let (workflow, version) = api.engine.workflow(id(&workflow, "workflow")?).await?;
    Ok(Json(json!({
        "id": workflow.id,
        "name": workflow.name,
        "workspace_id": workflow.workspace_id,
        "version": version.version,
        "source_yaml": version.source_yaml,
    })))
}

/// Checks a workflow file without saving it: `{errors: [{line, column,
/// message}]}`, empty when valid (the builder's live validation).
async fn validate_workflow(Body(input): Body<SaveWorkflow>) -> ApiResult<Json<Value>> {
    check_size(&input)?;
    let errors: Vec<Value> = sun_engine::check_workflow(&input.source_yaml)
        .into_iter()
        .map(|e| json!({ "line": e.line, "column": e.column, "message": e.message }))
        .collect();
    Ok(Json(json!({ "errors": errors })))
}

/// A dry run: steps in order, mods, runtimes and a cost estimate.
async fn dry_run_workflow(
    State(api): State<Api>,
    Body(input): Body<SaveWorkflow>,
) -> ApiResult<Json<sun_engine::DryRun>> {
    check_size(&input)?;
    Ok(Json(api.engine.dry_run(&input.source_yaml).await?))
}

/// The role templates with the mods each needs.
async fn list_templates() -> Json<Vec<Value>> {
    Json(
        sun_engine::WORKFLOW_TEMPLATES
            .iter()
            .map(|t| {
                json!({
                    "id": t.id,
                    "role": t.role,
                    "title": t.title,
                    "description": t.description,
                    "mods": t.mods(),
                    "source_yaml": t.source_yaml,
                })
            })
            .collect(),
    )
}

/// Saves a role template as a new workflow (version 1) on `?workspace=` (default
/// Main). Its mods are not installed here: that is the catalog's consent flow.
/// 409 when that workspace already has a workflow of that name.
async fn install_template(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Path(template): Path<String>,
    Query(filter): Query<WorkspaceFilter>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let workspace = workspace_param(filter.workspace.as_deref())?.unwrap_or(WorkspaceId::MAIN);
    let template = sun_engine::WORKFLOW_TEMPLATES
        .iter()
        .find(|t| t.id == template)
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "not_found",
                format!("There is no template `{template}`."),
            )
        })?;
    if api
        .engine
        .workflows_of_workspace(workspace)
        .await?
        .iter()
        .any(|(w, _)| w.name == template.id)
    {
        return Err(ApiError::conflict(format!(
            "This workspace already has a workflow named `{}`. Edit it in the builder instead.",
            template.id
        )));
    }
    let input = SaveWorkflow {
        source_yaml: template.source_yaml.to_owned(),
        workspace_id: Some(workspace.to_string()),
    };
    Ok((StatusCode::CREATED, save(&api, &caller, None, input).await?))
}

#[derive(Deserialize)]
struct UsageFilter {
    run_id: Option<String>,
    /// `runtime`: one row per runtime instead of one total.
    by: Option<String>,
}

/// Subscription CLIs (Claude Code, Codex) meter usage in rolling 5-hour windows.
// ponytail: one window for every subscription runtime; per-runtime hours if a CLI differs.
const QUOTA_WINDOW_HOURS: i64 = 5;

/// Summed tokens and cost, of one run or of everything; with
/// `by=runtime`, per runtime, and for subscription runtimes also within the
/// current quota window.
async fn usage(State(api): State<Api>, Query(filter): Query<UsageFilter>) -> ApiResult<Response> {
    match filter.by.as_deref() {
        None => {}
        Some("runtime") => return Ok(Json(usage_by_runtime(&api).await?).into_response()),
        Some(other) => {
            return Err(ApiError::invalid(format!(
                "`by={other}` is not supported; use `by=runtime`."
            )));
        }
    }
    let run_id = filter.run_id.map(|r| id::<RunId>(&r, "run")).transpose()?;
    Ok(Json(api.engine.usage(run_id).await?).into_response())
}

async fn usage_by_runtime(api: &Api) -> ApiResult<Vec<Value>> {
    let since = chrono::Utc::now() - chrono::Duration::hours(QUOTA_WINDOW_HOURS);
    let window = api.engine.usage_by_runtime(Some(since)).await?;
    // ponytail: deployment-wide usage names a runtime by name; a name two
    // workspaces share shows the first one's source.
    let runtimes = api.engine.runtimes(None).await?;
    let rows = api.engine.usage_by_runtime(None).await?;
    Ok(rows
        .into_iter()
        .map(|(runtime, total)| {
            let source = runtimes
                .iter()
                .find(|r| r.name == runtime)
                .map_or(RuntimeSource::ByoRuntime, |r| r.source);
            let in_window = (source == RuntimeSource::ByoRuntime).then(|| {
                let used = window
                    .iter()
                    .find(|(name, _)| *name == runtime)
                    .map(|(_, u)| *u)
                    .unwrap_or_default();
                json!({ "hours": QUOTA_WINDOW_HOURS, "usage": used })
            });
            json!({
                "runtime": runtime,
                "source": source,
                "usage": total,
                "quota_window": in_window,
            })
        })
        .collect())
}

/// The `git check-ref-format` rules that matter for a branch argument.
pub(crate) fn check_branch(branch: &str) -> ApiResult<()> {
    let bad_branch = branch.is_empty()
        || branch.len() > 255
        || branch.starts_with(['-', '/'])
        || branch.contains("..")
        || branch
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || "~^:?*[\\".contains(c));
    match bad_branch {
        true => Err(ApiError::invalid(
            "The default branch is not a valid branch name.",
        )),
        false => Ok(()),
    }
}

/// The task's artifact versions, oldest first.
async fn list_artifacts(
    State(api): State<Api>,
    Path(task): Path<String>,
) -> ApiResult<Json<Vec<Artifact>>> {
    let task_id = id(&task, "task")?;
    api.engine.task(task_id).await?;
    Ok(Json(api.engine.artifacts(task_id).await?))
}

/// Stale and conflicting knowledge sources for the task.
async fn knowledge_flags(
    State(api): State<Api>,
    Path(task): Path<String>,
) -> ApiResult<Json<Vec<sun_context::knowledge::KnowledgeFlag>>> {
    Ok(Json(api.engine.knowledge_flags(id(&task, "task")?).await?))
}

/// A device verify screenshot the run's `device_result` cites.
async fn device_screenshot(
    State(api): State<Api>,
    Path((run, blob)): Path<(String, String)>,
) -> ApiResult<Response> {
    let blob = sun_core::BlobHash::new(blob).map_err(ApiError::from)?;
    let bytes = api
        .engine
        .device_screenshot(id(&run, "run")?, &blob)
        .await?;
    Ok((
        [
            (header::CONTENT_TYPE, "image/png"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            // Content-addressed: the bytes behind a hash never change.
            (
                header::CACHE_CONTROL,
                "private, max-age=31536000, immutable",
            ),
        ],
        bytes,
    )
        .into_response())
}

/// One artifact version's content, as a download.
async fn download_artifact(
    State(api): State<Api>,
    Path(artifact): Path<String>,
) -> ApiResult<Response> {
    let (artifact, bytes) = api.engine.artifact(id(&artifact, "artifact")?).await?;
    // The name comes from a workflow file: keep safe header characters only.
    let name: String = artifact
        .name
        .rsplit('/')
        .next()
        .unwrap_or_default()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || "._-".contains(*c))
        .collect();
    let disposition = format!("attachment; filename=\"v{}-{name}\"", artifact.version);
    Ok((
        [
            (header::CONTENT_TYPE, artifact.mime),
            (header::CONTENT_DISPOSITION, disposition),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff".into()),
        ],
        bytes,
    )
        .into_response())
}
