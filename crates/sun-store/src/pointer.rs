//! Which blobs a run may read back through `sun.get`.

use async_trait::async_trait;
use sun_core::{BlobHash, RunId};

use crate::{Result, Tx};

/// **Repository** query scoping blob pointers to one run.
#[async_trait]
pub trait PointerRepo {
    /// Whether `hash` belongs to the run: one of its context or hand-off
    /// packs, an artifact of its task, or a blob on one of its events.
    async fn run_has_blob(&mut self, run_id: RunId, hash: &BlobHash) -> Result<bool>;
}

#[async_trait]
impl PointerRepo for Tx {
    async fn run_has_blob(&mut self, run_id: RunId, hash: &BlobHash) -> Result<bool> {
        let found: i64 = sqlx::query_scalar(
            "SELECT CASE WHEN EXISTS (
               SELECT 1 FROM context_pack c JOIN step s ON s.id = c.step_id
                 WHERE s.run_id = $1 AND c.pack_hash = $2
               UNION ALL SELECT 1 FROM handoff_pack WHERE run_id = $1 AND pack_hash = $2
               UNION ALL SELECT 1 FROM artifact
                 WHERE task_id = (SELECT task_id FROM run WHERE id = $1) AND blob = $2
               UNION ALL SELECT 1 FROM event WHERE run_id = $1 AND blob = $2) THEN 1 ELSE 0 END",
        )
        .bind(run_id.to_string())
        .bind(hash.as_str())
        .fetch_one(self.conn())
        .await?;
        Ok(found != 0)
    }
}
