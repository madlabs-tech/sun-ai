//! Accounts: people sign up with their email (an emailed code, as the
//! deployment's sign-up policy allows); deployment owners add accounts (an
//! emailed code or a password they set), re-invite, set passwords,
//! deactivate and reactivate them; members sign in with their password or
//! the emailed code and set their own password (and email, for members from
//! before invites and the first-run owner); deployment owners configure SMTP.
//! Passwords are hashed here with Argon2id (default params, PHC string) on
//! the blocking pool; codes are 6 random digits stored as
//! `sha256(email ':' code)`. No credential is logged, echoed or returned.

use std::sync::OnceLock;
use std::time::Duration;

use argon2::Argon2;
use argon2::password_hash::phc::PasswordHash;
use argon2::password_hash::{PasswordHasher, PasswordVerifier};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::{Extension, Json};
use serde::Deserialize;
use serde_json::{Value, json};
use sun_core::entitlement::Quota;
use sun_core::model::{Mail, Member, Role, SmtpSettings, SmtpTls, normalize_email};
use sun_core::{MemberId, Timestamp, WorkspaceId};
use sun_engine::FirstSignIn;

use super::members::not_yourself;
use super::{Caller, digest};
use crate::Api;
use crate::error::{ApiError, ApiResult};
use crate::license::within_plan;
use crate::routes::Body;

/// How long an invite code works.
pub(crate) const CODE_TTL: Duration = Duration::from_secs(15 * 60);
/// Shortest and longest password accepted (bytes cap the hashing work).
const MIN_PASSWORD: usize = 12;
const MAX_PASSWORD: usize = 1024;

fn configure_email_first() -> ApiError {
    ApiError::conflict("Configure email first: Settings → Email (SMTP).")
}

fn invalid_password() -> ApiError {
    ApiError::invalid(format!(
        "Use a password of {MIN_PASSWORD} to {MAX_PASSWORD} characters."
    ))
}

/// 422 unless `password` is [`MIN_PASSWORD`] to [`MAX_PASSWORD`] long.
fn check_password(password: &str) -> ApiResult<()> {
    if !(MIN_PASSWORD..=MAX_PASSWORD).contains(&password.chars().count())
        || password.len() > MAX_PASSWORD
    {
        return Err(invalid_password());
    }
    Ok(())
}

/// Argon2id of `password`, on the blocking pool.
async fn hash_password_blocking(password: String) -> ApiResult<String> {
    blocking(move || hash_password(&password)).await?
}

impl Api {
    /// First run (`sun serve`): seeds the owner's member row as
    /// [`sun_core::model::FIRST_ADMIN_EMAIL`] with the kernel token as the
    /// password, or gives an older owner row (no password) the
    /// token as its password; either must change it at first sign-in. The email to sign in with when it did either;
    /// `None` when the owner already has a password.
    /// An owner whose password is still the token (it was printed, maybe
    /// into logs) is made to change it too (`None`: nothing new to print).
    pub async fn seed_owner(&self) -> sun_core::Result<Option<String>> {
        let unavailable = |_| sun_core::Error::Unavailable("password hashing failed".into());
        let user = &self.config.principal.user;
        if let Some(owner) = self.engine.owner_member(user).await?
            && !owner.must_set_password
            && let Some(old) = self.engine.password_hash(owner.id).await?
        {
            let token = self.token();
            if blocking(move || verify_password(&token, &old))
                .await
                .map_err(unavailable)?
            {
                self.engine
                    .force_password_change(
                        owner.id,
                        &format!("user:{user}"),
                        "must replace the kernel token as password",
                    )
                    .await?;
            }
            return Ok(None);
        }
        let hash = hash_password_blocking(self.token())
            .await
            .map_err(unavailable)?;
        self.engine.seed_owner(user, &hash).await
    }
}

fn hash_password(password: &str) -> ApiResult<String> {
    Argon2::default()
        .hash_password(password.as_bytes())
        .map(|h| h.to_string())
        .map_err(|_| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                "Password hashing failed.",
            )
        })
}

