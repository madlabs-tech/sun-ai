//! Groups and the terminal policy (BLO-192), part of the **Facade** for the
//! API: deployment-wide sets of people ([`sun_core::group`]) with their
//! manual members, who a set of domains would bring in, and the
//! [`TerminalPolicy`] kernel setting. Each change commits with its
//! `settings_change` audit row. Who is in a group is
//! [`sun_store::GroupRepo::groups_of`], read per request.

use serde_json::json;
use sun_core::group::{
    MemberGroup, TerminalPolicy, domain_matches, normalize_domains, normalize_text,
};
use sun_core::model::{AuditAction, Member};
use sun_core::{Error, GroupId, MemberId, Result};
use sun_store::{AuditRepo, GroupRepo, MemberRepo, SettingRepo, Tx};

use crate::Engine;

/// `kernel_setting` key of the [`TerminalPolicy`].
const TERMINAL: &str = "terminal_policy";

/// A change to a group, as its audit row.
async fn audit(tx: &mut Tx, actor: &str, group: &MemberGroup, detail: &str) -> Result<()> {
    let target = format!("group:{}", group.name);
    tx.append_audit(actor, AuditAction::SettingsChange, &target, detail)
        .await?;
    Ok(())
}

/// The saved terminal policy (`granted` until saved).
async fn terminal_of(tx: &mut Tx) -> Result<TerminalPolicy> {
    match tx.setting(TERMINAL).await? {
        Some(v) => serde_json::from_value(v).map_err(|e| Error::InvalidInput(e.to_string())),
        None => Ok(TerminalPolicy::default()),
    }
}

/// Saves `policy`, audited as `actor`.
async fn save_terminal(tx: &mut Tx, policy: &TerminalPolicy, actor: &str) -> Result<()> {
    tx.put_setting(TERMINAL, &json!(policy)).await?;
    let detail = format!(
        "terminal: {} ({} group(s) listed)",
        json!(policy.mode).as_str().unwrap_or("?"),
        policy.groups.len()
    );
    tx.append_audit(actor, AuditAction::SettingsChange, TERMINAL, &detail)
        .await?;
    Ok(())
}

/// A group's edit: each field `None` is left as is.
#[derive(Debug, Default)]
pub struct GroupChange {
    /// New name.
    pub name: Option<String>,
    /// New description.
    pub description: Option<String>,
    /// New domains.
    pub domains: Option<Vec<String>>,
}

impl Engine {
    /// Every group with its manual members, by name.
    pub async fn groups(&self) -> Result<Vec<(MemberGroup, Vec<MemberId>)>> {
        let mut tx = self.store.read().await?;
        let mut out = Vec::new();
        for group in tx.groups().await? {
            let members = tx.group_member_ids(group.id).await?;
            out.push((group, members));
        }
        Ok(out)
    }

    /// Group `id` with its manual members; `NotFound` when there is none.
    pub async fn group(&self, id: GroupId) -> Result<(MemberGroup, Vec<MemberId>)> {
        let mut tx = self.store.read().await?;
        let group = tx.get_group(id).await?;
        let members = tx.group_member_ids(id).await?;
        Ok((group, members))
    }

    /// The groups `member` is in now (never cached).
    pub async fn groups_of(&self, member: MemberId) -> Result<Vec<MemberGroup>> {
        Ok(self.store.read().await?.groups_of(member).await?)
    }

    /// Creates a group, audited as `actor`. `InvalidInput` for a bad or
    /// taken name, a long description or a bad domain.
    pub async fn create_group(
        &self,
        name: &str,
        description: &str,
        domains: &[String],
        created_by: Option<MemberId>,
        actor: &str,
    ) -> Result<MemberGroup> {
        let (name, description) = normalize_text(name, description)?;
        let group = MemberGroup {
            id: GroupId::generate(),
            name,
            description,
            domains: normalize_domains(domains)?,
            created_by,
            created_at: chrono::Utc::now(),
        };
        let mut tx = self.store.begin().await?;
        tx.insert_group(&group).await?;
        let detail = format!("group created (domains: {})", group.domains.join(", "));
        audit(&mut tx, actor, &group, &detail).await?;
        tx.commit().await?;
        Ok(group)
    }

