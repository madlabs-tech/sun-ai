//! The `bare` environment: the CLI and commands run directly on the host.

use async_trait::async_trait;
use sun_core::host_protocol::{ExecOutput, ExecRequest};
use sun_core::{Result, SessionSpec};

use super::ExecEnv;
use crate::process;

/// Runs everything on the host in the worktree; nothing to set up.
pub struct Bare;

#[async_trait]
impl ExecEnv for Bare {
    async fn prepare(&self, _spec: &mut SessionSpec) -> Result<()> {
        Ok(())
    }

    async fn exec(&self, request: &ExecRequest) -> Result<ExecOutput> {
        process::exec(request).await
    }

    async fn teardown(&self) -> Result<()> {
        Ok(())
    }
}