fn verify_password(password: &str, hash: &str) -> bool {
    PasswordHash::new(hash).is_ok_and(|parsed| {
        Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok()
    })
}

/// A real Argon2id hash of nothing anyone knows: unknown emails are
/// verified against it, so they take as long as known ones.
fn dummy_hash() -> &'static str {
    static DUMMY: OnceLock<String> = OnceLock::new();
    DUMMY.get_or_init(|| hash_password("not a password of anyone").unwrap_or_default())
}

/// Runs Argon2 off the async workers.
async fn blocking<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> ApiResult<T> {
    tokio::task::spawn_blocking(work).await.map_err(|_| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            "Hashing stopped.",
        )
    })
}

/// `sha256(email ':' code)`: what the store keeps of an invite code.
pub(super) fn code_hash(email: &str, code: &str) -> String {
    digest(&format!("{email}:{code}"))
}

/// Six random digits from the OS RNG, uniform (rejection sampling).
pub(super) fn new_code() -> ApiResult<String> {
    loop {
        let mut bytes = [0u8; 4];
        getrandom::fill(&mut bytes).map_err(|_| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                "No randomness.",
            )
        })?;
        let n = u32::from_le_bytes(bytes);
        // 4_294_000_000 is the largest multiple of 1e6 below 2^32.
        if n < 4_294_000_000 {
            return Ok(format!("{:06}", n % 1_000_000));
        }
    }
}

/// The member whose password this is, or `None`; every path takes about as
/// long (an unknown email is checked against [`dummy_hash`]).
pub(super) async fn password_sign_in(
    api: &Api,
    email: &str,
    password: &str,
) -> ApiResult<Option<Member>> {
    let member = match normalize_email(email) {
        Ok(email) => api.engine.member_by_email(&email).await?,
        Err(_) => None,
    };
    let hash = match &member {
        Some(m) => api.engine.password_hash(m.id).await?,
        None => None,
    };
    let known = hash.is_some();
    let hash = hash.unwrap_or_else(|| dummy_hash().to_owned());
    let password = password.to_owned();
    let ok = password.len() <= MAX_PASSWORD
        && blocking(move || verify_password(&password, &hash)).await?
        && known;
    Ok(member.filter(|m| ok && m.deactivated_at.is_none()))
}

/// Signs in with an emailed code (an invite or a reset); `false` for any
/// failure.
pub(super) async fn code_sign_in(
    api: &Api,
    email: &str,
    code: &str,
    session_hash: &str,
    expires_at: Timestamp,
) -> ApiResult<bool> {
    let Ok(email) = normalize_email(email) else {
        return Ok(false);
    };
    let code = code.trim();
    if code.len() != 6 || !code.bytes().all(|b| b.is_ascii_digit()) {
        return Ok(false);
    }
    let Some(member) = api.engine.member_by_email(&email).await? else {
        return Ok(false);
    };
    if member.deactivated_at.is_some() {
        return Ok(false);
    }
    let signed_in = api
        .engine
        .sign_in_with_code(
            member.id,
            &code_hash(&email, code),
            session_hash,
            expires_at,
        )
        .await?;
    Ok(signed_in.is_some())
}

/// The caller's email: the member's, or the owner row's for the token holder.
pub(super) async fn email_of(api: &Api, caller: &Caller) -> ApiResult<Option<String>> {
    if caller.member_id.is_some() {
        return Ok(caller.email.clone());
    }
    let owner = api.engine.owner_member(&caller.principal.user).await?;
    Ok(owner.and_then(|m| m.email))
}

