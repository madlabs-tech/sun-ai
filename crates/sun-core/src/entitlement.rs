//! Plans and what a workspace may use.
//!
//! The effective [`Entitlement`] of a workspace comes from an
//! [`EntitlementProvider`]: Sun Cloud's per-workspace policy, or
//! [`Community`] (self-hosted, no limits). A provider never fails: anything
//! wrong falls back to an unlimited Community entitlement, so free features
//! keep working.
//!
//! The signed [`License`] ([`verify_license`], [`issuer`]) is kept for the
//! cloud's license tooling only; the engine no longer reads a license.

use async_trait::async_trait;
use chrono::Duration;
use ed25519_dalek::{Signature, VerifyingKey};
use serde::{Deserialize, Serialize};

use crate::{Error, Result, Timestamp, WorkspaceId};

/// Token prefix; bump the version when the payload or encoding changes.
pub const LICENSE_PREFIX: &str = "sunlic1";

/// Domain separation: what is actually signed is this tag plus the payload,
/// so a signature made for something else (e.g. a mod manifest) never
/// verifies as a license.
const SIGNING_CONTEXT: &[u8] = b"sun-license-v1\n";

/// Upper bound on token size; anything larger is rejected before decoding.
const MAX_TOKEN_LEN: usize = 16 * 1024;

string_enum! {
    /// Sun plans.
    pub enum Plan {
        /// Self-hosted: the full engine, no limits.
        Community => "community",
        /// Sun Cloud Free.
        Free => "free",
        /// Sun Cloud Pro (paid).
        Pro => "pro",
        /// Sun Cloud Business (paid).
        Business => "business",
    }
}

string_enum! {
    /// A countable resource a plan may cap.
    pub enum Quota {
        /// Member seats.
        Seats => "seats",
        /// Registered runtimes.
        Runtimes => "runtimes",
        /// Bring-your-own API keys.
        ByoKeys => "byo_keys",
        /// Agents.
        Agents => "agents",
        /// Runtime hosts.
        Hosts => "hosts",
    }
}

/// Caps per [`Quota`]; `None` (JSON `null` or missing) means unlimited.
/// Unknown keys are ignored, so a newer policy still reads.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Limits {
    /// Max seats.
    #[serde(default)]
    pub seats: Option<u32>,
    /// Max runtimes.
    #[serde(default)]
    pub runtimes: Option<u32>,
    /// Max BYO keys.
    #[serde(default)]
    pub byo_keys: Option<u32>,
    /// Max agents.
    #[serde(default)]
    pub agents: Option<u32>,
    /// Max hosts.
    #[serde(default)]
    pub hosts: Option<u32>,
}

impl Limits {
    /// The cap for `quota`, `None` when unlimited.
    pub fn max(&self, quota: Quota) -> Option<u32> {
        match quota {
            Quota::Seats => self.seats,
            Quota::Runtimes => self.runtimes,
            Quota::ByoKeys => self.byo_keys,
            Quota::Agents => self.agents,
            Quota::Hosts => self.hosts,
        }
    }
}

/// Where a workspace's entitlement comes from right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntitlementStatus {
    /// Sun Cloud's policy, fresh until `valid_until`.
    Cloud {
        /// When the policy must be fetched again.
        valid_until: Timestamp,
    },
    /// Sun Cloud's last policy, past its validity: refreshes are failing.
    Stale,
    /// Sun Cloud is configured but unreachable and nothing usable is cached:
    /// Community (no limits) until it answers.
    Unavailable,
    /// Self-hosted, no Sun Cloud: Community.
    Community,
}

impl EntitlementStatus {
    /// The snake_case name on the wire.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cloud { .. } => "cloud",
            Self::Stale => "stale",
            Self::Unavailable => "unavailable",
            Self::Community => "community",
        }
    }

    /// Until when a cloud policy is fresh; `None` otherwise.
    pub fn valid_until(self) -> Option<Timestamp> {
        match self {
            Self::Cloud { valid_until } => Some(valid_until),
            Self::Stale | Self::Unavailable | Self::Community => None,
        }
    }
}

/// What a workspace may use right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entitlement {
    /// Effective plan.
    pub plan: Plan,
    /// A paid plan: unlocks exclusive mods.
    pub paid: bool,
    /// Effective caps.
    pub limits: Limits,
    /// Why this is the effective plan.
    pub status: EntitlementStatus,
}

impl Entitlement {
    /// Community: full engine, no limits, unpaid.
    pub fn community(status: EntitlementStatus) -> Self {
        Self {
            plan: Plan::Community,
            paid: false,
            limits: Limits::default(),
            status,
        }
    }