    /// Edits group `id`, audited as `actor`; `NotFound` when there is none.
    pub async fn update_group(
        &self,
        id: GroupId,
        change: GroupChange,
        actor: &str,
    ) -> Result<MemberGroup> {
        let domains = change
            .domains
            .as_deref()
            .map(normalize_domains)
            .transpose()?;
        let mut tx = self.store.begin().await?;
        let mut group = tx.get_group(id).await?;
        let before = group.name.clone();
        let (name, description) = normalize_text(
            change.name.as_deref().unwrap_or(&group.name),
            change.description.as_deref().unwrap_or(&group.description),
        )?;
        group.name = name;
        group.description = description;
        if let Some(domains) = domains {
            group.domains = domains;
        }
        tx.update_group(&group).await?;
        let detail = format!(
            "group edited (was `{before}`; domains: {})",
            group.domains.join(", ")
        );
        audit(&mut tx, actor, &group, &detail).await?;
        tx.commit().await?;
        Ok(group)
    }

    /// Deletes group `id`, audited as `actor`: its manual members and its
    /// editor grants go with it, and the terminal policy stops listing it
    /// (in the same transaction). `NotFound` when there is none.
    pub async fn delete_group(&self, id: GroupId, actor: &str) -> Result<MemberGroup> {
        let mut tx = self.store.begin().await?;
        let group = tx.delete_group(id).await?;
        audit(&mut tx, actor, &group, "group deleted").await?;
        let mut policy = terminal_of(&mut tx).await?;
        if policy.groups.contains(&id) {
            policy.groups.retain(|g| *g != id);
            save_terminal(&mut tx, &policy, actor).await?;
        }
        tx.commit().await?;
        Ok(group)
    }

    /// Adds `member` to group `id` by hand, audited as `actor` when they
    /// weren't in it. `NotFound` for an unknown group or member.
    pub async fn add_group_member(
        &self,
        id: GroupId,
        member: MemberId,
        added_by: Option<MemberId>,
        actor: &str,
    ) -> Result<()> {
        let mut tx = self.store.begin().await?;
        let group = tx.get_group(id).await?;
        let who = tx.get_member(member).await?;
        if tx.add_group_member(id, member, added_by).await? {
            let detail = format!("member {} added", who.username);
            audit(&mut tx, actor, &group, &detail).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Removes `member` from group `id`'s manual members, audited as
    /// `actor`; it applies to their next request. `NotFound` for an unknown
    /// group, or a member who wasn't added by hand.
    pub async fn remove_group_member(
        &self,
        id: GroupId,
        member: MemberId,
        actor: &str,
    ) -> Result<()> {
        let mut tx = self.store.begin().await?;
        let group = tx.get_group(id).await?;
        if !tx.remove_group_member(id, member).await? {
            return Err(Error::NotFound(format!("group member {member}")));
        }
        let who = tx.get_member(member).await?;
        let detail = format!("member {} removed", who.username);
        audit(&mut tx, actor, &group, &detail).await?;
        tx.commit().await?;
        Ok(())
    }

    /// The active members whose verified email is on one of `domains`
    /// (normalized; `InvalidInput` for a bad one), by id: who a group with
    /// those domains would bring in.
    pub async fn group_preview(&self, domains: &[String]) -> Result<Vec<Member>> {
        let domains = normalize_domains(domains)?;
        // ponytail: scans every member; a domain column when that's slow.
        let members = self.store.read().await?.members().await?;
        Ok(members
            .into_iter()
            .filter(|m| {
                m.deactivated_at.is_none()
                    && m.email_verified_at.is_some()
                    && m.email
                        .as_deref()
                        .is_some_and(|e| domain_matches(e, &domains))
            })
            .collect())
    }

    /// The deployment's terminal policy (`granted` until saved).
    pub async fn terminal_policy(&self) -> Result<TerminalPolicy> {
        terminal_of(&mut self.store.read().await?).await
    }

    /// Saves the terminal policy, audited as `actor`; every listed group
    /// must exist (`groups` with none listed: deployment owners only). The
    /// saved policy, its group ids deduplicated.
    pub async fn set_terminal_policy(
        &self,
        mut policy: TerminalPolicy,
        actor: &str,
    ) -> Result<TerminalPolicy> {
        policy.groups.sort_by_key(ToString::to_string);
        policy.groups.dedup();
        let mut tx = self.store.begin().await?;
        for id in &policy.groups {
            tx.get_group(*id).await?;
        }
        save_terminal(&mut tx, &policy, actor).await?;
        tx.commit().await?;
        Ok(policy)
    }

    /// What the editor's terminal rule needs about `member`, read now in
    /// one snapshot: their groups (none for the token holder) and the
    /// terminal policy.
    pub async fn terminal_access(
        &self,
        member: Option<MemberId>,
    ) -> Result<(Vec<GroupId>, TerminalPolicy)> {
        let mut tx = self.store.read().await?;
        let groups = match member {
            Some(id) => tx.groups_of(id).await?.iter().map(|g| g.id).collect(),
            None => Vec::new(),
        };
        Ok((groups, terminal_of(&mut tx).await?))
    }
}
