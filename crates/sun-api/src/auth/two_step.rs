//! 2-step sign-in: after a right password, a member with a second step
//! (or anyone, when the `require_two_step` policy is on and mail works)
//! gets a 5-minute challenge instead of a session, and answers it with a
//! code from their authenticator (RFC 6238: SHA-1, 30 s, 6 digits, ±1 step,
//! each step once), a code mailed to them, or a passkey ([`super::passkey`]).
//! Members add and remove their
//! own second steps; admins reset someone's (a lost phone); the ecosystem's
//! [`SecurityPolicy`] can require a second step, and ask for it to change a
//! password. An emailed-code sign-in (invite or reset) and SSO skip it.

use axum::extract::{ConnectInfo, Path, State};
use axum::http::StatusCode;
use axum::http::header::SET_COOKIE;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use base64::Engine as _;
use hmac::{Hmac, Mac};
use passkey_auth::{AuthenticationState, RegistrationState};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha1::Sha1;
use std::net::SocketAddr;
use std::time::Duration;
use sun_core::model::{Factor, FactorKind, Mail, Member, SecurityPolicy};
use sun_core::{FactorId, MemberId};

use super::members::not_yourself;
use super::passkey;
use super::users::{code_hash, new_code, parse_id, send};
use super::{Caller, SESSION_TTL, digest, same, session_cookie, wrong_credentials};
use crate::Api;
use crate::error::{ApiError, ApiResult};
use crate::hosts::random_hex;
use crate::routes::Body;

/// How long a sign-in challenge (and the code mailed for it) works.
pub(super) const CHALLENGE_TTL: Duration = Duration::from_secs(5 * 60);
/// RFC 6238 time step.
const STEP_SECS: i64 = 30;
/// Bytes of a new authenticator secret (160 bits, as RFC 4226 advises).
const SECRET_BYTES: usize = 20;

/// The RFC 4226 dynamic truncation of HMAC-SHA1(`secret`, `step`): a 31-bit
/// number whose last 6 digits are the code.
fn hotp(secret: &[u8], step: u64) -> u32 {
    let Ok(mut mac) = Hmac::<Sha1>::new_from_slice(secret) else {
        return u32::MAX; // HMAC takes keys of any length.
    };
    mac.update(&step.to_be_bytes());
    let hash = mac.finalize().into_bytes();
    let at = usize::from(hash[19] & 0x0f);
    u32::from_be_bytes([hash[at] & 0x7f, hash[at + 1], hash[at + 2], hash[at + 3]])
}

/// The current RFC 6238 time step.
fn current_step() -> i64 {
    chrono::Utc::now().timestamp() / STEP_SECS
}

/// The step in `now - 1 ..= now + 1` whose code is `code`, if any.
fn matching_step(secret: &[u8], code: u32, now: i64) -> Option<i64> {
    (now - 1..=now + 1).find(|&step| {
        u64::try_from(step).is_ok_and(|s| {
            same(
                (hotp(secret, s) % 1_000_000).to_be_bytes().as_slice(),
                code.to_be_bytes().as_slice(),
            )
        })
    })
}

/// RFC 4648 base32 without padding: how authenticator apps take a key.
fn base32(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    let mut out = String::new();
    for chunk in bytes.chunks(5) {
        let mut buf = [0u8; 5];
        buf[..chunk.len()].copy_from_slice(chunk);
        let bits = buf.iter().fold(0u64, |acc, b| (acc << 8) | u64::from(*b));
        for i in 0..(chunk.len() * 8).div_ceil(5) {
            out.push(char::from(ALPHABET[((bits >> (35 - i * 5)) & 31) as usize]));
        }
    }
    out
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(text: &str) -> Option<Vec<u8>> {
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok())
        .collect()
}

/// A 6-digit code as a number; `None` for anything else.
fn six_digits(code: &str) -> Option<u32> {
    let code = code.trim();
    (code.len() == 6 && code.bytes().all(|b| b.is_ascii_digit()))
        .then(|| code.parse().ok())
        .flatten()
}

/// What a challenge row's `data` holds, as JSON: the mailed code's hash and
/// the passkey ceremony under way (signing in, or adding a passkey). Each
/// method keeps its own field, so switching method loses nothing.
#[derive(Default, Serialize, Deserialize)]
pub(super) struct ChallengeData {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub(super) email: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) passkey: Option<AuthenticationState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) register: Option<RegistrationState>,
}

impl ChallengeData {
    /// `raw` read back; empty when it is '' (or not this JSON).
    pub(super) fn parse(raw: &str) -> Self {
        serde_json::from_str(raw).unwrap_or_default()
    }