/// Sends `mail` through the saved SMTP settings: 409 until they're saved,
/// 503 without a mailer, 502 when the server refused it.
pub(super) async fn send(api: &Api, mail: &Mail) -> ApiResult<()> {
    let (Some(smtp), _) = api.engine.smtp_settings().await? else {
        return Err(configure_email_first());
    };
    let Some(mailer) = api.mailer.as_ref() else {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "unavailable",
            "This kernel can't send mail.",
        ));
    };
    let password = api.engine.smtp_password().await?;
    mailer
        .send(&smtp, password.as_deref(), mail)
        .await
        .map_err(|e| {
            let known: Vec<&str> = password.as_deref().into_iter().collect();
            let why = sun_core::redact::redact(&e.to_string(), &known);
            ApiError::new(
                StatusCode::BAD_GATEWAY,
                "mail_failed",
                format!("The mail server refused the mail: {why}"),
            )
        })
}

/// The invite mail for `member` with `code`.
fn invite_mail(api: &Api, member: &Member, email: &str, code: &str) -> Mail {
    let link = match &api.mail_url {
        Some(url) => format!("{url}/"),
        None => "Sun's sign-in page".to_owned(),
    };
    Mail {
        to: email.to_owned(),
        subject: "Your Sun sign-in code".into(),
        body: format!(
            "Hi {},\n\nYour one-time Sun sign-in code is\n\n    {code}\n\n\
             It works once, for 15 minutes. Sign in at\n{link}\n\
             with your email, type the code in the password box, and then choose\n\
             a password.\n\nIf you didn't expect this, ignore this mail.\n",
            member.username
        ),
    }
}

pub(super) fn parse_id(id: &str) -> ApiResult<MemberId> {
    id.parse()
        .map_err(|_| ApiError::invalid(format!("`{id}` is not a valid member id.")))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Invite {
    email: String,
    username: String,
    /// The workspace they join (default Main).
    #[serde(default = "main")]
    workspace_id: WorkspaceId,
    /// Their role there (default viewer).
    #[serde(default = "viewer")]
    role: Role,
    #[serde(default)]
    is_owner: bool,
    /// Set their password instead of mailing a code.
    password: Option<String>,
    /// With `password`: they must change it at their first sign-in.
    #[serde(default = "yes")]
    must_change: bool,
}

fn yes() -> bool {
    true
}

fn main() -> WorkspaceId {
    WorkspaceId::MAIN
}

fn viewer() -> Role {
    Role::Viewer
}

/// `POST /api/v1/members {email, username, workspace_id?, role?, is_owner?,
/// password?, must_change?}` (deployment owner): adds an account straight
/// into the workspace (default Main) as `role` (default viewer) and emails
/// them a sign-in code, or, with `password`, sets it (no mail;
/// `must_change`, default true, forces a change at their first sign-in).
/// Workspace admins invite with `POST /workspaces/{id}/invitations`
/// instead. Without a password, 409 until SMTP is configured; a plan
/// without a free seat is 403.
pub(crate) async fn invite_member(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Body(input): Body<Invite>,
) -> ApiResult<(StatusCode, Json<Member>)> {
    let email = normalize_email(&input.email)?;
    let username = input.username.trim();
    let valid_name = (1..=64).contains(&username.len())
        && username
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c));
    if !valid_name {
        return Err(ApiError::invalid(
            "Use a username of 1-64 letters, digits, - _ or .",
        ));
    }
    let members = api.engine.members().await?;
    within_plan(&api, Quota::Seats, "seats", members.len())?;
    if let Some(password) = input.password {
        check_password(&password)?;
        let hash = hash_password_blocking(password).await?;
        let first = FirstSignIn::Password {
            hash: &hash,
            must_change: input.must_change,
        };
        let member = api
            .engine
            .invite_member(
                &email,
                username,
                input.is_owner,
                input.workspace_id,
                input.role,
                first,
                &caller.actor(),
            )
            .await?;
        return Ok((StatusCode::CREATED, Json(member)));
    }
    if api.engine.smtp_settings().await?.0.is_none() {
        return Err(configure_email_first());
    }
    let code = new_code()?;
    let first = FirstSignIn::Code {
        hash: &code_hash(&email, &code),
        expires_at: chrono::Utc::now() + CODE_TTL,
    };
    let member = api
        .engine
        .invite_member(
            &email,
            username,
            input.is_owner,
            input.workspace_id,
            input.role,
            first,
            &caller.actor(),
        )
        .await?;
    send(&api, &invite_mail(&api, &member, &email, &code)).await?;
    Ok((StatusCode::CREATED, Json(member)))
}

