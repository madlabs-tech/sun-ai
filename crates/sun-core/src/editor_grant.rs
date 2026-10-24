//! Editor grants: who may read, write and open a terminal on a workspace's
//! repos in the editor. Plain data and the one resolution rule
//! ([`capabilities_of`]); `sun-store` keeps the `editor_grant` rows and
//! `sun-api` enforces them on every editor and terminal route.
//!
//! - Deployment owners and a workspace's owners hold every capability,
//!   always: implicit, never stored, never revoked. The one exception is
//!   the [`TerminalPolicy`]: under `groups`, a workspace owner outside the
//!   listed groups has no `terminal` (a deployment owner always does).
//! - Everyone else holds what a grant gives them: to all members, to a role
//!   (that role and those above it, as approval `role:` keys), to a group
//!   ([`crate::group`]) or to one member; for the whole workspace or one
//!   repo.
//! - `editor.write` implies `editor.read`; `terminal` is never implied (it
//!   is arbitrary code execution on the host).
//! - A viewer holds `editor.read` at most, whatever was granted.

use serde::{Deserialize, Serialize};

use crate::group::TerminalPolicy;
use crate::model::Role;
use crate::{EditorGrantId, Error, GroupId, MemberId, RepoId, Result, Timestamp, WorkspaceId};

string_enum! {
    /// One thing the editor lets someone do on a repo.
    pub enum EditorCapability {
        /// List, read and quick-open files.
        Read => "editor.read",
        /// Write, create, rename and delete files (implies [`Self::Read`]).
        Write => "editor.write",
        /// Open a terminal in the editor worktree. Never implied.
        Terminal => "terminal",
    }
}

/// Who a grant is for: `{"subject_type": "all" | "role" | "group" |
/// "member", "subject_id": <role key | group id | member id>}`
/// (`editor_grant.subject_type`, `subject_id`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "subject_type",
    content = "subject_id",
    rename_all = "snake_case"
)]
pub enum GrantSubject {
    /// Every member of the workspace.
    All,
    /// Members with this role or one above it.
    Role(Role),
    /// The members of a group ([`crate::group::is_member`]).
    Group(GroupId),
    /// One member.
    Member(MemberId),
}

impl GrantSubject {
    /// `(subject_type, subject_id)` as stored.
    pub fn to_columns(self) -> (&'static str, Option<String>) {
        match self {
            Self::All => ("all", None),
            Self::Role(role) => ("role", Some(role.as_str().to_owned())),
            Self::Group(id) => ("group", Some(id.to_string())),
            Self::Member(id) => ("member", Some(id.to_string())),
        }
    }

    /// Reads the stored columns back; `InvalidInput` for anything else.
    pub fn from_columns(subject_type: &str, subject_id: Option<&str>) -> Result<Self> {
        match (subject_type, subject_id) {
            ("all", None) => Ok(Self::All),
            ("role", Some(role)) => Ok(Self::Role(role.parse()?)),
            ("group", Some(id)) => Ok(Self::Group(id.parse()?)),
            ("member", Some(id)) => Ok(Self::Member(id.parse()?)),
            (kind, _) => Err(Error::InvalidInput(format!("grant subject `{kind}`"))),
        }
    }

    /// True when it names someone who is `member` with `role` there and in
    /// `groups`.
    fn covers(self, member: Option<MemberId>, role: Role, groups: &[GroupId]) -> bool {
        match self {
            Self::All => true,
            Self::Role(granted) => role.covers(granted),
            Self::Group(id) => groups.contains(&id),
            Self::Member(id) => member == Some(id),
        }
    }
}

/// `editor_grant` — one capability given to a subject in a workspace, on
/// every repo there (`repo_id` `None`) or one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditorGrant {
    /// Id.
    pub id: EditorGrantId,
    /// The workspace.
    pub workspace_id: WorkspaceId,
    /// The one repo it applies to; `None`: every repo of the workspace.
    pub repo_id: Option<RepoId>,
    /// Who holds it.
    #[serde(flatten)]
    pub subject: GrantSubject,
    /// What it gives.
    pub capability: EditorCapability,
    /// Who granted it (`None`: the token holder, or a removed member).
    pub granted_by: Option<MemberId>,
    /// When.
    pub created_at: Timestamp,
}

/// `InvalidInput` when a viewer (the role, or a member whose role in the
/// workspace is `role`) is given more than [`EditorCapability::Read`].
pub fn check_viewer_cap(role: Role, capability: EditorCapability) -> Result<()> {
    if role == Role::Viewer && capability != EditorCapability::Read {
        return Err(Error::InvalidInput(format!(
            "Viewers can be granted `editor.read` only, not `{capability}`."
        )));
    }
    Ok(())
}