    pub(super) fn text(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }
}

/// A member's name for a second step, `default` when blank; 422 when too
/// long or with control characters.
pub(super) fn label<'a>(input: &'a str, default: &'a str) -> ApiResult<&'a str> {
    match input.trim() {
        "" => Ok(default),
        label if label.chars().count() <= 64 && !label.chars().any(char::is_control) => Ok(label),
        _ => Err(ApiError::invalid("Use a name of up to 64 characters.")),
    }
}

fn internal(what: &str) -> ApiError {
    ApiError::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        "internal",
        what.to_owned(),
    )
}

/// Checks `code` against the member's confirmed authenticators and uses
/// its time step; `false` when wrong or replayed.
async fn check_totp(api: &Api, member: MemberId, code: &str) -> ApiResult<bool> {
    let Some(code) = six_digits(code) else {
        return Ok(false);
    };
    let now = current_step();
    for (id, secret, last_step) in api.engine.totp_secrets(member).await? {
        let Some(secret) = last_step.and(unhex(&secret)) else {
            continue;
        };
        if let Some(step) = matching_step(&secret, code, now) {
            return Ok(api.engine.use_totp_step(id, step).await?);
        }
    }
    Ok(false)
}

/// The second steps `member` can answer a challenge with: theirs, or the
/// emailed code when the policy requires a second step they don't have
/// yet (and mail works). Empty: no challenge.
async fn methods(api: &Api, member: &Member) -> ApiResult<Vec<FactorKind>> {
    let mut kinds: Vec<FactorKind> = Vec::new();
    for factor in api.engine.factors(member.id).await? {
        if !kinds.contains(&factor.kind) {
            kinds.push(factor.kind);
        }
    }
    if kinds.is_empty()
        && member.email.is_some()
        && api.engine.security_policy().await?.require_two_step
        && api.engine.smtp_settings().await?.0.is_some()
    {
        kinds.push(FactorKind::Email);
    }
    Ok(kinds)
}

/// Starts a challenge of `member` answerable by `methods`:
/// `{challenge, methods}`.
async fn challenge(api: &Api, member: MemberId, methods: &[FactorKind]) -> ApiResult<Value> {
    let challenge = random_hex()?;
    let expires_at = chrono::Utc::now() + CHALLENGE_TTL;
    api.engine
        .start_two_step(&digest(&challenge), member, expires_at, "")
        .await?;
    Ok(json!({ "challenge": challenge, "methods": methods }))
}

/// After a right password: `Some({two_step:{challenge, methods}})` when
/// `member` must also pass a second step, `None` to sign in now.
pub(super) async fn after_password(api: &Api, member: &Member) -> ApiResult<Option<Value>> {
    let methods = methods(api, member).await?;
    if methods.is_empty() {
        return Ok(None);
    }
    let challenge = challenge(api, member.id, &methods).await?;
    Ok(Some(json!({ "two_step": challenge })))
}

/// The answer to a challenge: `{challenge, method, code}`, or for a passkey
/// `{challenge, method, credential}`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Answer {
    challenge: String,
    method: String,
    #[serde(default)]
    code: String,
    /// A passkey's answer: the browser's `PublicKeyCredential.toJSON()`.
    #[serde(default)]
    credential: Option<Value>,
}

/// Checks `answer` against the live challenge of `member` (when given; the
/// challenge's member otherwise). The member on success; a wrong code counts
/// a try against the challenge. `None` for anything wrong.
async fn check(api: &Api, answer: &Answer, member: Option<MemberId>) -> ApiResult<Option<Member>> {
    let hash = digest(&answer.challenge);
    let Some((id, data)) = api.engine.two_step(&hash).await? else {
        return Ok(None);
    };
    let data = ChallengeData::parse(&data);
    if member.is_some_and(|m| m != id) {
        return Ok(None);
    }
    let member = api.engine.member(id).await?;
    let Ok(method) = answer.method.parse::<FactorKind>() else {
        return Ok(None);
    };
    if member.deactivated_at.is_some() || !methods(api, &member).await?.contains(&method) {
        return Ok(None);
    }
    let right = match method {
        FactorKind::Totp => check_totp(api, id, &answer.code).await?,
        FactorKind::Email => match (&member.email, six_digits(&answer.code)) {
            (Some(email), Some(_)) if !data.email.is_empty() => same(
                code_hash(email, answer.code.trim()).as_bytes(),
                data.email.as_bytes(),
            ),
            _ => false,
        },
        FactorKind::Passkey => {
            passkey::verify(api, id, data.passkey.as_ref(), answer.credential.as_ref()).await?
        }
    };
    if !right {
        api.engine.fail_two_step(&hash).await?;
        return Ok(None);
    }
    Ok(Some(member))
}