/// `POST /api/v1/members/{id}/invite` (deployment owner): a new code for a
/// lost invite or a forgotten password. Clears the password and ends their
/// sessions. Not on yourself (`not_yourself`).
pub(crate) async fn reinvite_member(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
) -> ApiResult<Json<Member>> {
    let id = parse_id(&id)?;
    not_yourself(&caller, id)?;
    let member = api.engine.member(id).await?;
    let Some(email) = member.email.clone() else {
        return Err(ApiError::invalid(format!(
            "{} has no email yet: they add it on their next sign-in.",
            member.username
        )));
    };
    if api.engine.smtp_settings().await?.0.is_none() {
        return Err(configure_email_first());
    }
    let code = new_code()?;
    let expires_at = chrono::Utc::now() + CODE_TTL;
    let member = api
        .engine
        .reinvite_member(id, &code_hash(&email, &code), expires_at, &caller.actor())
        .await?;
    send(&api, &invite_mail(&api, &member, &email, &code)).await?;
    Ok(Json(member))
}

/// `POST /api/v1/members/{id}/deactivate` (deployment owner): they can't
/// sign in until reactivated, and their sessions end. Nobody deactivates
/// themselves or the last active owner.
pub(crate) async fn deactivate_member(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
) -> ApiResult<Json<Member>> {
    set_active(&api, &caller, &id, false).await
}

/// `POST /api/v1/members/{id}/activate` (deployment owner): reactivates them.
pub(crate) async fn activate_member(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
) -> ApiResult<Json<Member>> {
    set_active(&api, &caller, &id, true).await
}

