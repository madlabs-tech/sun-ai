//! The status vocabulary of one runtime, a `(host, tool)` pair, as
//! `GET /runtimes` reports it (TRD BLO-117 §6). Three observed facts —
//! [`Presence`], [`Availability`] and the live session count — and the
//! [`RuntimeState`] derived from them. **The `state` values are public API
//! (a one-way door): clients switch on them. Don't add, rename or alias one
//! without a decision on the TRD.**

use serde::{Deserialize, Serialize};

use crate::Readiness;

/// Is the host connected right now?
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Presence {
    /// The host's socket is up and answered within the liveness window.
    Online,
    /// Disconnected, or silent for the liveness window.
    Offline,
}

/// Can the tool take a session, from the host's last report? Read even
/// while the host is offline (its last report), so the UI can say what was
/// true when it was last seen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Availability {
    /// Installed, logged in, tested version.
    Ready,
    /// Installed, not logged in.
    NeedsLogin,
    /// The account hit its usage limit and is cooling down.
    Limited,
    /// Ready, but on a version the contract tests don't cover. Schedulable.
    Outdated,
    /// On the detection list without an adapter or profile to run it.
    Unsupported,
    /// The CLI has no cheap login check. Schedulable; the first auth failure
    /// turns it into [`NeedsLogin`](Self::NeedsLogin).
    AuthUnknown,
}

impl Availability {
    /// From a host's readiness report and whether the account is cooling.
    /// Precedence: not logged in, then cooling, then an untested version.
    ///
    /// `Outdated` reads the probe's contract: a logged-in readiness carries a
    /// `detail` only to warn about the version (untested, or none reported).
    // ponytail: `Unsupported` and `AuthUnknown` need host fields (a detection
    // list, an auth state) that don't exist yet; nothing reports them today.
    pub fn of(readiness: &Readiness, cooling: bool) -> Self {
        if !readiness.logged_in {
            Self::NeedsLogin
        } else if cooling {
            Self::Limited
        } else if readiness.detail.is_some() {
            Self::Outdated
        } else {
            Self::Ready
        }
    }
}

/// What a runtime is doing, derived with the precedence **offline >
/// unsupported > needs_login > limited > busy > idle**. `outdated` and
/// `auth_unknown` are not states: they show as a badge on `idle`/`busy`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeState {
    /// The host is not connected.
    Offline,
    /// Sun can see the tool but can't run it.
    Unsupported,
    /// Installed but not logged in.
    NeedsLogin,
    /// The account's usage limit; back at `cooldown_until`.
    Limited,
    /// At least one session running.
    Busy,
    /// Ready, nothing running.
    Idle,
}

impl RuntimeState {
    /// The state of a runtime with `active` sessions running.
    pub fn derive(presence: Presence, availability: Availability, active: u32) -> Self {
        match (presence, availability) {
            (Presence::Offline, _) => Self::Offline,
            (_, Availability::Unsupported) => Self::Unsupported,
            (_, Availability::NeedsLogin) => Self::NeedsLogin,
            (_, Availability::Limited) => Self::Limited,
            (
                Presence::Online,
                Availability::Ready | Availability::Outdated | Availability::AuthUnknown,
            ) if active > 0 => Self::Busy,
            (
                Presence::Online,
                Availability::Ready | Availability::Outdated | Availability::AuthUnknown,
            ) => Self::Idle,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn readiness(logged_in: bool, detail: Option<&str>) -> Readiness {
        Readiness {
            installed: true,
            version: Some("2.2.0".into()),
            logged_in,
            detail: detail.map(Into::into),
        }
    }

    #[test]
    fn availability_puts_login_before_cooling_before_version() {
        let untested = Some("Claude Code 2.2.0 is untested");
        assert_eq!(
            Availability::of(&readiness(false, untested), true),
            Availability::NeedsLogin
        );
        assert_eq!(
            Availability::of(&readiness(true, untested), true),
            Availability::Limited
        );
        assert_eq!(
            Availability::of(&readiness(true, untested), false),
            Availability::Outdated
        );
        assert_eq!(
            Availability::of(&readiness(true, None), false),
            Availability::Ready
        );
    }

    #[test]
    fn state_precedence_is_offline_unsupported_login_limited_busy_idle() {
        use Availability as A;
        use Presence::{Offline, Online};
        use RuntimeState as S;
        let all = [
            A::Ready,
            A::NeedsLogin,
            A::Limited,
            A::Outdated,
            A::Unsupported,
            A::AuthUnknown,
        ];
        for availability in all {
            assert_eq!(RuntimeState::derive(Offline, availability, 3), S::Offline);
        }
        assert_eq!(
            RuntimeState::derive(Online, A::Unsupported, 1),
            S::Unsupported
        );
        assert_eq!(
            RuntimeState::derive(Online, A::NeedsLogin, 1),
            S::NeedsLogin
        );
        assert_eq!(RuntimeState::derive(Online, A::Limited, 1), S::Limited);
        for badge in [A::Ready, A::Outdated, A::AuthUnknown] {
            assert_eq!(RuntimeState::derive(Online, badge, 2), S::Busy);
            assert_eq!(RuntimeState::derive(Online, badge, 0), S::Idle);
        }
    }

    #[test]
    fn wire_names_are_the_api_vocabulary() {
        let names = |v: serde_json::Value| v.to_string();
        assert_eq!(
            names(serde_json::json!([
                RuntimeState::Offline,
                RuntimeState::Unsupported,
                RuntimeState::NeedsLogin,
                RuntimeState::Limited,
                RuntimeState::Busy,
                RuntimeState::Idle
            ])),
            r#"["offline","unsupported","needs_login","limited","busy","idle"]"#
        );
        assert_eq!(
            names(serde_json::json!([
                Availability::Ready,
                Availability::NeedsLogin,
                Availability::Limited,
                Availability::Outdated,
                Availability::Unsupported,
                Availability::AuthUnknown
            ])),
            r#"["ready","needs_login","limited","outdated","unsupported","auth_unknown"]"#
        );
    }
}