    /// May one more `quota` be added when `current` already exist?
    /// [`Error::PlanLimit`] says which cap it hit, on which plan.
    pub fn check(&self, quota: Quota, current: usize) -> Result<()> {
        match self.limits.max(quota) {
            Some(max) if current >= usize::try_from(max).unwrap_or(usize::MAX) => {
                let what = match quota {
                    Quota::ByoKeys => "BYO keys",
                    other => other.as_str(),
                };
                Err(Error::PlanLimit(format!(
                    "Plan limit reached ({what}: {max} on the {} plan). Remove one or upgrade the plan.",
                    self.plan
                )))
            }
            _ => Ok(()),
        }
    }
}

/// Where each workspace's [`Entitlement`] comes from (**Strategy**: Sun
/// Cloud's policy, or [`Community`]).
///
/// Contract: never fails and never blocks long; anything wrong (unreachable,
/// bad answer) falls back to the last good value or to an unlimited
/// Community entitlement.
#[async_trait]
pub trait EntitlementProvider: Send + Sync {
    /// The entitlement of `workspace` now.
    async fn for_workspace(&self, workspace: WorkspaceId) -> Entitlement;
}

/// Self-hosted without Sun Cloud: every workspace is Community, unlimited.
#[derive(Debug, Clone, Copy, Default)]
pub struct Community;

#[async_trait]
impl EntitlementProvider for Community {
    async fn for_workspace(&self, _workspace: WorkspaceId) -> Entitlement {
        Entitlement::community(EntitlementStatus::Community)
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
            limits: Limits {
                agents: Some(5),
                ..Limits::default()
            },
        }
    }

    #[test]
    fn community_is_unlimited_and_unpaid() {
        use std::task::{Context, Poll, Waker};
        let mut future = Community.for_workspace(WorkspaceId::MAIN);
        let Poll::Ready(e) = future
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
        else {
            panic!("Community answers at once");
        };
        assert_eq!(e.plan, Plan::Community);
        assert_eq!(e.status, EntitlementStatus::Community);
        assert!(!e.paid);
        assert_eq!(e.limits, Limits::default());
        assert!(e.check(Quota::Hosts, usize::MAX - 1).is_ok());
    }

    #[test]
    fn plans_and_limits_read_the_policy_json() {
        let plans: Vec<Plan> = serde_json::from_str(r#"["free","pro","business"]"#).unwrap();
        assert_eq!(plans, [Plan::Free, Plan::Pro, Plan::Business]);
        assert!(serde_json::from_str::<Plan>(r#""individual""#).is_err());
        let limits: Limits = serde_json::from_str(
            r#"{"seats": 3, "runtimes": null, "agents": 5, "hosts": 1, "future_cap": 9}"#,
        )
        .unwrap();
        assert_eq!(
            limits,
            Limits {
                seats: Some(3),
                agents: Some(5),
                hosts: Some(1),
                ..Limits::default()
            }
        );
        let e = Entitlement {
            plan: Plan::Pro,
            limits,
            ..Entitlement::community(EntitlementStatus::Stale)
        };
        assert!(e.check(Quota::Agents, 4).is_ok());
        assert_eq!(
            e.check(Quota::Agents, 5),
            Err(Error::PlanLimit(
                "Plan limit reached (agents: 5 on the pro plan). Remove one or upgrade the plan."
                    .into()
            ))
        );
        assert!(e.check(Quota::Hosts, 1).is_err());
        assert!(e.check(Quota::Runtimes, 1_000).is_ok());
    }

    #[test]
    fn status_names_and_validity() {
        let cloud = EntitlementStatus::Cloud { valid_until: at(2) };
        assert_eq!(cloud.as_str(), "cloud");
        assert_eq!(cloud.valid_until(), Some(at(2)));
        for (status, name) in [
            (EntitlementStatus::Stale, "stale"),
            (EntitlementStatus::Unavailable, "unavailable"),
            (EntitlementStatus::Community, "community"),
        ] {
            assert_eq!((status.as_str(), status.valid_until()), (name, None));
        }
    }

    #[test]
    fn valid_license_round_trips() {
        let token = dev::sign(&license(Plan::Business), &SEED).unwrap();
        assert_eq!(
            verify_license(&token, &dev::public_key(&SEED)).unwrap(),
            license(Plan::Business)
        );
        assert_eq!(license(Plan::Pro).grace_ends_at(), at(17));
    }

    #[test]
    fn tampered_or_foreign_tokens_do_not_verify() {
        let t = dev::sign(&license(Plan::Free), &SEED).unwrap();
        let (head, sig) = t.rsplit_once('.').unwrap();
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
            assert!(
                verify_license(bad, &dev::public_key(&SEED)).is_err(),
                "{bad:.40}"
            );
        }
    }
}
