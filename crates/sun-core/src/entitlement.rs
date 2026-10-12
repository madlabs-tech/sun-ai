//! Plans and signed entitlements.
//!
//! A license is a token issued and signed (ed25519) by Sun Cloud:
//! `sunlic1.<hex payload json>.<hex signature>`. The engine only holds Sun's
//! public key; verifying needs no network. A license keeps working offline for
//! [`License::offline_grace_days`] after `expires_at`, then the ecosystem falls
//! back to the [`Plan::Community`] tier. No license, a bad license or an
//! expired one never blocks free features: [`Entitlement::resolve`] always
//! returns a usable entitlement.
//!
//! Pure functions only: the caller passes the token, the public key and `now`.

use chrono::Duration;
use ed25519_dalek::{Signature, VerifyingKey};
use serde::{Deserialize, Serialize};

use crate::{Error, Result, Timestamp};

/// Token prefix; bump the version when the payload or encoding changes.
pub const LICENSE_PREFIX: &str = "sunlic1";

/// Domain separation: what is actually signed is this tag plus the payload,
/// so a signature made for something else (e.g. a mod manifest) never
/// verifies as a license.
const SIGNING_CONTEXT: &[u8] = b"sun-license-v1\n";

/// Upper bound on token size; anything larger is rejected before decoding.
const MAX_TOKEN_LEN: usize = 16 * 1024;

/// Allowed clock skew before `issued_at` counts as "in the future".
const CLOCK_SKEW_MINUTES: i64 = 5;

string_enum! {
    /// Sun plans (business.md).
    pub enum Plan {
        /// Self-hosted, no license: the full engine, no seat or run limits.
        Community => "community",
        /// Sun Cloud Free: ecosystem hosted, agents run on the user's machine.
        Free => "free",
        /// Sun Cloud Individual (paid).
        Individual => "individual",
        /// Sun Cloud Business (paid).
        Business => "business",
    }
}

impl Plan {
    /// Paid plans unlock exclusive mods, in the cloud or self-hosted.
    pub fn is_paid(self) -> bool {
        match self {
            Self::Community | Self::Free => false,
            Self::Individual | Self::Business => true,
        }
    }

    /// The Sun Cloud caps from business.md, for an issuer that has no
    /// per-customer override. Community has none. Paid plans inherit Free's
    /// caps ("everything in Free"); the license may carry different ones.
    pub fn cloud_limits(self) -> Limits {
        match self {
            Self::Community => Limits::default(),
            Self::Free | Self::Individual | Self::Business => Limits {
                ecosystems: Some(1),
                runtimes: Some(2),
                byo_keys: Some(2),
                agents: Some(5),
                seats: None,
            },
        }
    }
}

string_enum! {
    /// A countable resource a plan may cap.
    pub enum Quota {
        /// Ecosystems.
        Ecosystems => "ecosystems",
        /// Registered runtimes.
        Runtimes => "runtimes",
        /// Bring-your-own API keys.
        ByoKeys => "byo_keys",
        /// Agents.
        Agents => "agents",
        /// Member seats.
        Seats => "seats",
    }
}

/// Caps per [`Quota`]; `None` means unlimited.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Limits {
    /// Max ecosystems (`workspaces` in licenses issued before the rename).
    #[serde(default)]
    #[serde(alias = "workspaces")]
    pub ecosystems: Option<u32>,
    /// Max runtimes.
    #[serde(default)]
    pub runtimes: Option<u32>,
    /// Max BYO keys.
    #[serde(default)]
    pub byo_keys: Option<u32>,
    /// Max agents.
    #[serde(default)]
    pub agents: Option<u32>,
    /// Max seats.
    #[serde(default)]
    pub seats: Option<u32>,
}

impl Limits {
    /// The cap for `quota`, `None` when unlimited.
    pub fn max(&self, quota: Quota) -> Option<u32> {
        match quota {
            Quota::Ecosystems => self.ecosystems,
            Quota::Runtimes => self.runtimes,
            Quota::ByoKeys => self.byo_keys,
            Quota::Agents => self.agents,
            Quota::Seats => self.seats,
        }
    }

    /// May one more `quota` be added when `current` already exist?
    pub fn check(&self, quota: Quota, current: u32) -> Result<()> {
        match self.max(quota) {
            Some(max) if current >= max => Err(Error::InvalidInput(format!(
                "plan limit reached: {max} {quota}"
            ))),
            _ => Ok(()),
        }
    }
}

