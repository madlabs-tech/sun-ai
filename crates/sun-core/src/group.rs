//! Groups (BLO-192): deployment-wide sets of people. A group grants nothing
//! by itself; it is a subject of editor grants
//! ([`crate::editor_grant::GrantSubject::Group`]), host sharing and the
//! [`TerminalPolicy`]. Unlike the groups removed in `d48f160`, it never
//! attaches a role to a workspace.
//!
//! Effective membership is the manual members ∪ the members whose
//! **verified** email has a domain in [`MemberGroup::domains`]
//! ([`is_member`]), computed on every request, never cached.

use serde::{Deserialize, Serialize};

use crate::{Error, GroupId, MemberId, Result, Timestamp};

/// `member_group` — a named set of people.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemberGroup {
    /// Id.
    pub id: GroupId,
    /// Unique, 1–100 characters.
    pub name: String,
    /// Free text, at most 1000 characters.
    pub description: String,
    /// Email domains whose verified members belong, lowercase
    /// ([`normalize_domains`]).
    pub domains: Vec<String>,
    /// Who created it (`None`: the token holder, or a removed member).
    pub created_by: Option<MemberId>,
    /// When.
    pub created_at: Timestamp,
}

impl MemberGroup {
    /// Most domains a group may list.
    pub const MAX_DOMAINS: usize = 50;
}

/// Checks a group's name and description: `InvalidInput` for an empty or
/// too long name, or a too long description. Both trimmed.
pub fn normalize_text(name: &str, description: &str) -> Result<(String, String)> {
    let (name, description) = (name.trim(), description.trim());
    if name.is_empty() || name.chars().count() > 100 {
        return Err(Error::InvalidInput(
            "A group name is 1 to 100 characters.".into(),
        ));
    }
    if description.chars().count() > 1000 {
        return Err(Error::InvalidInput(
            "A group description is at most 1000 characters.".into(),
        ));
    }
    Ok((name.to_owned(), description.to_owned()))
}

/// Checks and lowercases email domains (`madlabs.tech`): letters, digits,
/// `-` and `.`, at least two labels, none empty or edged with `-`, at most
/// 253 bytes; sorted, duplicates dropped, at most
/// [`MemberGroup::MAX_DOMAINS`]. `InvalidInput` names the first bad one.
pub fn normalize_domains(domains: &[String]) -> Result<Vec<String>> {
    let mut out = Vec::with_capacity(domains.len());
    for domain in domains {
        let domain = domain.trim().to_ascii_lowercase();
        let labels: Vec<&str> = domain.split('.').collect();
        let ok = domain.len() <= 253
            && labels.len() >= 2
            && labels.iter().all(|l| {
                !l.is_empty()
                    && l.len() <= 63
                    && !l.starts_with('-')
                    && !l.ends_with('-')
                    && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
            });
        if !ok {
            return Err(Error::InvalidInput(format!(
                "`{domain}` is not an email domain (like `madlabs.tech`)."
            )));
        }
        out.push(domain);
    }
    out.sort();
    out.dedup();
    if out.len() > MemberGroup::MAX_DOMAINS {
        return Err(Error::InvalidInput(format!(
            "At most {} domains.",
            MemberGroup::MAX_DOMAINS
        )));
    }
    Ok(out)
}

/// True when `email`'s domain (after its last `@`) is exactly one of
/// `domains`, ignoring case: `madlabs.tech` matches neither
/// `evilmadlabs.tech` nor `madlabs.tech.evil.com` nor `a.madlabs.tech`.
pub fn domain_matches(email: &str, domains: &[String]) -> bool {
    email
        .rsplit_once('@')
        .is_some_and(|(_, domain)| domains.iter().any(|d| d.eq_ignore_ascii_case(domain)))
}

/// The one membership rule: added by hand (`manual`), or a **verified**
/// `email` (`verified`) whose domain is in `group`'s domains.
pub fn is_member(group: &MemberGroup, manual: bool, email: Option<&str>, verified: bool) -> bool {
    manual || (verified && email.is_some_and(|e| domain_matches(e, &group.domains)))
}

/// Who may open a terminal in the editor besides holding `terminal` there:
/// the `terminal_policy` kernel setting, saved by a deployment owner.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TerminalPolicy {
    /// What it takes.
    #[serde(default)]
    pub mode: TerminalMode,
    /// The groups whose members may under [`TerminalMode::Groups`].
    #[serde(default)]
    pub groups: Vec<GroupId>,
}

