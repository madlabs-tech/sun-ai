//! Context rotation: the CLI owns compaction; Sun measures
//! and hands off before it gets lossy.
//!
//! A child module of `executor`, like `failover`, whose hand-off it reuses
//! (reason `context_rotation`) — there is no second hand-off path.
//!
//! - Every `Usage.context_used` of a running agent step's session is compared
//!   with its window (`WatchdogConfig::context_windows`, filled from
//!   `RuntimeAdapter::context_window`). At `rotate_at_percent` (default 80)
//!   the session is armed; a later reading below it (the CLI compacted on its
//!   own) disarms it. Unknown window: never rotated.
//! - At the next safe point — the end of a tool call (`ToolResult`) — the
//!   engine journals `rotation {step_id, session_id, runtime, context_used,
//!   context_window, percent}` and moves `running → handing_off`. The usual
//!   hand-off follows: checkpoint commit, hand-off pack, then a **fresh**
//!   session of the same runtime (never a resume of the full one) in the same
//!   worktree, with the pack as P1. Pinned constraints are P0 of every pack.
//!
//! - Loop guard: when the first reading of the session a rotation started is
//!   already past the threshold (the pack alone fills a tiny window), that
//!   session is never rotated: one `rotation_skipped` event says why, and the
//!   CLI's own compaction takes over. No rotation after every tool call.
//!
//! The armed set and the guard are in memory: after a kernel restart the
//! next `Usage` arms the session again.

use std::collections::HashSet;

use serde_json::json;
use sun_context::HandoffReason;
use sun_core::model::Step;
use sun_core::{Result, SessionId, StepId, SunEvent};
use sun_store::Tx;

use super::failover::Handoff;
use super::{Effect, Executor, append};

/// A session was rotated to a fresh one (timeline).
const ROTATION: &str = "rotation";
/// A fresh session started past the threshold and won't rotate (timeline).
const ROTATION_SKIPPED: &str = "rotation_skipped";

/// The loop guard's memory.
#[derive(Debug, Default)]
pub(super) struct Guard {
    /// Steps whose next session is a rotation's fresh one, not yet measured.
    rotated: HashSet<StepId>,
    /// Sessions that started past the threshold: never rotated.
    pub(super) born_full: HashSet<SessionId>,
}

/// `used` as a whole percent of `window` (rounded down; `window > 0`).
pub(super) fn percent(used: u32, window: u32) -> u32 {
    u32::try_from(u64::from(used) * 100 / u64::from(window.max(1))).unwrap_or(u32::MAX)
}

impl Executor {
    /// The context window of runtime `adapter`, if its adapter knows it.
    pub(super) fn context_window(&self, adapter: &str) -> Option<u32> {
        self.config
            .watchdog
            .context_windows
            .get(adapter)
            .copied()
            .filter(|w| *w > 0)
    }

    /// Hook: a fresh agent `event` of `session`, whose step is `running`.
    pub(super) async fn rotate_if_due(
        &mut self,
        tx: &mut Tx,
        step: &mut Step,
        session: SessionId,
        event: &SunEvent,
        effects: &mut Vec<Effect>,
    ) -> Result<()> {
        match event {
            SunEvent::Usage {
                context_used: Some(used),
                ..
            } => {
                let Some((_, adapter)) = super::ran_on(tx, step).await? else {
                    return Ok(());
                };
                let Some(window) = self.context_window(&adapter) else {
                    return Ok(());
                };
                let fill = percent(*used, window);
                let full = fill >= u32::from(self.config.watchdog.rotate_at_percent);
                if self.rotation_guard.rotated.remove(&step.id) && full {
                    self.rotation_guard.born_full.insert(session);
                    let payload = json!({
                        "step_id": step.id,
                        "session_id": session,
                        "percent": fill,
                        "summary": format!(
                            "The fresh session starts {fill}% full: its context pack alone fills \
                             the window, so it won't rotate again"
                        ),
                    });
                    append(tx, step, ROTATION_SKIPPED, payload).await?;
                }
                if full && !self.rotation_guard.born_full.contains(&session) {
                    self.rotations.insert(session, (*used, window));
                } else {
                    self.rotations.remove(&session);
                }
                Ok(())
            }
            SunEvent::ToolResult { .. } => {
                let Some((used, window)) = self.rotations.remove(&session) else {
                    return Ok(());
                };
                let runtime = self.runtime_of(tx, step).await?;
                let percent = percent(used, window);
                let payload = json!({
                    "step_id": step.id,
                    "session_id": session,
                    "runtime": runtime,
                    "context_used": used,
                    "context_window": window,
                    "percent": percent,
                    "summary": format!(
                        "Context {percent}% full: continuing in a fresh `{runtime}` session"
                    ),
                });
                append(tx, step, ROTATION, payload).await?;
                self.rotation_guard.rotated.insert(step.id);
                let handoff = Handoff {
                    step_id: step.id,
                    from: runtime.clone(),
                    to: runtime,
                    reason: HandoffReason::ContextRotation,
                    reset_at: None,
                };
                self.begin_handoff(tx, step, handoff, None, effects).await
            }
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotation_percent_rounds_down_and_survives_odd_input() {
        assert_eq!(percent(799, 1000), 79);
        assert_eq!(percent(800, 1000), 80);
        assert_eq!(percent(1_200_000, 1_000_000), 120);
        assert_eq!(percent(5, 0), 500);
    }
}