/// The signed license payload, as issued by Sun Cloud.
///
/// Unknown fields are ignored so older engines accept newer licenses.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct License {
    /// Issuer's license id.
    pub license_id: String,
    /// Who it was sold to (shown in settings).
    pub licensee: String,
    /// The plan it grants.
    pub plan: Plan,
    /// When it was issued.
    pub issued_at: Timestamp,
    /// End of the paid period; normally refreshed before this.
    pub expires_at: Timestamp,
    /// Days the license keeps working past `expires_at` without a refresh.
    pub offline_grace_days: u16,
    /// Caps; omitted means unlimited.
    #[serde(default)]
    pub limits: Limits,
}

impl License {
    /// The last moment the license still grants its plan.
    pub fn grace_ends_at(&self) -> Timestamp {
        self.expires_at
            .checked_add_signed(Duration::days(i64::from(self.offline_grace_days)))
            .unwrap_or(Timestamp::MAX_UTC)
    }
}

/// Parse and verify a license token against Sun's public key.
///
/// Checks the encoding and signature only; expiry is [`Entitlement::resolve`]'s job.
pub fn verify_license(token: &str, public_key: &[u8; 32]) -> Result<License> {
    let invalid = |why: &str| Error::InvalidInput(format!("license: {why}"));
    let token = token.trim();
    if token.len() > MAX_TOKEN_LEN {
        return Err(invalid("token too large"));
    }
    let mut parts = token.split('.');
    let (Some(LICENSE_PREFIX), Some(payload_hex), Some(signature_hex), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(invalid("not a sunlic1 token"));
    };
    let payload = hex_decode(payload_hex).ok_or_else(|| invalid("bad payload encoding"))?;
    let signature: [u8; 64] = hex_decode(signature_hex)
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or_else(|| invalid("bad signature encoding"))?;
    let key = VerifyingKey::from_bytes(public_key).map_err(|_| invalid("bad public key"))?;
    key.verify_strict(
        &signed_message(&payload),
        &Signature::from_bytes(&signature),
    )
    .map_err(|_| invalid("signature does not match"))?;
    let license: License =
        serde_json::from_slice(&payload).map_err(|e| invalid(&format!("bad payload: {e}")))?;
    if license.expires_at < license.issued_at {
        return Err(invalid("expires before it was issued"));
    }
    Ok(license)
}

/// Where the current entitlement comes from; drives the plan badge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum EntitlementStatus {
    /// No license installed: Community tier.
    Unlicensed,
    /// License valid.
    Active,
    /// Past `expires_at`, still honoured until `ends_at` (offline grace).
    Grace {
        /// When the grace period ends.
        ends_at: Timestamp,
    },
    /// Grace ran out: back on Community until a new license is installed.
    Expired {
        /// When the grace period ended.
        ended_at: Timestamp,
    },
    /// The token failed to verify: back on Community.
    Invalid {
        /// Why (safe to show; contains no secret).
        reason: String,
    },
}

/// What this ecosystem may use right now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entitlement {
    /// Effective plan.
    pub plan: Plan,
    /// Effective caps.
    pub limits: Limits,
    /// Why this is the effective plan.
    pub status: EntitlementStatus,
    /// The installed license, when it verified.
    pub license: Option<License>,
}

impl Entitlement {
    /// Self-hosted with no license: full engine, no limits, no exclusive mods.
    pub fn community(status: EntitlementStatus) -> Self {
        Self {
            plan: Plan::Community,
            limits: Limits::default(),
            status,
            license: None,
        }
    }

    /// Resolve the effective entitlement. Never fails: anything wrong with the
    /// license falls back to Community, so free features keep working.
    pub fn resolve(token: Option<&str>, public_key: &[u8; 32], now: Timestamp) -> Self {
        let Some(token) = token.filter(|t| !t.trim().is_empty()) else {
            return Self::community(EntitlementStatus::Unlicensed);
        };
        let license = match verify_license(token, public_key) {
            Ok(license) => license,
            Err(e) => {
                return Self::community(EntitlementStatus::Invalid {
                    reason: e.to_string(),
                });
            }
        };
        if now < license.issued_at - Duration::minutes(CLOCK_SKEW_MINUTES) {
            return Self::community(EntitlementStatus::Invalid {
                reason: "license: issued in the future, check the system clock".into(),
            });
        }
        let ends_at = license.grace_ends_at();
        let status = if now <= license.expires_at {
            EntitlementStatus::Active
        } else if now <= ends_at {
            EntitlementStatus::Grace { ends_at }
        } else {
            let mut expired = Self::community(EntitlementStatus::Expired { ended_at: ends_at });
            expired.license = Some(license);
            return expired;
        };
        Self {
            plan: license.plan,
            limits: license.limits,
            status,
            license: Some(license),
        }
    }

