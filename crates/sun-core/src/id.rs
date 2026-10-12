//! Identifiers. All entity ids are ULIDs (sortable, generated without the DB).

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use ulid::Ulid;

use crate::Error;

macro_rules! ulid_id {
    ($($(#[$meta:meta])* $name:ident),+ $(,)?) => {$(
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub Ulid);

        impl $name {
            /// A new, time-ordered id.
            pub fn generate() -> Self {
                Self(Ulid::generate())
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(&self.0, f)
            }
        }

        impl FromStr for $name {
            type Err = Error;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Ulid::from_string(s).map(Self).map_err(|e| {
                    Error::InvalidInput(format!("{} `{}`: {}", stringify!($name), s, e))
                })
            }
        }
    )+};
}

ulid_id!(
    /// The ecosystem (one per kernel); secrets use the nil ULID.
    EcosystemId,
    /// `workspace.id` — a workspace owns workflows and, through them, tasks.
    WorkspaceId,
    /// `repo.id`
    RepoId,
    /// `workflow.id`
    WorkflowId,
    /// `task.id`
    TaskId,
    /// `run.id`
    RunId,
    /// `step.id` — one per step attempt.
    StepId,
    /// `outbox.id`
    OutboxId,
    /// `runtime.id`
    RuntimeId,
    /// `host.id`
    HostId,
    /// `session.id` — Sun's id for one agent session on a host.
    SessionId,
    /// `handoff_pack.id`
    HandoffPackId,
    /// `context_pack.id`
    ContextPackId,
    /// `note.id`
    NoteId,
    /// `memory_item.id`
    MemoryItemId,
    /// `correction.id`
    CorrectionId,
    /// `artifact.id`
    ArtifactId,
    /// `secret.id`
    SecretId,
    /// `member.id` — a person who signed in.
    MemberId,
    /// `user_group.id` — a named, editable set of permissions.
    GroupId,
    /// `audit.id`
    AuditId,
    /// `member_factor.id` — a member's second sign-in step.
    FactorId,
    /// `pin.id` — a design-canvas pin.
    PinId,
    /// `eval_set.id` — saved past tasks to replay.
    EvalSetId,
    /// `eval_run.id` — one replay of past tasks.
    EvalRunId,
    /// `project.id` — a named group of a workspace's tasks.
    ProjectId,
    /// `vcs_connection.id` — a workspace's GitHub or GitLab connection.
    VcsConnectionId,
    /// `task_activity.id` — one entry of a task's activity log.
    TaskActivityId,
    /// `agent.id` — an ecosystem-wide agent a task can be assigned to.
    AgentId,
    /// `pull_request.id` — a pull request mirrored from a git host.
    PullRequestId,
);

impl WorkspaceId {
    /// Workspace "Main": the migration creates it and puts every older workflow
    /// on it; workflows created without a workspace land here. It can't be deleted.
    pub const MAIN: Self = Self(Ulid(1));
}

impl GroupId {
    /// Group "Viewer" (key `viewer`), seeded by the groups migration.
    pub const VIEWER: Self = Self(Ulid(1));
    /// Group "Member" (key `member`), seeded by the groups migration.
    pub const MEMBER: Self = Self(Ulid(2));
    /// Group "Admin" (key `admin`), seeded by the groups migration.
    pub const ADMIN: Self = Self(Ulid(3));
}

/// A step's `id` from the workflow file (e.g. `implement`). Unique within a workflow.
///
/// Valid: 1–64 chars, starts with `a-z`, then `a-z`, `0-9`, `_` or `-`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct StepKey(String);

impl StepKey {
    /// Validates and wraps a step key.
    pub fn new(key: impl Into<String>) -> Result<Self, Error> {
        let key = key.into();
        let mut chars = key.chars();
        let valid = key.len() <= 64
            && chars.next().is_some_and(|c| c.is_ascii_lowercase())
            && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-');
        if valid {
            Ok(Self(key))
        } else {
            Err(Error::InvalidInput(format!("step key `{key}`")))
        }
    }

    /// The key as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for StepKey {
    type Error = Error;

    fn try_from(key: String) -> Result<Self, Self::Error> {
        Self::new(key)
    }
}

impl From<StepKey> for String {
    fn from(key: StepKey) -> Self {
        key.0
    }
}

impl fmt::Display for StepKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Content hash of a blob in the data dir: 64 lowercase hex chars.
/// The hash algorithm is owned by `sun-store`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct BlobHash(String);

impl BlobHash {
    /// Validates and wraps a hex digest.
    pub fn new(hex: impl Into<String>) -> Result<Self, Error> {
        let hex = hex.into();
        if hex.len() == 64 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
            Ok(Self(hex))
        } else {
            Err(Error::InvalidInput(format!("blob hash `{hex}`")))
        }
    }

    /// The digest as hex.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for BlobHash {
    type Error = Error;

    fn try_from(hex: String) -> Result<Self, Self::Error> {
        Self::new(hex)
    }
}

impl From<BlobHash> for String {
    fn from(hash: BlobHash) -> Self {
        hash.0
    }
}

/// Idempotency key of an outbox side effect (`outbox.idem_key`).
///
/// Built as `{run_id}:{step_key}:{action_key}` — deterministic and unique
/// because neither a ULID nor a [`StepKey`] contains `:`. It is the same for
/// every attempt of a step, so a retry can never duplicate the side effect.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct IdemKey(String);

impl IdemKey {
    /// The key for `action_key` (e.g. `github.open_pr`) of a step in a run.
    pub fn new(run_id: RunId, step_key: &StepKey, action_key: &str) -> Self {
        Self(format!("{run_id}:{step_key}:{action_key}"))
    }

    /// The key of a side effect outside any run: `{scope}:{action_key}`
    /// (e.g. `trigger_failed:<audit id>:slack:#eng`). `scope` names the kind
    /// and must not be a ULID, so it never collides with a run's keys.
    pub fn outside_run(scope: &str, action_key: &str) -> Self {
        Self(format!("{scope}:{action_key}"))
    }

    /// The key as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ulid_ids_round_trip_through_text_and_json() {
        let id = RunId::generate();
        assert_eq!(id.to_string().parse::<RunId>().unwrap(), id);
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, format!("\"{id}\""));
        assert_eq!(serde_json::from_str::<RunId>(&json).unwrap(), id);
        assert!("not-a-ulid".parse::<RunId>().is_err());
    }

    #[test]
    fn step_key_validation() {
        for ok in ["implement", "a", "fix_bug-2"] {
            assert!(StepKey::new(ok).is_ok(), "{ok}");
        }
        for bad in ["", "Implement", "2fix", "a:b", "a b", &"a".repeat(65)] {
            assert!(StepKey::new(bad).is_err(), "{bad}");
        }
        assert!(serde_json::from_str::<StepKey>("\"Bad\"").is_err());
    }

    #[test]
    fn blob_hash_validation() {
        assert!(BlobHash::new("a".repeat(64)).is_ok());
        assert!(BlobHash::new("A".repeat(64)).is_err());
        assert!(BlobHash::new("a".repeat(63)).is_err());
    }

    #[test]
    fn idem_key_is_stable_per_run_step_action() {
        let run = RunId::generate();
        let step = StepKey::new("pr").unwrap();
        assert_eq!(
            IdemKey::new(run, &step, "github.open_pr"),
            IdemKey::new(run, &step, "github.open_pr")
        );
        assert_eq!(
            IdemKey::new(run, &step, "x").as_str(),
            format!("{run}:pr:x")
        );
    }
}
