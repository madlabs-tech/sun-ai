//! # sun-context — context for agents
//!
//! **Responsibility:** the context compiler, hand-off packs, the knowledge
//! index and the `sun` MCP server.
//!
//! **Contract:**
//! - Ledger-first, file-delivered, pull-based context; every pack is
//!   reproducible from the event log.
//! - The MCP server reaches the engine only through `sun_core::EngineHandle`.
//!
//! **May depend on:** `sun-core`, `sun-store`.
//!
//! ## Using it (engine side)
//!
//! 1. [`compile`] (pure) → [`ContextPack`]; [`ContextPack::envelope`] goes into
//!    the first prompt, [`ContextPack::files`] into the session dir
//!    ([`write_files`] on the host).
//! 2. [`store_pack`] puts the pack blob and returns the `context_pack` row;
//!    insert it with `sun_store::ContextRepo` in the step's transaction.
//! 3. On failover/rotation, [`build_handoff`] (pure, no LLM) → [`HandoffPack`];
//!    put it in the next [`LedgerView`] and compile again.
//! 4. [`McpSessions::issue`] a token per session; the stdio shim presents it
//!    and [`McpSessions::open`] gives the scoped [`SunMcp`] server to serve
//!    ([`SunMcp::serve_on`]);
//!    `with_knowledge(store)` enables its `search` tool.
//! 5. Knowledge: [`knowledge::add_source`], then [`knowledge::ingest`] each
//!    connector doc (denylist, hash skip, redact, chunk, FTS5);
//!    [`knowledge::search`] returns cited [`KnowledgeHit`]s, scoped to the
//!    task's sources; [`knowledge::retrieve_for_step`] gives P4 for
//!    [`compile`]. Vectors are opt-in: pass a [`knowledge::Embedder`] in the
//!    [`knowledge::Scope`] and run [`knowledge::embed_pending`] after ingest.
//!    Retrieved hits carry stale/conflict [`knowledge::KnowledgeFlag`]s, which
//!    [`compile`] puts up front as warnings; connector tombstones go to
//!    [`knowledge::retire`].

mod handoff;
pub mod knowledge;
mod mcp;
mod pack;

use std::path::Path;

pub use handoff::{
    GateResult, GitState, HandoffFrom, HandoffInput, HandoffPack, HandoffReason, PlanItem,
    build_handoff, transcript,
};
pub use mcp::{AskArgs, DoneArgs, GetArgs, McpSessions, NoteArgs, SearchArgs, SunMcp, render_hits};
pub use pack::{
    BYTES_PER_TOKEN, ContextPack, DesignFile, HANDOFF_PATH, KnowledgeHit, LedgerView, PACK_PATH,
    PROGRESS_PATH, PackItem, SOURCES_DIR, SessionFile, StepOutput, WORKSPACE_CONTEXT, compile,
    estimate_tokens,
};
use sun_core::{ContextPackId, StepId, model};
use sun_store::{BlobStore, StoreError};

/// Puts the rendered pack in the blob store and returns its `context_pack`
/// row. Call it before the step's write transaction, then insert the row with
/// `sun_store::ContextRepo::insert_context_pack` inside it.
pub async fn store_pack(
    blobs: &BlobStore,
    step_id: StepId,
    pack: &ContextPack,
) -> Result<model::ContextPack, StoreError> {
    let pack_hash = blobs.put(pack.render_markdown().into_bytes()).await?;
    Ok(model::ContextPack {
        id: ContextPackId::generate(),
        step_id,
        pack_hash,
        token_estimate: pack.token_estimate,
        items: serde_json::to_value(&pack.items)?,
    })
}

/// Writes session files under `dir`. `.sun/context/` is derived, so it is
/// cleared first (no stale sources); files with `overwrite: false` are only
/// created when missing.
pub fn write_files(dir: &Path, files: &[SessionFile]) -> std::io::Result<()> {
    match std::fs::remove_dir_all(dir.join(".sun/context")) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    for file in files {
        let path = dir.join(&file.path);
        if !file.overwrite && path.exists() {
            continue;
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, &file.contents)?;
    }
    Ok(())
}