    /// Exclusive mods install only on a paid plan.
    pub fn allows_exclusive_mods(&self) -> bool {
        self.plan.is_paid()
    }
}

fn signed_message(payload: &[u8]) -> Vec<u8> {
    [SIGNING_CONTEXT, payload].concat()
}

fn hex_decode(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) || !text.is_ascii() {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).ok())
        .collect()
}

/// Token encoding for an issuer that holds the signing key elsewhere (KMS,
/// HSM): build the bytes to sign, have the key sign them, wrap the signature
/// into a token. Holds no key itself; enable the `license-issuer` feature.
#[cfg(any(test, feature = "license-issuer"))]
pub mod issuer {
    use super::{LICENSE_PREFIX, License, signed_message};
    use crate::{Error, Result};

    /// A license serialized once, so the signed bytes and the token payload
    /// are exactly the same.
    #[derive(Debug, Clone)]
    pub struct UnsignedLicense {
        payload: Vec<u8>,
    }

    impl UnsignedLicense {
        /// Serializes `license`.
        pub fn new(license: &License) -> Result<Self> {
            let payload =
                serde_json::to_vec(license).map_err(|e| Error::InvalidInput(e.to_string()))?;
            Ok(Self { payload })
        }

        /// The exact bytes the ed25519 key must sign (domain tag + payload).
        pub fn message(&self) -> Vec<u8> {
            signed_message(&self.payload)
        }

        /// The `sunlic1.` token for this payload and its ed25519 `signature`.
        pub fn into_token(self, signature: &[u8; 64]) -> String {
            format!(
                "{LICENSE_PREFIX}.{}.{}",
                hex_encode(&self.payload),
                hex_encode(signature)
            )
        }
    }

    pub(crate) fn hex_encode(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }
}

/// Dev/test-only issuer. The real signing key is Sun Cloud's and never ships
/// in the engine; enable the `dev-license` feature in a dev-dependency to use this.
#[cfg(any(test, feature = "dev-license"))]
pub mod dev {
    use ed25519_dalek::{Signer, SigningKey};

    use super::License;
    use super::issuer::UnsignedLicense;
    use crate::Result;

    /// The public key for a signing-key `seed` (deterministic keygen).
    pub fn public_key(seed: &[u8; 32]) -> [u8; 32] {
        SigningKey::from_bytes(seed).verifying_key().to_bytes()
    }

