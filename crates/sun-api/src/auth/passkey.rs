//! Passkeys as a second step: WebAuthn credentials checked by the
//! pure-Rust `passkey-auth` relying party (ES256 and EdDSA, attestation
//! `none`). The relying party is the public URL's host, so passkeys work
//! only where browsers allow them: https, or `localhost`. Each ceremony's
//! state (the challenge the authenticator must sign) lives in a 2-step
//! challenge row, so it works once and dies with that row. Browsers speak
//! WebAuthn's camelCase JSON (`toJSON()`, `parse*OptionsFromJSON`); this
//! module is the one boundary that handles it.

use axum::extract::State;
use axum::http::StatusCode;
use axum::{Extension, Json};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use openidconnect::url::Url;
use passkey_auth::{
    Attachment, AuthenticationResponse, AuthenticationState, CredentialId, PasskeyCredential,
    RegistrationResponse, Webauthn,
};
use serde::Deserialize;
use serde_json::{Value, json};
use sun_core::model::{Factor, Member};
use sun_core::{Error, MemberId};

use super::two_step::{CHALLENGE_TTL, ChallengeData, label, member_of};
use super::{Caller, digest};
use crate::Api;
use crate::error::{ApiError, ApiResult};
use crate::hosts::random_hex;
use crate::routes::Body;

/// The relying party at `public_url`; `None` where browsers refuse
/// passkeys (plain http but on localhost, or an IP address).
pub(super) fn relying_party(public_url: &str) -> Option<Webauthn> {
    let url = Url::parse(public_url).ok()?;
    let host = url.domain()?;
    if url.scheme() != "https" && host != "localhost" {
        return None;
    }
    Some(
        Webauthn::new(host, "Sun", &url.origin().ascii_serialization())
            .authenticator_attachment(Attachment::Any)
            .strict_base64(true),
    )
}

fn rp(api: &Api) -> ApiResult<Webauthn> {
    relying_party(&api.config.public_url).ok_or_else(|| {
        ApiError::conflict(
            "Passkeys need Sun on https or localhost. Ask an admin to set its public URL.",
        )
    })
}

/// A browser's `PublicKeyCredential.toJSON()` in the flat shape
/// `passkey-auth` reads: `response`'s fields plus the credential `id`.
fn flatten(credential: &Value) -> Value {
    let mut flat = credential.get("response").cloned().unwrap_or(Value::Null);
    if let (Some(fields), Some(id)) = (flat.as_object_mut(), credential.get("id")) {
        fields.insert("id".into(), id.clone());
    }
    flat
}

/// Whether `clientDataJSON` (base64url) says the ceremony ran in a frame of
/// another site (`crossOrigin: true`); Sun never frames itself, so such a
/// ceremony is refused. `passkey-auth` parses the flag but doesn't check it.
fn cross_origin(client_data_json: &str) -> bool {
    URL_SAFE_NO_PAD
        .decode(client_data_json)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .is_some_and(|data| data["crossOrigin"] == Value::Bool(true))
}

/// The member's passkeys, each with its factor id.
async fn credentials(
    api: &Api,
    member: MemberId,
) -> ApiResult<Vec<(sun_core::FactorId, PasskeyCredential)>> {
    Ok(api
        .engine
        .passkeys(member)
        .await?
        .into_iter()
        .filter_map(|(id, text)| Some((id, serde_json::from_str(&text).ok()?)))
        .collect())
}

