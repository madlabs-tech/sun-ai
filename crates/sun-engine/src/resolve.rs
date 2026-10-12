//! A human decides an `unknown` side effect. Child module of
//! `executor`.
//!
//! A delivery that timed out is never sent again on its own (`outbox`); its
//! step waits. A human who checked the far side says what happened:
//! - `delivered`: it did happen. Recorded like a delivery
//!   (`outbox_delivered` with `{resolved_by}`), so the step moves on and
//!   nothing is sent.
//! - `resend`: it did not. The row goes back to `pending` once and the
//!   worker delivers it again under the same idem key (`outbox_resolved`).
//!
//! Either way the row must still be `unknown` and not being delivered right
//! now (`outbox::InFlight`), so a second decision (or a double click) is
//! `InvalidInput` and never a second send.

use serde::{Deserialize, Serialize};
use serde_json::json;
use sun_core::model::{OutboxItem, OutboxStatus};
use sun_core::{Error, OutboxId, Result};
use sun_store::{EventLog, NewEvent, OutboxRepo};

use super::{Command, Effect, Engine, Executor};
use crate::outbox::{Outcome, Settlement, in_flight};

/// A human's answer for an `unknown` outbox row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Resolution {
    /// The effect happened: record it, send nothing.
    Delivered,
    /// The effect did not happen: send it once more.
    Resend,
}

/// Event kind: a human decided an `unknown` row; payload
/// `{outbox_id, idem_key, target, resolution, by}`.
const OUTBOX_RESOLVED: &str = "outbox_resolved";

impl Engine {
    /// Outbox rows in `status`, oldest first (at most 500).
    pub async fn outbox(&self, status: OutboxStatus) -> Result<Vec<OutboxItem>> {
        Ok(self
            .store
            .read()
            .await?
            .outbox_with_status(status, 500)
            .await?)
    }

    /// Decides the `unknown` row `id` as user `by`. `InvalidInput` unless
    /// the row is `unknown`.
    pub async fn resolve_outbox(
        &self,
        id: OutboxId,
        resolution: Resolution,
        by: String,
    ) -> Result<()> {
        self.request(|reply| Command::ResolveOutbox {
            id,
            resolution,
            by,
            reply,
        })
        .await
    }
}

impl Executor {
    pub(super) async fn resolve_outbox(
        &mut self,
        id: OutboxId,
        resolution: Resolution,
        by: String,
    ) -> Result<()> {
        let mut tx = self.store.begin().await?;
        // ponytail: scans the unknown rows; they are few and wait on humans.
        let item = tx
            .outbox_with_status(OutboxStatus::Unknown, u32::MAX)
            .await?
            .into_iter()
            .find(|row| row.id == id && !in_flight(&self.in_flight).contains(&id))
            .ok_or_else(|| {
                Error::InvalidInput(format!("outbox item {id} is not waiting for a decision"))
            })?;
        if resolution == Resolution::Delivered {
            drop(tx);
            // Recorded like a delivery, in one transaction with the step's move.
            let outcome = Outcome::Delivered(json!({ "resolved_by": by }));
            return self.on_settled(Settlement { item, outcome }).await;
        }
        if let Some(run_id) = item.run_id {
            tx.append_event(NewEvent::new(
                run_id,
                None,
                OUTBOX_RESOLVED,
                json!({
                    "outbox_id": item.id,
                    "idem_key": item.idem_key,
                    "target": item.target,
                    "resolution": resolution,
                    "by": by,
                }),
            ))
            .await?;
        }
        tx.set_outbox_status(id, OutboxStatus::Pending, None)
            .await?;
        tx.commit().await?;
        self.apply(vec![Effect::WakeOutbox]).await
    }
}