async fn set_active(api: &Api, caller: &Caller, id: &str, active: bool) -> ApiResult<Json<Member>> {
    let id = parse_id(id)?;
    if !active && caller.member_id == Some(id) {
        return Err(ApiError::invalid("You can't deactivate yourself."));
    }
    let member = api
        .engine
        .set_member_active(id, active, &caller.actor())
        .await?;
    Ok(Json(member))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AdminPassword {
    password: String,
    #[serde(default = "yes")]
    must_change: bool,
}

/// `PUT /api/v1/members/{id}/password {password, must_change?}`
/// (deployment owner): sets the member's password (`must_change`, default
/// true, forces a change at their next sign-in) and ends their sessions.
/// The hash never comes back. Not on yourself (`not_yourself`).
pub(crate) async fn set_member_password(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
    Body(input): Body<AdminPassword>,
) -> ApiResult<Json<Member>> {
    let id = parse_id(&id)?;
    not_yourself(&caller, id)?;
    check_password(&input.password)?;
    let hash = hash_password_blocking(input.password).await?;
    let member = api
        .engine
        .set_member_password(id, &hash, input.must_change, &caller.actor())
        .await?;
    Ok(Json(member))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SetPassword {
    password: String,
    /// Needed to change a password the caller already has (not right after
    /// an invite-code sign-in).
    current_password: Option<String>,
    /// The answer to `POST /me/two_step/challenge`, when the policy asks for
    /// a second step to change a password.
    two_step: Option<super::two_step::Answer>,
}

/// `PUT /api/v1/me/password {password, current_password?, two_step?}`: the
/// member's own password (12+ characters). Lifts the after-invite gate.
/// With the `two_step_for_password_change` policy, a member with a second
/// step must pass it too (403 `two_step_required` without `two_step`).
/// Wrong `current_password` and `two_step` tries are rate-limited together
/// per member and IP (429), across challenges, until a change is saved.
/// Replacing a password that was the kernel token (the first-run owner)
/// rotates the token.
pub(crate) async fn set_my_password(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    connect: Option<Extension<axum::extract::ConnectInfo<std::net::SocketAddr>>>,
    Body(input): Body<SetPassword>,
) -> ApiResult<StatusCode> {
    let Some(id) = caller.member_id else {
        return Err(ApiError::invalid(
            "The kernel token holder has no password: use `sun user set-password <email>`.",
        ));
    };
    check_password(&input.password)?;
    let old_hash = api.engine.password_hash(id).await?;
    // Wrong current passwords and 2-step answers count per member and IP,
    // like sign-in tries; only a saved change clears them.
    let (key, ip) = (id.to_string(), super::client_ip(connect));
    if !caller.must_set_password
        && let Some(hash) = old_hash.clone()
    {
        api.auth.current_password.reserve(&key, &ip)?;
        let current = input.current_password.unwrap_or_default();
        if current.len() > MAX_PASSWORD
            || !blocking(move || verify_password(&current, &hash)).await?
        {
            return Err(ApiError::new(
                StatusCode::FORBIDDEN,
                "forbidden",
                "Your current password is not right.",
            ));
        }
        api.auth.current_password.release(&key, &ip);
    }
    if !caller.must_set_password {
        super::two_step::check_password_change(&api, id, input.two_step.as_ref(), &ip).await?;
    }
    let password = input.password;
    let hash = blocking(move || hash_password(&password)).await??;
    // Rotate first: if that fails nothing changed. If saving fails after it,
    // they still must set a password and can retry.
    if let (Some(old), Some(rotate)) = (old_hash, api.rotate_token.clone()) {
        let token = api.token();
        if blocking(move || verify_password(&token, &old)).await? {
            rotate_token(&api, &caller, rotate).await?;
        }
    }
    api.engine.set_own_password(id, &hash).await?;
    api.auth.current_password.succeed(&key, &ip);
    Ok(StatusCode::NO_CONTENT)
}

/// Writes a new kernel token through `rotate` and swaps it in, so the old
/// one (printed at first run) stops working; audited.
async fn rotate_token(api: &Api, caller: &Caller, rotate: crate::TokenRotation) -> ApiResult<()> {
    let fresh = blocking(move || rotate()).await?.map_err(|why| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            format!("Nothing changed: the kernel token couldn't be rotated: {why}"),
        )
    })?;
    *api.token
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = fresh;
    api.engine
        .audit(
            &caller.actor(),
            sun_core::model::AuditAction::SettingsChange,
            "kernel_token",
            "rotated: the first-run password was replaced",
        )
        .await?;
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SetEmail {
    email: String,
}

/// `PUT /api/v1/me/email {email}`: the token holder's email (creates the
/// owner's member row if missing), the first-run owner's real email in place
/// of the placeholder, or the email of a pre-invite member that had
/// none. A member's real email is changed by no one here.
pub(crate) async fn set_my_email(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Body(input): Body<SetEmail>,
) -> ApiResult<Json<Value>> {
    let email = normalize_email(&input.email)?;
    match caller.member_id {
        None => {
            api.engine
                .set_owner_email(&caller.principal.user, &email)
                .await?;
        }
        Some(id) if caller.needs_email => {
            api.engine.set_own_email(id, &email).await?;
        }
        Some(_) => {
            return Err(ApiError::new(
                StatusCode::FORBIDDEN,
                "forbidden",
                "Ask an admin to change your email.",
            ));
        }
    }
    Ok(Json(json!({ "email": email })))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Signup {
    email: String,
}

/// `POST /api/v1/signup {email}` (open): a new account, signed in with the
/// emailed code (`POST /session {email, password: code}`), which verifies
/// the email and asks for a password. 202 once the code is mailed. As the
/// deployment allows: 403 when sign-up is closed (or limited to other email
/// domains) and no pending invitation names the email; 409 when an account
/// with this email exists (sign in instead); 503 when the deployment can't
/// send mail. Tries are rate-limited per email (429).
pub(crate) async fn signup(
    State(api): State<Api>,
    Body(input): Body<Signup>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let email = normalize_email(&input.email)?;
    // ponytail: per email only (one inbox can't be flooded); a per-IP cap
    // would hit everyone behind one proxy. Add one keyed on a trusted
    // forwarded-for header if sign-ups get scripted.
    api.auth.signup.reserve(&email, "")?;
    if api.engine.smtp_settings().await?.0.is_none() || api.mailer.is_none() {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "unavailable",
            "This deployment can't send email yet, so nobody can sign up. Ask its owner.",
        ));
    }
    let code = new_code()?;
    let expires_at = chrono::Utc::now() + CODE_TTL;
    let Some(member) = api
        .engine
        .signup(&email, &api.signup, &code_hash(&email, &code), expires_at)
        .await?
    else {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "account_exists",
            "An account with this email exists. Sign in instead.",
        ));
    };
    send(&api, &invite_mail(&api, &member, &email, &code)).await?;
    Ok((StatusCode::ACCEPTED, Json(json!({ "email": email }))))
}

