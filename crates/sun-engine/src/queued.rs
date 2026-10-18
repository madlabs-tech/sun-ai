//! Messages queued for the next step: instead of
//! steering the live agent, a person leaves a message that the run's next
//! agent session gets in its first prompt.
//!
//! Event sourced, no table: `message_queued {text}` journals a message, and
//! the session that takes them journals `messages_delivered {step_id,
//! through, count}` in the same transaction as its assignment, so each
//! message reaches exactly one session.

use serde_json::json;
use sun_core::model::{EventRecord, Step};
use sun_core::{Error, Result, RunId, StateMachine};
use sun_store::{EventLog, NewEvent, RunRepo, Tx};

use crate::{Actor, Engine};

/// A message waiting for the next step.
pub(crate) const QUEUED: &str = "message_queued";
/// The queued messages up to `through` went to a session.
const DELIVERED: &str = "messages_delivered";

impl Engine {
    /// Queues `by`'s `text` for the next agent session of the run (not a
    /// steer of the current one); from there the run acts for `by` (its
    /// bypass cap, [`crate::bypass`]). The run must not have ended.
    pub async fn queue_message(
        &self,
        run_id: RunId,
        text: String,
        by: Actor,
    ) -> Result<EventRecord> {
        let mut tx = self.store.begin().await?;
        if tx.get_run(run_id).await?.status.is_terminal() {
            return Err(Error::InvalidInput(
                "this run has ended; there is no next step".into(),
            ));
        }
        let event = leave(&mut tx, run_id, &text, Some(&by)).await?;
        tx.commit().await?;
        Ok(event)
    }
}

/// Journals `by`'s `text` for the run's next agent session, in the caller's
/// transaction.
pub(crate) async fn leave(
    tx: &mut Tx,
    run_id: RunId,
    text: &str,
    by: Option<&Actor>,
) -> Result<EventRecord> {
    let mut message = json!({ "text": text });
    crate::bypass::stamp(&mut message, by);
    Ok(tx
        .append_event(NewEvent::new(run_id, None, QUEUED, message))
        .await?)
}

/// `prompt` plus every message queued since the last delivery, which is
/// journaled on `step` in the caller's transaction.
pub(crate) async fn take_queued(tx: &mut Tx, step: &Step, prompt: String) -> Result<String> {
    Ok(prompt + &take(tx, step).await?)
}

/// [`take_queued`]'s messages block (empty without messages).
pub(crate) async fn take(tx: &mut Tx, step: &Step) -> Result<String> {
    let (mut after, mut queued) = (0, Vec::new());
    loop {
        let page = tx.events_after(Some(step.run_id), after, 500).await?;
        let Some(last) = page.last() else { break };
        after = last.seq;
        for e in page {
            match e.kind.as_str() {
                QUEUED => queued.push(e),
                DELIVERED => queued.clear(),
                _ => {}
            }
        }
    }
    let Some(through) = queued.last().map(|e| e.seq) else {
        return Ok(String::new());
    };
    tx.append_event(NewEvent::new(
        step.run_id,
        Some(step.step_key.clone()),
        DELIVERED,
        json!({ "step_id": step.id, "through": through, "count": queued.len() }),
    ))
    .await?;
    let mut out = String::from("\n\nMessages left for this step:");
    for e in &queued {
        out.push_str(&format!(
            "\n- {}",
            e.payload["text"].as_str().unwrap_or_default()
        ));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use sun_core::HostId;
    use sun_store::Store;

    use crate::gates::tests::{LocalHost, events, mods, seed, until_run_ends};
    use crate::{EngineConfig, WatchdogConfig};

    use super::*;

    #[tokio::test(flavor = "multi_thread")]
    async fn queued_messages_reach_the_next_session_once() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).await.unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let host = LocalHost::new(dir.path(), {
            let seen = seen.clone();
            move |_, envelope| seen.lock().unwrap().push(envelope.to_owned())
        });
        let config = EngineConfig {
            allow_bypass: false,
            host: HostId::generate(),
            adapter: "fake".into(),
            delivery_timeout: Duration::from_secs(5),
            watchdog: WatchdogConfig::default(),
        };
        let (engine, _task) = Engine::start(store.clone(), host, mods(json!({})), config)
            .await
            .unwrap();
        let flow = "name: two\nsteps:\n  - id: a\n    type: agent\n    prompt: first\n  - id: b\n    type: agent\n    prompt: second\n    needs: [a]\n";
        let run = seed(&store, flow).await;
        engine
            .queue_message(run, "use the v2 API".into(), ana())
            .await
            .unwrap();
        engine.start_run(run).await.unwrap();
        let ended = until_run_ends(&store, run).await;
        assert!(ended.status.is_terminal());
        let seen = seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 2, "{seen:?}");
        assert!(seen[0].contains("use the v2 API"), "{}", seen[0]);
        assert!(!seen[1].contains("use the v2 API"), "delivered once");
        assert_eq!(events(&store, run, DELIVERED).await.len(), 1);
        let err = engine.queue_message(run, "late".into(), ana()).await;
        let err = err.unwrap_err();
        assert!(matches!(err, Error::InvalidInput(_)));
    }

    /// The token holder, as a person acting on the run.
    fn ana() -> crate::Actor {
        crate::Actor {
            member_id: None,
            name: "user:ana".into(),
        }
    }
}
