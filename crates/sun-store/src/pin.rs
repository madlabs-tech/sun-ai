//! `pin`: design-canvas pins on a task.

use async_trait::async_trait;
use sqlx::Row;
use sqlx::sqlite::SqliteRow;
use sun_core::model::Pin;
use sun_core::{BlobHash, PinId, TaskId};

use crate::repo::{json, parse, parse_ts, ts};
use crate::{Result, StoreError, Tx};

/// **Repository** for `pin` rows. `save_pin` upserts the whole row.
#[async_trait]
pub trait PinRepo {
    /// Inserts the pin, or replaces the row with the same id.
    async fn save_pin(&mut self, pin: &Pin) -> Result<()>;
    /// One pin, or `NotFound`.
    async fn get_pin(&mut self, id: PinId) -> Result<Pin>;
    /// The task's pins, oldest first.
    async fn pins_of(&mut self, task_id: TaskId) -> Result<Vec<Pin>>;
    /// Deletes the pin (no-op when missing).
    async fn delete_pin(&mut self, id: PinId) -> Result<()>;
}

#[async_trait]
impl PinRepo for Tx {
    async fn save_pin(&mut self, pin: &Pin) -> Result<()> {
        sqlx::query(
            "INSERT INTO pin \
             (id, task_id, status, comment, target_json, edits_json, created_at, updated_at, \
             before_crop, after_crop) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) \
             ON CONFLICT (id) DO UPDATE SET task_id = excluded.task_id, \
             status = excluded.status, comment = excluded.comment, \
             target_json = excluded.target_json, edits_json = excluded.edits_json, \
             created_at = excluded.created_at, updated_at = excluded.updated_at, \
             before_crop = excluded.before_crop, after_crop = excluded.after_crop",
        )
        .bind(pin.id.to_string())
        .bind(pin.task_id.to_string())
        .bind(pin.status.as_str())
        .bind(&pin.comment)
        .bind(serde_json::to_string(&pin.target)?)
        .bind(serde_json::to_string(&pin.edits)?)
        .bind(ts(&pin.created_at))
        .bind(ts(&pin.updated_at))
        .bind(pin.before_crop.as_ref().map(|h| h.as_str().to_owned()))
        .bind(pin.after_crop.as_ref().map(|h| h.as_str().to_owned()))
        .execute(self.conn())
        .await?;
        Ok(())
    }

    async fn get_pin(&mut self, id: PinId) -> Result<Pin> {
        let row = sqlx::query("SELECT * FROM pin WHERE id = $1")
            .bind(id.to_string())
            .fetch_optional(self.conn())
            .await?
            .ok_or_else(|| StoreError::not_found("pin", id))?;
        pin_from(&row)
    }

    async fn pins_of(&mut self, task_id: TaskId) -> Result<Vec<Pin>> {
        sqlx::query("SELECT * FROM pin WHERE task_id = $1 ORDER BY created_at, rowid")
            .bind(task_id.to_string())
            .fetch_all(self.conn())
            .await?
            .iter()
            .map(pin_from)
            .collect()
    }

    async fn delete_pin(&mut self, id: PinId) -> Result<()> {
        sqlx::query("DELETE FROM pin WHERE id = $1")
            .bind(id.to_string())
            .execute(self.conn())
            .await?;
        Ok(())
    }
}

fn pin_from(row: &SqliteRow) -> Result<Pin> {
    Ok(Pin {
        id: parse(row.try_get("id")?)?,
        task_id: parse(row.try_get("task_id")?)?,
        status: parse(row.try_get("status")?)?,
        comment: row.try_get("comment")?,
        target: json(row.try_get("target_json")?)?,
        edits: json(row.try_get("edits_json")?)?,
        before_crop: crop(row, "before_crop")?,
        after_crop: crop(row, "after_crop")?,
        created_at: parse_ts(row.try_get("created_at")?)?,
        updated_at: parse_ts(row.try_get("updated_at")?)?,
    })
}

fn crop(row: &SqliteRow, column: &str) -> Result<Option<BlobHash>> {
    let hex: Option<String> = row.try_get(column)?;
    Ok(hex.map(BlobHash::new).transpose()?)
}
