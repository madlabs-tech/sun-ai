//! Shared hosts, part of the **Facade** for the API: the hosts shared with
//! a member's groups, and the deployment owner's shares. Who may share is
//! the API's check; who may attach what is [`attach_path`]. A link made
//! through a share counts only while the share holds
//! (`sun_store::HostRepo::attach_host`), so an unshare or a member leaving a
//! group stops new placements on the host at once and running sessions
//! finish. Each change commits with its audit row and re-places waiting
//! runs.

use sun_core::model::{AuditAction, Host, HostShare};
use sun_core::{Error, GroupId, HostId, MemberId, Result, WorkspaceId};
use sun_store::{AuditRepo, GroupRepo, HostRepo, SettingRepo};

use crate::Engine;

/// `kernel_setting` key of the host `sun serve --with-host` runs inside
/// the kernel (a JSON host id). Its sessions run as the kernel's user, next
/// to its data dir and master key: sharing it needs an explicit
/// confirmation.
pub const KERNEL_HOST: &str = "kernel_host";

/// Why a member may attach a host to a workspace where they manage
/// runtimes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AttachPath {
    /// A deployment owner: any host.
    Owner,
    /// They enrolled it.
    Enrolled,
    /// It is shared with a group they are in; the link lasts as long as
    /// that holds.
    Share,
}

/// How `member` (a deployment owner when `is_owner`; `None`: the token
/// holder, an owner) may attach `host`, or `None` when they may not.
/// `shared` is [`HostRepo::hosts_shared_with`] for them, read once per
/// request.
pub fn attach_path(
    host: &Host,
    member: Option<MemberId>,
    is_owner: bool,
    shared: &[HostId],
) -> Option<AttachPath> {
    if is_owner {
        Some(AttachPath::Owner)
    } else if member.is_some() && host.enrolled_by == member {
        Some(AttachPath::Enrolled)
    } else if shared.contains(&host.id) {
        Some(AttachPath::Share)
    } else {
        None
    }
}

impl Engine {
    /// The host `sun serve --with-host` runs in the kernel, if any.
    pub async fn kernel_host(&self) -> Result<Option<HostId>> {
        let setting = self.store.read().await?.setting(KERNEL_HOST).await?;
        Ok(setting.and_then(|v| serde_json::from_value(v).ok()))
    }

    /// The hosts shared with a group `member` is in right now.
    pub async fn hosts_shared_with(&self, member: MemberId) -> Result<Vec<HostId>> {
        Ok(self.store.read().await?.hosts_shared_with(member).await?)
    }

    /// Every workspace `host` is linked to, a link whose share lapsed
    /// included (`sun host leave` removes them all).
    pub async fn host_links(&self, host: HostId) -> Result<Vec<WorkspaceId>> {
        Ok(self.store.read().await?.host_links(host).await?)
    }

    /// The groups `host` is shared with; `NotFound` for a missing host.
    pub async fn host_shares(&self, host: HostId) -> Result<Vec<HostShare>> {
        let mut read = self.store.read().await?;
        read.get_host(host).await?;
        Ok(read.host_shares(host).await?)
    }

    /// Shares `host` with `group` (a no-op when it already is), audited as
    /// `actor`. `NotFound` for a missing host or group.
    pub async fn share_host(
        &self,
        host: HostId,
        group: GroupId,
        by: Option<MemberId>,
        actor: &str,
    ) -> Result<()> {
        let mut tx = self.store.begin().await?;
        let host_name = tx.get_host(host).await?.name;
        let group_name = tx.get_group(group).await?.name;
        if tx.share_host(host, group, by).await? {
            let detail = format!("host {host_name} shared with group {group_name}");
            tx.append_audit(actor, AuditAction::SettingsChange, &host_name, &detail)
                .await?;
        }
        tx.commit().await?;
        // A link whose share had lapsed holds again.
        self.queue_wake.notify_one();
        Ok(())
    }

    /// Stops sharing `host` with `group`, audited as `actor`: workspaces
    /// that had it only through this share get no new runs on it; running
    /// sessions finish. `NotFound` when it wasn't shared with `group`.
    pub async fn unshare_host(&self, host: HostId, group: GroupId, actor: &str) -> Result<()> {
        let mut tx = self.store.begin().await?;
        let host_name = tx.get_host(host).await?.name;
        let group_name = tx.get_group(group).await?.name;
        if !tx.unshare_host(host, group).await? {
            return Err(Error::NotFound(format!(
                "host {host_name} is not shared with group {group_name}"
            )));
        }
        let detail = format!("host {host_name} no longer shared with group {group_name}");
        tx.append_audit(actor, AuditAction::SettingsChange, &host_name, &detail)
            .await?;
        tx.commit().await?;
        self.queue_wake.notify_one();
        Ok(())
    }
}