/// [`TerminalPolicy::mode`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalMode {
    /// The editor grant (or owning the workspace) is enough.
    #[default]
    Granted,
    /// The grant and membership in one of [`TerminalPolicy::groups`];
    /// owning the workspace doesn't bypass it.
    Groups,
}

impl TerminalPolicy {
    /// The one rule: may someone who is a deployment owner (`is_owner`) and
    /// in `groups` open a terminal, the grant aside? A deployment owner
    /// always may; a workspace owner is no exception to the groups.
    pub fn allows(&self, is_owner: bool, groups: &[GroupId]) -> bool {
        is_owner
            || match self.mode {
                TerminalMode::Granted => true,
                TerminalMode::Groups => self.groups.iter().any(|g| groups.contains(g)),
            }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn group(domains: &[&str]) -> MemberGroup {
        MemberGroup {
            id: GroupId::generate(),
            name: "team".into(),
            description: String::new(),
            domains: domains.iter().map(|d| (*d).to_owned()).collect(),
            created_by: None,
            created_at: chrono::DateTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn a_domain_matches_exactly_and_ignoring_case_only() {
        let team = ["madlabs.tech".to_owned()];
        assert!(domain_matches("ana@madlabs.tech", &team));
        assert!(domain_matches("Ana@MadLabs.TECH", &team), "case");
        for other in [
            "ana@evilmadlabs.tech",
            "ana@madlabs.tech.evil.com",
            "ana@sub.madlabs.tech",
            "madlabs.tech",
            "ana@madlabs.tech@evil.com",
            "ana@",
        ] {
            assert!(!domain_matches(other, &team), "{other}");
        }
    }

    #[test]
    fn membership_is_manual_or_a_verified_email_on_a_domain() {
        let team = group(&["madlabs.tech"]);
        let at = Some("ana@madlabs.tech");
        assert!(is_member(&team, false, at, true), "verified domain");
        assert!(!is_member(&team, false, at, false), "unverified");
        assert!(!is_member(&team, false, None, true), "no email");
        assert!(!is_member(&team, false, Some("ana@gmail.com"), true));
        assert!(
            is_member(&team, true, Some("ana@gmail.com"), false),
            "manual"
        );
        assert!(is_member(&group(&[]), true, None, false));
        assert!(!is_member(&group(&[]), false, at, true));
    }

    #[test]
    fn domains_normalize_and_bad_ones_are_refused() {
        let given = [" MadLabs.Tech ".to_owned(), "madlabs.tech".to_owned()];
        assert_eq!(normalize_domains(&given).unwrap(), ["madlabs.tech"]);
        for bad in [
            "",
            "madlabs",
            "@madlabs.tech",
            "a@madlabs.tech",
            ".madlabs.tech",
            "madlabs..tech",
            "-madlabs.tech",
            "mad labs.tech",
            "*.madlabs.tech",
        ] {
            assert!(normalize_domains(&[bad.to_owned()]).is_err(), "{bad}");
        }
        let many: Vec<String> = (0..=MemberGroup::MAX_DOMAINS)
            .map(|i| format!("d{i}.test"))
            .collect();
        assert!(normalize_domains(&many).is_err());
        assert!(normalize_text(" ", "").is_err());
        assert_eq!(
            normalize_text(" Team ", " x ").unwrap(),
            ("Team".into(), "x".into())
        );
    }

    #[test]
    fn the_terminal_policy_lets_deployment_owners_and_listed_groups() {
        let (team, other) = (GroupId::generate(), GroupId::generate());
        let granted = TerminalPolicy::default();
        assert_eq!(granted.mode, TerminalMode::Granted);
        assert!(granted.allows(false, &[]), "today's behavior");
        let groups = TerminalPolicy {
            mode: TerminalMode::Groups,
            groups: vec![team],
        };
        assert!(groups.allows(true, &[]), "deployment owner");
        assert!(groups.allows(false, &[other, team]));
        assert!(!groups.allows(false, &[other]));
        assert!(!groups.allows(false, &[]));
        let parsed: TerminalPolicy =
            serde_json::from_value(serde_json::json!({"mode": "groups"})).unwrap();
        assert!(parsed.groups.is_empty() && !parsed.allows(false, &[team]));
        assert!(serde_json::from_value::<TerminalPolicy>(serde_json::json!({"x": 1})).is_err());
    }
}
