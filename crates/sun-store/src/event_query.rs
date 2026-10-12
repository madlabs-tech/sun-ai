//! `event_query`: timeline search, usage per runtime and retention
//! compaction over the `event` log.

use async_trait::async_trait;
use sqlx::Row;
use sun_core::model::EventRecord;
use sun_core::{RunId, RunStatus, StateMachine, Timestamp};

use crate::repo::{EVENT_COLUMNS, event_from, parse, ts};
use crate::{Result, Tx, Usage};

/// Kinds compaction keeps: cost per run and per runtime stays exact.
pub const KEPT_KINDS: [&str; 2] = ["usage", "session_assigned"];

/// **Repository** for reads and deletes across the `event` log beyond the
/// append path of [`crate::EventLog`].
#[async_trait]
pub trait EventQuery {
    /// Up to `limit` of the run's events with `seq > after_seq`, ascending,
    /// of `kind` only and containing `text` (case-insensitive, in the
    /// payload) when given.
    async fn search_events(
        &mut self,
        run_id: RunId,
        kind: Option<&str>,
        text: Option<&str>,
        after_seq: i64,
        limit: u32,
    ) -> Result<Vec<EventRecord>>;
    /// Summed `usage` events per runtime name (the `session_assigned`
    /// adapter of the event's session), since `since` when given.
    async fn usage_by_runtime(&mut self, since: Option<&Timestamp>)
    -> Result<Vec<(String, Usage)>>;
    /// Up to `limit` ended runs with no event at or after `before` and not
    /// compacted yet, oldest first.
    async fn runs_to_compact(&mut self, before: &Timestamp, limit: u32) -> Result<Vec<RunId>>;
    /// Deletes the run's events up to `through_seq`, except [`KEPT_KINDS`].
    /// Returns how many went.
    async fn delete_compacted(&mut self, run_id: RunId, through_seq: i64) -> Result<u64>;
}

/// The `kind` of the event compaction leaves behind; its blob holds the
/// removed events as JSON lines.
pub const COMPACTED: &str = "events_compacted";

#[async_trait]
impl EventQuery for Tx {
    async fn search_events(
        &mut self,
        run_id: RunId,
        kind: Option<&str>,
        text: Option<&str>,
        after_seq: i64,
        limit: u32,
    ) -> Result<Vec<EventRecord>> {
        // `LOWER(..) LIKE`: a case-insensitive match.
        let pattern = text.map(|t| {
            let escaped = t
                .to_lowercase()
                .replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_");
            format!("%{escaped}%")
        });
        sqlx::query(&format!(
            "SELECT {EVENT_COLUMNS} FROM event
             WHERE run_id = $1 AND seq > $2 AND ($3 IS NULL OR kind = $3)
               AND ($4 IS NULL OR LOWER(payload_json) LIKE $4 ESCAPE '\\')
             ORDER BY seq LIMIT $5"
        ))
        .bind(run_id.to_string())
        .bind(after_seq)
        .bind(kind)
        .bind(pattern)
        .bind(i64::from(limit))
        .fetch_all(self.conn())
        .await?
        .iter()
        .map(event_from)
        .collect()
    }

    async fn usage_by_runtime(
        &mut self,
        since: Option<&Timestamp>,
    ) -> Result<Vec<(String, Usage)>> {
        // ponytail: joins on json_extract without an index; add a
        // `session.runtime` column if this grows slow on big logs.
        let rows = sqlx::query(
            "SELECT json_extract(a.payload_json, '$.adapter') AS runtime,
                    CAST(COALESCE(SUM(CAST(json_extract(u.payload_json, '$.input') AS BIGINT)), 0)
                         AS BIGINT) AS input,
                    CAST(COALESCE(SUM(CAST(json_extract(u.payload_json, '$.output') AS BIGINT)), 0)
                         AS BIGINT) AS output,
                    CAST(COALESCE(SUM(CAST(json_extract(u.payload_json, '$.cost_usd')
                                  AS DOUBLE PRECISION)), 0) AS DOUBLE PRECISION) AS cost
             FROM event u
             JOIN event a ON a.kind = 'session_assigned'
                         AND json_extract(a.payload_json, '$.session_id') = u.session_id
             WHERE u.kind = 'usage' AND ($1 IS NULL OR u.at >= $1)
             GROUP BY json_extract(a.payload_json, '$.adapter')
             ORDER BY runtime",
        )
        .bind(since.map(ts))
        .fetch_all(self.conn())
        .await?;
        rows.iter()
            .map(|row| {
                Ok((
                    row.try_get("runtime")?,
                    Usage {
                        input_tokens: row.try_get("input")?,
                        output_tokens: row.try_get("output")?,
                        cost_usd: row.try_get("cost")?,
                    },
                ))
            })
            .collect()
    }

    async fn runs_to_compact(&mut self, before: &Timestamp, limit: u32) -> Result<Vec<RunId>> {
        // Static strings from `RunStatus`, not input.
        let ended = RunStatus::ALL
            .iter()
            .filter(|s| s.is_terminal())
            .map(|s| format!("'{}'", s.as_str()))
            .collect::<Vec<_>>()
            .join(", ");
        let ids: Vec<String> = sqlx::query_scalar(&format!(
            "SELECT r.id FROM run r
             WHERE r.status IN ({ended})
               AND EXISTS (SELECT 1 FROM event e WHERE e.run_id = r.id)
               AND NOT EXISTS (SELECT 1 FROM event e WHERE e.run_id = r.id
                               AND (e.at >= $1 OR e.kind = '{COMPACTED}'))
             ORDER BY r.id LIMIT $2"
        ))
        .bind(ts(before))
        .bind(i64::from(limit))
        .fetch_all(self.conn())
        .await?;
        ids.into_iter().map(parse).collect()
    }

    async fn delete_compacted(&mut self, run_id: RunId, through_seq: i64) -> Result<u64> {
        let kept = KEPT_KINDS.map(|k| format!("'{k}'")).join(", ");
        let done = sqlx::query(&format!(
            "DELETE FROM event WHERE run_id = $1 AND seq <= $2 AND kind NOT IN ({kept})"
        ))
        .bind(run_id.to_string())
        .bind(through_seq)
        .execute(self.conn())
        .await?;
        Ok(done.rows_affected())
    }
}