/// `POST /api/v1/me/two_step/passkey`: starts adding a passkey.
/// `{challenge, options}`: `options` is for
/// `PublicKeyCredential.parseCreationOptionsFromJSON`; answer at
/// `/passkey/confirm` within 5 minutes. 409 where passkeys can't work.
pub(crate) async fn start(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
) -> ApiResult<Json<Value>> {
    let member = api.engine.member(member_of(&caller)?).await?;
    let rp = rp(&api)?;
    let existing: Vec<CredentialId> = credentials(&api, member.id)
        .await?
        .into_iter()
        .map(|(_, c)| c.id)
        .collect();
    let name = member.email.as_deref().unwrap_or(&member.username);
    let (options, state) = rp.start_registration(
        member.id.to_string().as_bytes(),
        name,
        &member.username,
        &existing,
    );
    let data = ChallengeData {
        register: Some(state),
        ..ChallengeData::default()
    };
    let challenge = random_hex()?;
    api.engine
        .start_two_step(
            &digest(&challenge),
            member.id,
            chrono::Utc::now() + CHALLENGE_TTL,
            &data.text(),
        )
        .await?;
    Ok(Json(json!({ "challenge": challenge, "options": options })))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Confirm {
    challenge: String,
    /// The browser's `PublicKeyCredential.toJSON()`.
    credential: Value,
    #[serde(default)]
    label: String,
}

/// `POST /api/v1/me/two_step/passkey/confirm {challenge, credential, label?}`:
/// checks the new passkey (origin, not framed by another site, relying
/// party, the challenge) and turns it on: `201` with the factor. 422 when it
/// doesn't check out, its credential is added already (by anyone), or the
/// challenge was used.
pub(crate) async fn confirm(
    State(api): State<Api>,
    Extension(caller): Extension<Caller>,
    Body(input): Body<Confirm>,
) -> ApiResult<(StatusCode, Json<Factor>)> {
    let member = member_of(&caller)?;
    let label = label(&input.label, "Passkey")?;
    let rp = rp(&api)?;
    let again = || ApiError::invalid("That passkey isn't being added. Start again.");
    let hash = digest(&input.challenge);
    let state = match api.engine.two_step(&hash).await? {
        Some((id, data)) if id == member => ChallengeData::parse(&data).register,
        _ => None,
    }
    .ok_or_else(again)?;
    let refused = || ApiError::invalid("Sun couldn't check that passkey. Try again.");
    let response: RegistrationResponse =
        serde_json::from_value(flatten(&input.credential)).map_err(|_| refused())?;
    if cross_origin(&response.client_data_json) {
        return Err(refused());
    }
    let credential = rp
        .finish_registration(&state, &response)
        .map_err(|_| refused())?;
    // A credential id names one passkey of one member (WebAuthn §7.1 step 25):
    // the store's unique index refuses a taken one (`InvalidInput`), races too.
    let text = serde_json::to_string(&credential).map_err(|_| refused())?;
    let factor = match api.engine.add_passkey(member, label, &text, &hash).await {
        Ok(factor) => factor.ok_or_else(again)?,
        Err(Error::InvalidInput(taken)) => return Err(ApiError::invalid(taken)),
        Err(e) => return Err(e.into()),
    };
    Ok((StatusCode::CREATED, Json(factor)))
}

/// For `POST /session/two_step {challenge, method: "passkey"}`: request
/// options for `member`'s passkeys (for
/// `PublicKeyCredential.parseRequestOptionsFromJSON`), kept with the
/// challenge whose `data` is `data`. `None` without a passkey or where
/// passkeys can't work.
pub(super) async fn options(
    api: &Api,
    member: &Member,
    challenge_hash: &str,
    data: &str,
) -> ApiResult<Option<Value>> {
    let creds: Vec<PasskeyCredential> = credentials(api, member.id)
        .await?
        .into_iter()
        .map(|(_, c)| c)
        .collect();
    let Some(rp) = relying_party(&api.config.public_url) else {
        return Ok(None);
    };
    if creds.is_empty() {
        return Ok(None);
    }
    let (options, state) =
        rp.start_authentication_with_creds_for_user(member.id.to_string().as_bytes(), &creds);
    let mut data = ChallengeData::parse(data);
    data.passkey = Some(state);
    api.engine
        .set_two_step_data(challenge_hash, &data.text(), false)
        .await?;
    Ok(Some(json!({ "options": options })))
}

/// Checks `credential` (the browser's `toJSON()` of an assertion) against
/// the ceremony `state` and `member`'s passkeys, and saves the passkey's new
/// sign count; `false` for anything wrong.
pub(super) async fn verify(
    api: &Api,
    member: MemberId,
    state: Option<&AuthenticationState>,
    credential: Option<&Value>,
) -> ApiResult<bool> {
    let (Some(rp), Some(state), Some(credential)) =
        (relying_party(&api.config.public_url), state, credential)
    else {
        return Ok(false);
    };
    let Ok(response) = serde_json::from_value::<AuthenticationResponse>(flatten(credential)) else {
        return Ok(false);
    };
    if cross_origin(&response.client_data_json) {
        return Ok(false);
    }
    let creds = credentials(api, member).await?;
    let Some((id, mut stored)) = creds
        .into_iter()
        .find(|(_, c)| c.id.to_b64url() == response.id)
    else {
        return Ok(false);
    };
    let Ok(done) = rp.finish_authentication(state, &response, &stored) else {
        return Ok(false);
    };
    stored.counter = done.new_counter;
    let text = serde_json::to_string(&stored)
        .map_err(|_| ApiError::invalid("Sun couldn't save that passkey."))?;
    api.engine.use_passkey(id, &text).await?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_passkeys_need_https_or_localhost() {
        let rp_id = |url: &str| relying_party(url).map(|rp| rp.rp_id().to_owned());
        assert_eq!(
            rp_id("https://sun.example.com").as_deref(),
            Some("sun.example.com")
        );
        assert_eq!(rp_id("http://localhost:7777").as_deref(), Some("localhost"));
        assert_eq!(
            rp_id("https://sun.example:8443/").as_deref(),
            Some("sun.example")
        );
        assert_eq!(rp_id("http://sun.example.com"), None);
        assert_eq!(rp_id("http://127.0.0.1:7777"), None);
        assert_eq!(rp_id("https://10.0.0.2"), None);
        assert_eq!(rp_id("not a url"), None);
    }

    #[test]
    fn auth_passkey_json_is_flattened_for_the_verifier() {
        let browser = json!({
            "id": "abc", "rawId": "abc", "type": "public-key",
            "response": {"clientDataJSON": "c", "authenticatorData": "a", "signature": "s"}
        });
        let flat = flatten(&browser);
        assert_eq!(flat["id"], "abc");
        assert_eq!(flat["signature"], "s");
        assert_eq!(flatten(&json!("junk")), Value::Null);
    }

    #[test]
    fn auth_passkey_cross_origin_flag_is_read() {
        let encode = |v: Value| URL_SAFE_NO_PAD.encode(v.to_string());
        assert!(cross_origin(&encode(json!({"crossOrigin": true}))));
        assert!(!cross_origin(&encode(json!({"crossOrigin": false}))));
        assert!(!cross_origin(&encode(json!({"type": "webauthn.create"}))));
        assert!(!cross_origin("not base64 json"));
    }
}