/// What someone holds on `repo`: every capability for a deployment owner
/// (`is_owner`); nothing for a non-member (`role` `None`); every capability
/// for the workspace's owner, else what the `grants` of the workspace give
/// `member` with `role` there and in `groups` ([`crate::group`]) on `repo`,
/// with write implying read and capped at read for a viewer; and
/// `terminal` only when the `terminal` policy allows it too. In
/// [`EditorCapability::ALL`] order.
pub fn capabilities_of(
    is_owner: bool,
    member: Option<MemberId>,
    role: Option<Role>,
    groups: &[GroupId],
    terminal: &TerminalPolicy,
    repo: RepoId,
    grants: &[EditorGrant],
) -> Vec<EditorCapability> {
    if is_owner {
        return EditorCapability::ALL.to_vec();
    }
    let Some(role) = role else {
        return Vec::new();
    };
    let held: Vec<EditorCapability> = match role {
        Role::Owner => EditorCapability::ALL.to_vec(),
        _ => grants
            .iter()
            .filter(|g| {
                g.repo_id.is_none_or(|r| r == repo) && g.subject.covers(member, role, groups)
            })
            .map(|g| g.capability)
            .collect(),
    };
    EditorCapability::ALL
        .iter()
        .copied()
        .filter(|cap| match cap {
            EditorCapability::Read => {
                held.contains(&EditorCapability::Read) || held.contains(&EditorCapability::Write)
            }
            other => held.contains(other),
        })
        .filter(|cap| check_viewer_cap(role, *cap).is_ok())
        .filter(|cap| *cap != EditorCapability::Terminal || terminal.allows(false, groups))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::group::TerminalMode;
    use EditorCapability::{Read, Terminal, Write};

    fn grant(subject: GrantSubject, capability: EditorCapability) -> EditorGrant {
        EditorGrant {
            id: EditorGrantId::generate(),
            workspace_id: WorkspaceId::MAIN,
            repo_id: None,
            subject,
            capability,
            granted_by: None,
            created_at: chrono::DateTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn owners_hold_everything_without_a_grant_and_others_nothing() {
        let (ana, repo) = (Some(MemberId::generate()), RepoId::generate());
        let all = vec![Read, Write, Terminal];
        assert_eq!(
            capabilities_of(true, None, None, &[], &TerminalPolicy::default(), repo, &[]),
            all
        );
        assert_eq!(
            capabilities_of(
                true,
                ana,
                Some(Role::Viewer),
                &[],
                &TerminalPolicy::default(),
                repo,
                &[]
            ),
            all
        );
        assert_eq!(
            capabilities_of(
                false,
                ana,
                Some(Role::Owner),
                &[],
                &TerminalPolicy::default(),
                repo,
                &[]
            ),
            all
        );
        for role in [Role::Admin, Role::Member, Role::Viewer] {
            assert!(
                capabilities_of(
                    false,
                    ana,
                    Some(role),
                    &[],
                    &TerminalPolicy::default(),
                    repo,
                    &[]
                )
                .is_empty()
            );
        }
        let everyone = [grant(GrantSubject::All, Terminal)];
        assert!(
            capabilities_of(
                false,
                ana,
                None,
                &[],
                &TerminalPolicy::default(),
                repo,
                &everyone
            )
            .is_empty()
        );
    }

    #[test]
    fn grants_to_all_a_role_or_a_member_resolve_with_write_implying_read_only() {
        let (ana, bob, repo) = (
            MemberId::generate(),
            MemberId::generate(),
            RepoId::generate(),
        );
        let of = |member, role, grants: &[EditorGrant]| {
            capabilities_of(
                false,
                Some(member),
                Some(role),
                &[],
                &TerminalPolicy::default(),
                repo,
                grants,
            )
        };
        let to_all = [grant(GrantSubject::All, Read)];
        assert_eq!(of(ana, Role::Member, &to_all), vec![Read]);
        assert_eq!(of(ana, Role::Viewer, &to_all), vec![Read]);

        let to_members = [grant(GrantSubject::Role(Role::Member), Write)];
        assert_eq!(of(ana, Role::Member, &to_members), vec![Read, Write]);
        assert_eq!(
            of(ana, Role::Admin, &to_members),
            vec![Read, Write],
            "above"
        );
        assert!(of(ana, Role::Viewer, &to_members).is_empty(), "below");

        let to_ana = [grant(GrantSubject::Member(ana), Terminal)];
        assert_eq!(of(ana, Role::Member, &to_ana), vec![Terminal], "no read");
        assert!(of(bob, Role::Member, &to_ana).is_empty());
    }

    #[test]
    fn a_repo_grant_covers_only_its_repo_and_a_workspace_grant_every_repo() {
        let (ana, here, there) = (MemberId::generate(), RepoId::generate(), RepoId::generate());
        let scoped = EditorGrant {
            repo_id: Some(here),
            ..grant(GrantSubject::Member(ana), Write)
        };
        let grants = [scoped, grant(GrantSubject::All, Read)];
        let on = |repo| {
            capabilities_of(
                false,
                Some(ana),
                Some(Role::Member),
                &[],
                &TerminalPolicy::default(),
                repo,
                &grants,
            )
        };
        assert_eq!(on(here), vec![Read, Write]);
        assert_eq!(on(there), vec![Read]);
    }

    #[test]
    fn a_viewer_holds_read_at_most_and_the_api_refuses_more() {
        let (ana, repo) = (MemberId::generate(), RepoId::generate());
        let grants = [
            grant(GrantSubject::All, Write),
            grant(GrantSubject::Member(ana), Terminal),
        ];
        let held = capabilities_of(
            false,
            Some(ana),
            Some(Role::Viewer),
            &[],
            &TerminalPolicy::default(),
            repo,
            &grants,
        );
        assert_eq!(held, vec![Read]);
        assert!(check_viewer_cap(Role::Viewer, Read).is_ok());
        assert!(check_viewer_cap(Role::Viewer, Write).is_err());
        assert!(check_viewer_cap(Role::Viewer, Terminal).is_err());
        assert!(check_viewer_cap(Role::Member, Terminal).is_ok());
    }

    #[test]
    fn a_revoked_grant_is_gone_on_the_next_resolution() {
        let (ana, repo) = (MemberId::generate(), RepoId::generate());
        let mut grants = vec![grant(GrantSubject::Member(ana), Write)];
        let of = |grants: &[EditorGrant]| {
            capabilities_of(
                false,
                Some(ana),
                Some(Role::Member),
                &[],
                &TerminalPolicy::default(),
                repo,
                grants,
            )
        };
        assert_eq!(of(&grants), vec![Read, Write]);
        grants.clear();
        assert!(of(&grants).is_empty());
    }

    #[test]
    fn a_group_grant_covers_its_members_only() {
        let (ana, repo, team) = (
            MemberId::generate(),
            RepoId::generate(),
            GroupId::generate(),
        );
        let grants = [grant(GrantSubject::Group(team), Write)];
        let policy = TerminalPolicy::default();
        let of = |groups: &[GroupId]| {
            capabilities_of(
                false,
                Some(ana),
                Some(Role::Member),
                groups,
                &policy,
                repo,
                &grants,
            )
        };
        assert_eq!(of(&[team]), vec![Read, Write]);
        assert!(of(&[GroupId::generate()]).is_empty());
        assert!(
            of(&[]).is_empty(),
            "left the group: gone on the next resolution"
        );
    }

    #[test]
    fn the_groups_terminal_policy_refuses_workspace_owners_outside_its_groups() {
        let (ana, repo, team) = (
            MemberId::generate(),
            RepoId::generate(),
            GroupId::generate(),
        );
        let policy = TerminalPolicy {
            mode: TerminalMode::Groups,
            groups: vec![team],
        };
        let grants = [grant(GrantSubject::All, Terminal)];
        let of = |is_owner, role, groups: &[GroupId]| {
            capabilities_of(is_owner, Some(ana), role, groups, &policy, repo, &grants)
        };
        // A public sign-up owns their own workspace: no terminal.
        assert_eq!(of(false, Some(Role::Owner), &[]), vec![Read, Write]);
        assert_eq!(
            of(false, Some(Role::Owner), &[team]),
            vec![Read, Write, Terminal]
        );
        // A team member needs the grant as well as the group.
        assert_eq!(of(false, Some(Role::Member), &[team]), vec![Terminal]);
        assert!(of(false, Some(Role::Member), &[]).is_empty());
        let ungranted = capabilities_of(
            false,
            Some(ana),
            Some(Role::Member),
            &[team],
            &policy,
            repo,
            &[],
        );
        assert!(ungranted.is_empty());
        // The deployment owner always passes.
        assert_eq!(of(true, None, &[]), vec![Read, Write, Terminal]);
    }

    #[test]
    fn subjects_round_trip_their_columns_and_json() {
        let ana = MemberId::generate();
        for subject in [
            GrantSubject::All,
            GrantSubject::Role(Role::Viewer),
            GrantSubject::Group(GroupId::generate()),
            GrantSubject::Member(ana),
        ] {
            let (kind, id) = subject.to_columns();
            assert_eq!(
                GrantSubject::from_columns(kind, id.as_deref()).unwrap(),
                subject
            );
        }
        assert!(GrantSubject::from_columns("group", Some("x")).is_err());
        assert!(GrantSubject::from_columns("all", Some("x")).is_err());
        let json = serde_json::to_value(GrantSubject::Role(Role::Member)).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"subject_type": "role", "subject_id": "member"})
        );
        let all: GrantSubject =
            serde_json::from_value(serde_json::json!({"subject_type": "all"})).unwrap();
        assert_eq!(all, GrantSubject::All);
        assert_eq!("editor.write".parse::<EditorCapability>().unwrap(), Write);
    }
}
