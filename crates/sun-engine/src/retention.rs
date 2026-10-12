//! Event retention: the timelines of runs that ended and
//! went quiet are compacted into one zstd blob per run. `usage` and
//! `session_assigned` stay, so cost per run and per runtime stays exact.

use std::time::Duration;

use serde_json::json;
use sun_core::{Error, Result, RunId};
use sun_store::{COMPACTED, EventLog, EventQuery, KEPT_KINDS, NewEvent};

use crate::Engine;

/// Runs compacted per pass.
const BATCH: u32 = 50;
/// Events read per page while archiving.
const PAGE: u32 = 1000;
/// How often [`Engine::follow_retention`] runs.
const TICK: Duration = Duration::from_secs(60 * 60);

impl Engine {
    /// Compacts every ended run with no event in the last `keep`. Returns
    /// how many runs it compacted.
    pub async fn compact_events(&self, keep: chrono::Duration) -> Result<usize> {
        let before = chrono::Utc::now() - keep;
        let runs = self
            .store
            .read()
            .await?
            .runs_to_compact(&before, BATCH)
            .await?;
        for &run_id in &runs {
            self.compact_run(run_id).await?;
        }
        Ok(runs.len())
    }

    async fn compact_run(&self, run_id: RunId) -> Result<()> {
        let mut archive = Vec::new();
        let mut through = 0;
        loop {
            let page = self
                .store
                .read()
                .await?
                .events_after(Some(run_id), through, PAGE)
                .await?;
            let Some(last) = page.last() else { break };
            through = last.seq;
            for event in &page {
                serde_json::to_writer(&mut archive, event)
                    .map_err(|e| Error::Protocol(e.to_string()))?;
                archive.push(b'\n');
            }
        }
        // Blob before the transaction (never blob I/O inside one).
        let blob = self.store.blobs().put(archive).await?;
        let mut tx = self.store.begin().await?;
        let removed = tx.delete_compacted(run_id, through).await?;
        let mut event = NewEvent::new(
            run_id,
            None,
            COMPACTED,
            json!({
                "summary": format!("{removed} older events archived"),
                "count": removed,
                "through_seq": through,
                "kept": KEPT_KINDS,
            }),
        );
        event.blob = Some(blob);
        tx.append_event(event).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Runs forever: compacts quiet ended runs every hour, keeping
    /// `keep_days` of history. `sun serve` spawns it when
    /// `event_retention_days` is set.
    pub async fn follow_retention(self, keep_days: u32) {
        let keep = chrono::Duration::days(keep_days.into());
        let mut tick = tokio::time::interval(TICK);
        loop {
            tick.tick().await;
            if let Err(e) = self.compact_events(keep).await {
                eprintln!("sun: event retention: {e}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use serde_json::json;
    use sun_core::{HostId, RunStatus};
    use sun_store::{RunRepo, Store};

    use super::*;
    use crate::EngineConfig;
    use crate::gates::tests::{LocalHost, events, mods, seed};

    const FLOW: &str = "name: f\nsteps:\n  - id: a\n    type: agent\n    prompt: p\n";

    #[tokio::test]
    async fn quiet_ended_runs_compact_into_one_blob_and_keep_usage() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).await.unwrap();
        let ended = seed(&store, FLOW).await;
        let fresh = seed(&store, FLOW).await;
        let old = chrono::Utc::now() - chrono::Duration::days(40);
        let mut tx = store.begin().await.unwrap();
        for run in [ended, fresh] {
            tx.set_run_status(run, RunStatus::Cancelled, Some("cancelled"))
                .await
                .unwrap();
            for kind in ["text", "tool_call", "usage"] {
                let mut event = NewEvent::new(run, None, kind, json!({ "input": 3 }));
                if run == ended {
                    event.at = old;
                }
                tx.append_event(event).await.unwrap();
            }
        }
        tx.commit().await.unwrap();
        let config = EngineConfig {
            host: HostId::generate(),
            adapter: "fake".into(),
            delivery_timeout: Duration::from_secs(5),
            watchdog: crate::WatchdogConfig::default(),
        };
        let host = LocalHost::new(dir.path(), |_, _| {});
        let (engine, _task) = Engine::start(store.clone(), host, mods(json!({})), config)
            .await
            .unwrap();
        let keep = chrono::Duration::days(30);

        assert_eq!(engine.compact_events(keep).await.unwrap(), 1);
        assert_eq!(engine.compact_events(keep).await.unwrap(), 0, "once");
        assert!(events(&store, ended, "text").await.is_empty());
        assert_eq!(events(&store, ended, "usage").await.len(), 1);
        assert_eq!(events(&store, fresh, "text").await.len(), 1, "too recent");
        let page = engine.events(ended, 0, 100).await.unwrap();
        let marker = page.iter().find(|e| e.kind == COMPACTED).unwrap();
        assert_eq!(marker.payload["count"], 2);
        let archived = store
            .blobs()
            .get(marker.blob.as_ref().unwrap())
            .await
            .unwrap();
        assert_eq!(String::from_utf8(archived).unwrap().lines().count(), 3);
    }
}
