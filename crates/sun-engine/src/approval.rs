//! Approvals: who may decide a `waiting_approval` step,
//! and what its `timeout:` does.
//!
//! A `waiting_approval` step moves only through [`crate::Engine::decide`] by
//! an authorized [`Principal`], or by its own timeout policy. Nothing else
//! (resume, a retried `start_run`, a `done` claim, a stray timer) moves it.

use std::time::Duration;

use sun_core::{Error, Result, StepKey, Timestamp};
use sun_workflow::{ApprovalTimeout, StepKind, TimeoutAction, Workflow};

/// Who decides an approval: an authenticated user and the roles the API
/// resolved for them (role bindings live outside the engine).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Principal {
    /// User name, matched against `user:<name>` in `who:`.
    pub user: String,
    /// Role names, matched against `role:<name>` in `who:`.
    pub roles: Vec<String>,
}

/// The `who:` entry that lets `by` decide approval step `step_key`, or an
/// error if the step is not an approval step or `by` is not listed.
pub(crate) fn authorize(workflow: &Workflow, step_key: &StepKey, by: &Principal) -> Result<String> {
    let who = workflow
        .steps
        .iter()
        .find(|d| d.id == *step_key)
        .and_then(|d| match &d.kind {
            // A decide step escalates to a human approval (`decision`).
            StepKind::Approval { who, .. } | StepKind::Decide { who, .. } => Some(who),
            // An agent step that failed over (`runtime.review_after_failover`).
            StepKind::Agent { .. } => workflow
                .runtime
                .as_ref()
                .map(|r| &r.review_after_failover)
                .filter(|who| !who.is_empty()),
            _ => None,
        })
        .ok_or_else(|| Error::InvalidInput(format!("`{step_key}` is not an approval step")))?;
    let user = format!("user:{}", by.user);
    who.iter()
        .find(|entry| {
            **entry == user
                || by
                    .roles
                    .iter()
                    .any(|role| **entry == format!("role:{role}"))
        })
        .cloned()
        .ok_or_else(|| {
            Error::Forbidden(format!(
                "`{}` may not decide `{step_key}`; allowed: {}",
                by.user,
                who.join(", ")
            ))
        })
}

/// What a fired timeout does to a `waiting_approval` step.
#[derive(Debug, PartialEq)]
pub(crate) enum OnTimeout {
    /// Journal a reminder and wait another `after`.
    Remind,
    /// `waiting_approval → rejected` with this reason.
    Reject(&'static str),
}

pub(crate) fn on_timeout(timeout: &ApprovalTimeout) -> OnTimeout {
    match timeout.then {
        TimeoutAction::Remind => OnTimeout::Remind,
        TimeoutAction::Fail => OnTimeout::Reject("approval timed out"),
        TimeoutAction::AutoReject => OnTimeout::Reject("auto-rejected: approval timed out"),
    }
}

/// Time until the timeout of an approval that started waiting at `since`
/// fires next. `remind` repeats every `after`, so it is the time to the next
/// multiple; the others fire once (now, if already overdue).
pub(crate) fn next_timeout(
    timeout: &ApprovalTimeout,
    since: Option<Timestamp>,
    now: Timestamp,
) -> Duration {
    let after = timeout.after_secs.max(1);
    let elapsed = since.map_or(0, |at| u64::try_from((now - at).num_seconds()).unwrap_or(0));
    Duration::from_secs(match timeout.then {
        TimeoutAction::Remind => after - elapsed % after,
        TimeoutAction::Fail | TimeoutAction::AutoReject => after.saturating_sub(elapsed),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const FLOW: &str = "
name: a
steps:
  - id: work
    type: wait
    for: 1s
  - id: review
    type: approval
    who: [role:admin, user:carol]
";

    fn principal(user: &str, roles: &[&str]) -> Principal {
        Principal {
            user: user.into(),
            roles: roles.iter().map(|r| (*r).to_owned()).collect(),
        }
    }

    #[test]
    fn only_listed_users_and_roles_decide() {
        let wf = sun_workflow::parse(FLOW).unwrap();
        let review = StepKey::new("review").unwrap();
        assert_eq!(
            authorize(&wf, &review, &principal("bob", &["admin"])).unwrap(),
            "role:admin"
        );
        assert_eq!(
            authorize(&wf, &review, &principal("carol", &[])).unwrap(),
            "user:carol"
        );
        assert!(authorize(&wf, &review, &principal("bob", &["dev"])).is_err());
        // `role:` and `user:` don't cross over.
        assert!(authorize(&wf, &review, &principal("admin", &["carol"])).is_err());
        let work = StepKey::new("work").unwrap();
        assert!(authorize(&wf, &work, &principal("bob", &["admin"])).is_err());
    }

    #[test]
    fn timeouts_fire_once_or_repeat() {
        let now = chrono::Utc::now();
        let since = Some(now - chrono::Duration::seconds(250));
        let t = |then| ApprovalTimeout {
            after_secs: 100,
            then,
        };
        let remind = t(TimeoutAction::Remind);
        assert_eq!(next_timeout(&remind, since, now), Duration::from_secs(50));
        assert_eq!(next_timeout(&remind, None, now), Duration::from_secs(100));
        assert_eq!(
            next_timeout(&t(TimeoutAction::Fail), since, now),
            Duration::ZERO
        );
        assert_eq!(on_timeout(&remind), OnTimeout::Remind);
        assert!(matches!(
            on_timeout(&t(TimeoutAction::AutoReject)),
            OnTimeout::Reject(_)
        ));
    }
}