    /// Sign `license` with the key derived from `seed` into a token.
    pub fn sign(license: &License, seed: &[u8; 32]) -> Result<String> {
        let unsigned = UnsignedLicense::new(license)?;
        let signature = SigningKey::from_bytes(seed).sign(&unsigned.message());
        Ok(unsigned.into_token(&signature.to_bytes()))
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    const SEED: [u8; 32] = [7; 32];

    fn at(day: u32) -> Timestamp {
        chrono::Utc.with_ymd_and_hms(2026, 1, day, 0, 0, 0).unwrap()
    }

    fn license(plan: Plan) -> License {
        License {
            license_id: "lic_1".into(),
            licensee: "Acme".into(),
            plan,
            issued_at: at(1),
            expires_at: at(10),
            offline_grace_days: 7,
            limits: plan.cloud_limits(),
        }
    }

    fn token(plan: Plan) -> String {
        dev::sign(&license(plan), &SEED).unwrap()
    }

    fn resolve(token: Option<&str>, now: Timestamp) -> Entitlement {
        Entitlement::resolve(token, &dev::public_key(&SEED), now)
    }

    #[test]
    fn no_license_is_community_without_limits() {
        for token in [None, Some(""), Some("  \n")] {
            let e = resolve(token, at(5));
            assert_eq!(e.plan, Plan::Community);
            assert_eq!(e.status, EntitlementStatus::Unlicensed);
            assert_eq!(e.limits, Limits::default());
            assert!(!e.allows_exclusive_mods());
            assert!(e.limits.check(Quota::Runtimes, u32::MAX - 1).is_ok());
        }
    }

    /// A license signed before the rename says `workspaces`: its signed bytes
    /// are untouched and the cap still reads.
    #[test]
    fn a_license_issued_with_workspaces_still_verifies() {
        use ed25519_dalek::{Signer, SigningKey};
        let json = serde_json::to_string(&license(Plan::Business)).unwrap();
        assert!(json.contains("\"ecosystems\":1"), "{json}");
        let payload = json
            .replace("\"ecosystems\"", "\"workspaces\"")
            .into_bytes();
        let signature = SigningKey::from_bytes(&SEED).sign(&signed_message(&payload));
        let token = format!(
            "{LICENSE_PREFIX}.{}.{}",
            issuer::hex_encode(&payload),
            issuer::hex_encode(&signature.to_bytes())
        );
        let verified = verify_license(&token, &dev::public_key(&SEED)).unwrap();
        assert_eq!(verified, license(Plan::Business));
        assert_eq!(verified.limits.max(Quota::Ecosystems), Some(1));
    }

    #[test]
    fn valid_license_round_trips_and_grants_plan() {
        let t = token(Plan::Business);
        assert_eq!(
            verify_license(&t, &dev::public_key(&SEED)).unwrap(),
            license(Plan::Business)
        );
        let e = resolve(Some(&t), at(10));
        assert_eq!(e.plan, Plan::Business);
        assert_eq!(e.status, EntitlementStatus::Active);
        assert!(e.allows_exclusive_mods());
    }

    #[test]
    fn offline_grace_then_falls_back_to_community() {
        let t = token(Plan::Individual);
        let ends_at = at(17);
        let grace = resolve(Some(&t), at(11));
        assert_eq!(grace.plan, Plan::Individual);
        assert_eq!(grace.status, EntitlementStatus::Grace { ends_at });
        assert_eq!(resolve(Some(&t), ends_at).plan, Plan::Individual);

        let expired = resolve(Some(&t), at(18));
        assert_eq!(expired.plan, Plan::Community);
        assert_eq!(
            expired.status,
            EntitlementStatus::Expired { ended_at: ends_at }
        );
        assert!(!expired.allows_exclusive_mods());
        assert!(expired.license.is_some());
    }

    #[test]
    fn tampered_or_foreign_tokens_are_invalid_but_harmless() {
        let t = token(Plan::Free);
        let (head, sig) = t.rsplit_once('.').unwrap();
        // Re-sign a Business payload with another key, swap the plan in place,
        // break the encoding: all must fall back to Community.
        let other_key = dev::sign(&license(Plan::Business), &[9; 32]).unwrap();
        let swapped = t.replacen(
            &issuer::hex_encode(b"\"free\""),
            &issuer::hex_encode(b"\"business\""),
            1,
        );
        assert_ne!(swapped, t);
        let flipped_sig = format!(
            "{head}.{}{}",
            if sig.starts_with('0') { "1" } else { "0" },
            &sig[1..]
        );
        for bad in [
            other_key.as_str(),
            swapped.as_str(),
            flipped_sig.as_str(),
            "sunlic1.zz.00",
            "garbage",
            &"a".repeat(MAX_TOKEN_LEN + 1),
        ] {
            let e = resolve(Some(bad), at(5));
            assert_eq!(e.plan, Plan::Community, "{bad:.40}");
            assert!(matches!(e.status, EntitlementStatus::Invalid { .. }));
        }
    }

    #[test]
    fn clock_before_issue_is_invalid() {
        let e = resolve(Some(&token(Plan::Business)), at(1) - Duration::hours(1));
        assert!(matches!(e.status, EntitlementStatus::Invalid { .. }));
    }

    #[test]
    fn limits_check_counts_against_cap() {
        let limits = Plan::Free.cloud_limits();
        assert!(limits.check(Quota::Agents, 4).is_ok());
        assert!(limits.check(Quota::Agents, 5).is_err());
        assert!(limits.check(Quota::Seats, 1_000).is_ok());
    }

    #[test]
    fn entitlement_status_serializes_for_the_badge() {
        let json = serde_json::to_value(EntitlementStatus::Grace { ends_at: at(17) }).unwrap();
        assert_eq!(json["state"], "grace");
        let plan: Plan = serde_json::from_value("business".into()).unwrap();
        assert!(plan.is_paid());
    }
}