/// `GET /api/v1/smtp` (deployment owner): `{smtp: settings|null, has_password}`; the
/// password itself never comes back.
pub(crate) async fn get_smtp(State(api): State<Api>) -> ApiResult<Json<Value>> {
    let (smtp, has_password) = api.engine.smtp_settings().await?;
    Ok(Json(json!({ "smtp": smtp, "has_password": has_password })))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SmtpInput {
    host: String,
    port: u16,
    #[serde(default)]
    username: String,
    from: String,
    tls: SmtpTls,
    /// `None` keeps the stored password, `""` clears it.
    password: Option<String>,
}

/// `PUT /api/v1/smtp {host, port, username, from, tls, password?}`
/// (deployment owner):
/// saves the settings; the password is sealed as an ecosystem secret.
pub(crate) async fn put_smtp(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Body(input): Body<SmtpInput>,
) -> ApiResult<Json<Value>> {
    let from = normalize_email(&input.from)
        .map_err(|_| ApiError::invalid("The From address is not an email."))?;
    let settings = SmtpSettings {
        host: input.host.trim().to_ascii_lowercase(),
        port: input.port,
        username: input.username.trim().to_owned(),
        from,
        tls: input.tls,
    };
    api.engine
        .set_smtp_settings(&settings, input.password.as_deref(), &caller.actor())
        .await?;
    get_smtp(State(api)).await
}

/// `POST /api/v1/smtp/test` (deployment owner): mails the caller through the saved
/// settings. 422 when the caller has no email.
pub(crate) async fn test_smtp(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
) -> ApiResult<Json<Value>> {
    let Some(to) = email_of(&api, &caller).await? else {
        return Err(ApiError::invalid(
            "Add your email first, then send the test.",
        ));
    };
    let mail = Mail {
        to: to.clone(),
        subject: "Sun test mail".into(),
        body: "Sun can send mail: invites will reach your members.\n".into(),
    };
    send(&api, &mail).await?;
    Ok(Json(json!({ "sent_to": to })))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn auth_codes_are_six_digits_and_passwords_verify() {
        for _ in 0..50 {
            let code = new_code().unwrap();
            assert!(
                code.len() == 6 && code.bytes().all(|b| b.is_ascii_digit()),
                "{code}"
            );
        }
        let hash = hash_password("correct horse battery").unwrap();
        assert!(hash.starts_with("$argon2id$"), "{hash}");
        assert!(verify_password("correct horse battery", &hash));
        assert!(!verify_password("correct horse batterY", &hash));
        assert!(!verify_password("anything", "not a phc string"));
        assert!(!dummy_hash().is_empty());
        assert_eq!(code_hash("a@x.io", "123456"), digest("a@x.io:123456"));
    }
}