/// `POST /api/v1/session/two_step {challenge, method, code | credential}`:
/// answers the challenge `POST /session` gave and sets the session cookie.
/// Every failure is the same 401, rate-limited with the password tries.
/// `{challenge, method: "passkey"}` alone first gets `200 {options}` to sign
/// with the passkey (also for `PUT /me/password`'s challenge).
pub(crate) async fn sign_in(
    State(api): State<Api>,
    connect: Option<Extension<ConnectInfo<SocketAddr>>>,
    Body(answer): Body<Answer>,
) -> ApiResult<Response> {
    let ip = super::client_ip(connect);
    let hash = digest(&answer.challenge);
    let Some((id, data)) = api.engine.two_step(&hash).await? else {
        return Err(wrong_credentials());
    };
    let member = api.engine.member(id).await?;
    if answer.method == FactorKind::Passkey.as_str() && answer.credential.is_none() {
        if member.deactivated_at.is_some()
            || !methods(&api, &member).await?.contains(&FactorKind::Passkey)
        {
            return Err(wrong_credentials());
        }
        let options = passkey::options(&api, &member, &hash, &data).await?;
        return options
            .map(|options| Json(options).into_response())
            .ok_or_else(wrong_credentials);
    }
    // Limited per the challenge's member, like their password tries.
    let email = member.email.unwrap_or_default();
    api.auth.limiter.reserve(&email, &ip)?;
    let Some(member) = check(&api, &answer, None).await? else {
        return Err(wrong_credentials());
    };
    api.auth.limiter.succeed(&email, &ip);
    let session = random_hex()?;
    let method = answer.method.parse::<FactorKind>()?;
    let started = api
        .engine
        .finish_two_step(
            &hash,
            &member,
            method,
            &digest(&session),
            chrono::Utc::now() + SESSION_TTL,
        )
        .await?;
    if !started {
        return Err(wrong_credentials());
    }
    Ok((
        StatusCode::NO_CONTENT,
        [(SET_COOKIE, session_cookie(&session))],
    )
        .into_response())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MailCode {
    challenge: String,
}

/// `POST /api/v1/session/two_step/email {challenge}`: mails the member a
/// 6-digit code for the challenge (each mail counts as one of its tries).
/// 401 for an unknown challenge or one that can't use email.
pub(crate) async fn mail_code(
    State(api): State<Api>,
    Body(input): Body<MailCode>,
) -> ApiResult<StatusCode> {
    let hash = digest(&input.challenge);
    let Some((id, data)) = api.engine.two_step(&hash).await? else {
        return Err(wrong_credentials());
    };
    let member = api.engine.member(id).await?;
    let Some(email) = member.email.clone() else {
        return Err(wrong_credentials());
    };
    if !methods(&api, &member).await?.contains(&FactorKind::Email) {
        return Err(wrong_credentials());
    }
    let code = new_code()?;
    let mut data = ChallengeData::parse(&data);
    data.email = code_hash(&email, &code);
    api.engine
        .set_two_step_data(&hash, &data.text(), true)
        .await?;
    let mail = Mail {
        to: email,
        subject: "Your Sun sign-in code".into(),
        body: format!(
            "Hi {},\n\nYour Sun verification code is\n\n    {code}\n\n\
             It works once, for 5 minutes. If you didn't just sign in or change\n\
             your password, change it now: someone knows it.\n",
            member.username
        ),
    };
    send(&api, &mail).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// The signed-in member, or 422 for the kernel token holder.
pub(super) fn member_of(caller: &Caller) -> ApiResult<MemberId> {
    caller
        .member_id
        .ok_or_else(|| ApiError::invalid("The kernel token holder signs in with no second step."))
}

/// `GET /api/v1/me/two_step`: `{factors, policy, email_ready,
/// passkey_ready}` — the caller's second steps, the ecosystem rules, whether
/// mail works, and whether passkeys can (Sun on https or localhost).
pub(crate) async fn mine(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
) -> ApiResult<Json<Value>> {
    let factors = match caller.member_id {
        Some(id) => api.engine.factors(id).await?,
        None => Vec::new(),
    };
    Ok(Json(json!({
        "factors": factors,
        "policy": api.engine.security_policy().await?,
        "email_ready": api.engine.smtp_settings().await?.0.is_some(),
        "passkey_ready": passkey::relying_party(&api.config.public_url).is_some(),
    })))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StartTotp {
    #[serde(default)]
    label: String,
}

/// `POST /api/v1/me/two_step/totp {label?}`: starts adding an authenticator.
/// `{id, secret, otpauth_url, qr_svg}`: the key (base32, to type in), the
/// `otpauth://` link and its QR code as an SVG data URL. It counts once
/// `/totp/confirm` saw one of its codes.
pub(crate) async fn start_totp(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Body(input): Body<StartTotp>,
) -> ApiResult<Json<Value>> {
    let member = api.engine.member(member_of(&caller)?).await?;
    let label = label(&input.label, "Authenticator")?;
    let mut secret = [0u8; SECRET_BYTES];
    getrandom::fill(&mut secret).map_err(|_| internal("No randomness."))?;
    let factor = api
        .engine
        .start_totp(member.id, label, &hex(&secret))
        .await?;
    let key = base32(&secret);
    let account = member.email.unwrap_or(member.username);
    let escape = |text: &str| -> String {
        text.bytes()
            .map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                    char::from(b).to_string()
                }
                _ => format!("%{b:02X}"),
            })
            .collect()
    };
    let url = format!(
        "otpauth://totp/Sun:{}?secret={key}&issuer=Sun&algorithm=SHA1&digits=6&period=30",
        escape(&account)
    );
    let svg = qrcode::QrCode::new(url.as_bytes())
        .map_err(|_| internal("The QR code didn't render."))?
        .render::<qrcode::render::svg::Color>()
        .min_dimensions(200, 200)
        .build();
    let qr = base64::engine::general_purpose::STANDARD.encode(svg);
    Ok(Json(json!({
        "id": factor.id,
        "secret": key,
        "otpauth_url": url,
        "qr_svg": format!("data:image/svg+xml;base64,{qr}"),
    })))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ConfirmTotp {
    id: String,
    code: String,
}

/// `POST /api/v1/me/two_step/totp/confirm {id, code}`: a code from the new
/// authenticator turns it on (the factor); 422 when the code is wrong.
pub(crate) async fn confirm_totp(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Body(input): Body<ConfirmTotp>,
) -> ApiResult<Json<Factor>> {
    let member = member_of(&caller)?;
    let id: FactorId = input
        .id
        .parse()
        .map_err(|_| ApiError::invalid("That is not an authenticator being added."))?;
    let wrong = || ApiError::invalid("That code is not right. Type the one your app shows now.");
    let pending = api
        .engine
        .totp_secrets(member)
        .await?
        .into_iter()
        .find(|(f, _, last)| *f == id && last.is_none());
    let Some((_, secret, _)) = pending else {
        return Err(ApiError::invalid(
            "That authenticator isn't being added. Start again.",
        ));
    };
    let secret = unhex(&secret).ok_or_else(|| internal("A stored key is broken."))?;
    let step = six_digits(&input.code)
        .and_then(|code| matching_step(&secret, code, current_step()))
        .ok_or_else(wrong)?;
    if !api.engine.confirm_totp(member, id, step).await? {
        return Err(wrong());
    }
    let factors = api.engine.factors(member).await?;
    let factor = factors.into_iter().find(|f| f.id == id);
    factor
        .map(Json)
        .ok_or_else(|| internal("The authenticator vanished."))
}

/// `POST /api/v1/me/two_step/email`: turns on emailed codes as the
/// caller's second step. 409 until SMTP is set up, 422 without an email.
pub(crate) async fn add_email(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
) -> ApiResult<(StatusCode, Json<Factor>)> {
    let member = api.engine.member(member_of(&caller)?).await?;
    if member.email.is_none() {
        return Err(ApiError::invalid("Add your email first."));
    }
    if api.engine.smtp_settings().await?.0.is_none() {
        return Err(ApiError::conflict(
            "Email codes need a mail server. Ask an admin to set up email in Settings.",
        ));
    }
    let factor = api.engine.add_email_factor(member.id).await?;
    Ok((StatusCode::CREATED, Json(factor)))
}

/// `DELETE /api/v1/me/two_step/{id}`: removes one of the caller's second
/// steps; `409` for the last one while the policy requires 2-step.
pub(crate) async fn remove(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    let id: FactorId = id
        .parse()
        .map_err(|_| ApiError::invalid(format!("`{id}` is not a 2-step method id.")))?;
    let member = member_of(&caller)?;
    let factors = api.engine.factors(member).await?;
    if factors.len() == 1
        && factors[0].id == id
        && api.engine.security_policy().await?.require_two_step
    {
        return Err(ApiError::conflict(
            "Add another method before removing this one.",
        ));
    }
    api.engine.delete_factor(member, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /api/v1/me/two_step/challenge`: `{challenge, methods}` to answer in
/// `PUT /me/password` when the policy asks for a second step there (mail a
/// code with `POST /session/two_step/email`). 422 without a second step.
pub(crate) async fn my_challenge(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
) -> ApiResult<Json<Value>> {
    let member = api.engine.member(member_of(&caller)?).await?;
    let methods = methods(&api, &member).await?;
    if methods.is_empty() {
        return Err(ApiError::invalid("You have no 2-step method."));
    }
    Ok(Json(challenge(&api, member.id, &methods).await?))
}

/// For `PUT /me/password`: when the policy asks for a second step to change
/// a password and `member` has one, `answer` must pass it (403 otherwise).
/// Answers count in the `current_password` limiter per member and `ip`,
/// across challenges; the caller clears it once the change is saved.
pub(super) async fn check_password_change(
    api: &Api,
    member: MemberId,
    answer: Option<&Answer>,
    ip: &str,
) -> ApiResult<()> {
    if !api
        .engine
        .security_policy()
        .await?
        .two_step_for_password_change
        || api.engine.factors(member).await?.is_empty()
    {
        return Ok(());
    }
    let Some(answer) = answer else {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "two_step_required",
            "Confirm with your 2-step method to change your password.",
        ));
    };
    let (limiter, key) = (&api.auth.current_password, member.to_string());
    limiter.reserve(&key, ip)?;
    if check(api, answer, Some(member)).await?.is_none()
        || !api.engine.end_two_step(&digest(&answer.challenge)).await?
    {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "forbidden",
            "That 2-step code is not right.",
        ));
    }
    limiter.release(&key, ip);
    Ok(())
}

/// `DELETE /api/v1/members/{id}/two_step` (deployment owner): removes every
/// second step of the member (they lost their phone) and ends their
/// sessions. Not on yourself (`not_yourself`).
pub(crate) async fn reset(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
) -> ApiResult<Json<Member>> {
    let id = parse_id(&id)?;
    not_yourself(&caller, id)?;
    api.engine.member(id).await?;
    Ok(Json(api.engine.reset_two_step(id, &caller.actor()).await?))
}

/// `GET /api/v1/settings/security` (deployment owner): the 2-step rules.
pub(crate) async fn get_policy(State(api): State<Api>) -> ApiResult<Json<SecurityPolicy>> {
    Ok(Json(api.engine.security_policy().await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PolicyChange {
    require_two_step: Option<bool>,
    two_step_for_password_change: Option<bool>,
}

/// `PUT /api/v1/settings/security {require_two_step?, two_step_for_password_change?}`
/// (deployment owner): saves the 2-step rules; a rule left out keeps its
/// value. Answers with the saved rules.
pub(crate) async fn put_policy(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Body(input): Body<PolicyChange>,
) -> ApiResult<Json<SecurityPolicy>> {
    let change = |policy: &mut SecurityPolicy| {
        if let Some(on) = input.require_two_step {
            policy.require_two_step = on;
        }
        if let Some(on) = input.two_step_for_password_change {
            policy.two_step_for_password_change = on;
        }
    };
    let policy = api
        .engine
        .set_security_policy(change, &caller.actor())
        .await?;
    Ok(Json(policy))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn auth_totp_matches_the_rfc_6238_vectors() {
        // RFC 6238 appendix B, SHA-1: the 20-byte ASCII key, 8 digits.
        let key = b"12345678901234567890";
        for (time, code) in [
            (59u64, 94_287_082),
            (1_111_111_109, 7_081_804),
            (1_111_111_111, 14_050_471),
            (1_234_567_890, 89_005_924),
            (2_000_000_000, 69_279_037),
            (20_000_000_000, 65_353_130),
        ] {
            assert_eq!(hotp(key, time / 30) % 100_000_000, code, "T={time}");
        }
        // ±1 step of clock drift; nothing further, nothing else.
        let now = 1_111_111_111 / 30;
        assert_eq!(matching_step(key, 50_471, now), Some(now));
        assert_eq!(matching_step(key, 50_471, now + 1), Some(now));
        assert_eq!(matching_step(key, 50_471, now - 1), Some(now));
        assert_eq!(matching_step(key, 50_471, now + 2), None);
        assert_eq!(matching_step(key, 50_472, now), None);
        // RFC 4648 base32, and the hex the store keeps.
        assert_eq!(base32(b"foobar"), "MZXW6YTBOI");
        assert_eq!(base32(key), "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ");
        assert_eq!(unhex(&hex(key)).unwrap(), key);
        assert_eq!(unhex("zz"), None);
        assert_eq!(six_digits(" 012345 "), Some(12_345));
        assert_eq!(six_digits("12345"), None);
        assert_eq!(six_digits("12345a"), None);
    }
}
