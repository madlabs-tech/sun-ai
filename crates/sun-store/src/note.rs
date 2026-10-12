//! `note`: what agents record with `sun.note` (decisions, todos, learnings,
//! progress, questions) — the run's blackboard.

use async_trait::async_trait;
use sqlx::Row;
use sun_core::model::Note;
use sun_core::{NoteId, RunId, StepKey};

use crate::repo::parse;
use crate::{Result, StoreError, Tx};

/// **Repository** for `note` rows. Write the matching `note` event in the same `Tx`.
#[async_trait]
pub trait NoteRepo {
    /// Inserts a note.
    async fn insert_note(&mut self, note: &Note) -> Result<()>;
    /// The run's notes (every step's), oldest first.
    async fn notes_of_run(&mut self, run_id: RunId) -> Result<Vec<Note>>;
    /// One note. `NotFound` when missing.
    async fn get_note(&mut self, id: NoteId) -> Result<Note>;
}

#[async_trait]
impl NoteRepo for Tx {
    async fn insert_note(&mut self, note: &Note) -> Result<()> {
        sqlx::query(
            "INSERT INTO note (id, run_id, step_key, kind, body, superseded_by) \
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(note.id.to_string())
        .bind(note.run_id.to_string())
        .bind(note.step_key.as_str())
        .bind(note.kind.as_str())
        .bind(&note.body)
        .bind(note.superseded_by.map(|id| id.to_string()))
        .execute(self.conn())
        .await?;
        Ok(())
    }

    async fn get_note(&mut self, id: NoteId) -> Result<Note> {
        let row = sqlx::query("SELECT run_id FROM note WHERE id = $1")
            .bind(id.to_string())
            .fetch_optional(self.conn())
            .await?
            .ok_or_else(|| StoreError::not_found("note", id))?;
        let run_id: RunId = parse(row.try_get("run_id")?)?;
        self.notes_of_run(run_id)
            .await?
            .into_iter()
            .find(|n| n.id == id)
            .ok_or_else(|| StoreError::not_found("note", id))
    }

    async fn notes_of_run(&mut self, run_id: RunId) -> Result<Vec<Note>> {
        // Insertion order (ULIDs made in the same millisecond are unordered).
        let rows = sqlx::query(
            "SELECT id, step_key, kind, body, superseded_by FROM note \
             WHERE run_id = $1 ORDER BY rowid",
        )
        .bind(run_id.to_string())
        .fetch_all(self.conn())
        .await?;
        rows.iter()
            .map(|row| {
                let superseded: Option<String> = row.try_get("superseded_by")?;
                Ok(Note {
                    id: parse(row.try_get("id")?)?,
                    run_id,
                    step_key: StepKey::new(row.try_get::<String, _>("step_key")?)?,
                    kind: parse(row.try_get("kind")?)?,
                    body: row.try_get("body")?,
                    superseded_by: superseded.map(parse).transpose()?,
                })
            })
            .collect()
    }
}
