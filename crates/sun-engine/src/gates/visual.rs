//! The optional `visual_diff` gate: screens outside the
//! task's scope must look the same on the task branch as on the base branch.
//!
//! ```yaml
//! gates:
//!   - visual_diff:
//!       screens: [/, /pricing, /settings]   # default: the `.sun-preview` screens
//!       scope: [/settings]                  # what the task may change
//!       widths: [360, 1280]                 # default: 360, 768, 1280
//!       threshold: 0.01                     # share of pixels; default 1%
//! ```
//!
//! Each out-of-scope screen is screenshotted on both preview sides at every
//! width ([`HostLink::preview_screenshot`]) and compared pixel by pixel
//! ([`crate::screenshot::changed`]); the gate fails when any changed more
//! than `threshold`.

use serde::Deserialize;
use serde_json::Value;
use sun_core::host_protocol::{PreviewSide, ScreenshotRequest, Worktree};
use sun_core::{HostId, HostLink};

use crate::pins::BREAKPOINTS;
use crate::screenshot;

/// Changed pixels allowed by default (1%).
const DEFAULT_THRESHOLD: f64 = 0.01;

/// `visual_diff` params.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Params {
    /// Every screen to check, as page paths; empty: the repo's
    /// `.sun-preview` screens.
    #[serde(default)]
    screens: Vec<String>,
    /// Screens the task may change; never compared.
    #[serde(default)]
    scope: Vec<String>,
    /// Breakpoints.
    #[serde(default)]
    widths: Option<Vec<u32>>,
    /// Largest share of changed pixels allowed outside the scope.
    #[serde(default)]
    threshold: Option<f64>,
    /// Accepted like on every gate; screenshots carry the host's own wait.
    #[serde(default, rename = "timeout_s")]
    _timeout_s: Option<u64>,
}

impl Params {
    pub(super) fn parse(params: Option<&Value>) -> Result<Self, String> {
        let params: Self = serde_json::from_value(params.cloned().unwrap_or(Value::Null))
            .map_err(|e| format!("`visual_diff` params: {e}"))?;
        let page =
            |p: &String| p.starts_with('/') && p.len() <= 1000 && !p.contains(char::is_whitespace);
        if !params.screens.iter().chain(&params.scope).all(page) {
            return Err("`visual_diff` screens and scope are page paths starting with `/`".into());
        }
        if !params.threshold.is_none_or(|t| (0.0..=1.0).contains(&t)) {
            return Err("`visual_diff` threshold is a share between 0 and 1".into());
        }
        if params
            .widths
            .as_ref()
            .is_some_and(|w| w.is_empty() || w.iter().any(|w| !(100..=4000).contains(w)))
        {
            return Err("`visual_diff` widths are CSS pixels between 100 and 4000".into());
        }
        Ok(params)
    }

    /// Compares every out-of-scope screen; `Err` lists those that changed.
    pub(super) async fn check(
        &self,
        hosts: &dyn HostLink,
        host: HostId,
        worktree: &Worktree,
    ) -> Result<String, String> {
        let widths = self
            .widths
            .clone()
            .unwrap_or_else(|| BREAKPOINTS.iter().map(|(w, _)| *w).collect());
        let threshold = self.threshold.unwrap_or(DEFAULT_THRESHOLD);
        let (mut report, mut failed) = (Vec::new(), false);
        let screens = if self.screens.is_empty() {
            let design = crate::executor::read_design(hosts, host, worktree).await;
            design.screens.into_iter().map(|s| s.path).collect()
        } else {
            self.screens.clone()
        };
        for path in screens.iter().filter(|p| !self.scope.contains(p)) {
            for &width in &widths {
                let height = BREAKPOINTS
                    .iter()
                    .find(|(w, _)| *w == width)
                    .map_or(800, |(_, h)| *h);
                let shot = |side| ScreenshotRequest {
                    worktree: worktree.clone(),
                    side,
                    path: path.clone(),
                    width,
                    height,
                };
                let take = |side| async move {
                    hosts
                        .preview_screenshot(host, shot(side))
                        .await
                        .map_err(|e| format!("{path} at {width} px ({side}): {e}"))
                };
                let before = take(PreviewSide::Before).await?;
                let after = take(PreviewSide::After).await?;
                let share = screenshot::changed(&before, &after).map_err(|e| e.to_string())?;
                let over = share > threshold;
                failed |= over;
                report.push(format!(
                    "{path} at {width} px: {:.2}% of pixels changed{}",
                    share * 100.0,
                    if over { " (over the limit)" } else { "" }
                ));
            }
        }
        let mut text = report.join("\n");
        if failed {
            text.push_str(&format!(
                "\nscreens outside the task's scope changed by more than {:.2}%: \
                 undo those changes or add the screen to `scope:`",
                threshold * 100.0
            ));
            Err(text)
        } else if text.is_empty() {
            Ok("every screen is in the task's scope".into())
        } else {
            Ok(text)
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::path::PathBuf;

    use async_trait::async_trait;
    use serde_json::json;
    use sun_core::host_protocol::{ExecOutput, ExecRequest};
    use sun_core::{
        BoxStream, HostEvent, ModCall, ModLink, Result, RunId, SessionHandle, SessionId,
        SessionSpec, Steer, StopMode, TaskId,
    };
    use sun_workflow::GateSpec;

    use super::*;
    use crate::gates::Job;

    /// The app: `/pricing` gained a banner on the task branch, the rest is
    /// unchanged.
    struct App;

    #[async_trait]
    impl HostLink for App {
        async fn start_session(&self, _: HostId, _: &str, _: SessionSpec) -> Result<SessionHandle> {
            unreachable!()
        }
        async fn steer(&self, _: &SessionHandle, _: Steer) -> Result<()> {
            Ok(())
        }
        async fn stop_session(&self, _: &SessionHandle, _: StopMode) -> Result<()> {
            Ok(())
        }
        async fn is_alive(&self, _: &SessionHandle) -> Result<bool> {
            Ok(false)
        }
        fn events(&self) -> BoxStream<'static, HostEvent> {
            unreachable!()
        }
        async fn ack(&self, _: SessionId, _: u64) -> Result<()> {
            Ok(())
        }
        async fn preview_screenshot(&self, _: HostId, r: ScreenshotRequest) -> Result<Vec<u8>> {
            let banner = r.side == PreviewSide::After && r.path == "/pricing";
            Ok(screenshot::test_png(r.width, r.height, |_, y| {
                if banner && y < 100 {
                    [200, 0, 0, 255]
                } else {
                    [255, 255, 255, 255]
                }
            }))
        }
        async fn prepare_worktree(&self, _: HostId, _: Worktree) -> Result<PathBuf> {
            Ok(PathBuf::from("/w"))
        }
        /// The repo's `.sun-preview` lists two screens; it has no design files.
        async fn exec(&self, _: HostId, r: ExecRequest) -> Result<ExecOutput> {
            let list = r.command.starts_with("head -c 4096 .sun-preview");
            Ok(ExecOutput {
                code: Some(if list { 0 } else { 1 }),
                output: if list {
                    "npm run dev\n/  Home\n# not a screen\n/pricing Pricing\n\0".into()
                } else {
                    String::new()
                },
                timed_out: false,
            })
        }
    }

    struct NoMods;

    #[async_trait]
    impl ModLink for NoMods {
        async fn call(&self, _: ModCall) -> Result<Value> {
            unreachable!()
        }
    }

    async fn gate(params: Value) -> (bool, String) {
        let spec = GateSpec {
            name: "visual_diff".into(),
            params: Some(params),
        };
        let worktree = Worktree::Repo {
            task_id: TaskId::generate(),
            url: "/repos/app".into(),
            base: None,
            branch: "sun/SUN-1-x".into(),
        };
        let job = Job::new(HostId::generate(), worktree, RunId::generate(), &[spec]).unwrap();
        let result = job.run(&App, &NoMods).await.remove(0);
        (result.pass, result.output)
    }

    /// A task scoped to `/settings` that also changed `/pricing`
    /// fails the gate; with `/pricing` in scope it passes.
    #[tokio::test]
    async fn visual_gate_fails_out_of_scope_change() {
        let screens = json!(["/", "/pricing", "/settings"]);
        let (pass, output) =
            gate(json!({ "screens": screens, "scope": ["/settings"], "widths": [360] })).await;
        assert!(!pass, "{output}");
        assert!(
            output.contains("\n/pricing at 360 px: 13.51% of pixels changed (over the limit)"),
            "{output}"
        );
        assert!(
            output.starts_with("/ at 360 px: 0.00% of pixels changed\n"),
            "{output}"
        );
        assert!(
            !output.contains("/settings at"),
            "in scope, not compared: {output}"
        );

        let (pass, output) =
            gate(json!({ "screens": screens, "scope": ["/settings", "/pricing"] })).await;
        assert!(pass, "{output}");
        assert_eq!(output.lines().count(), 3, "one line per width: {output}");

        // A banner on 100 of 740 rows is under a 20% threshold.
        let (pass, _) =
            gate(json!({ "screens": ["/pricing"], "widths": [360], "threshold": 0.2 })).await;
        assert!(pass);

        // No `screens:`: the repo's `.sun-preview` screens are compared.
        let (pass, output) = gate(json!({ "widths": [360] })).await;
        assert!(!pass, "{output}");
        assert!(output.starts_with("/ at 360 px: 0.00%"), "{output}");
        assert!(output.contains("\n/pricing at 360 px: 13.51%"), "{output}");
        assert!(Params::parse(Some(&json!({ "screens": ["pricing"] }))).is_err());
        assert!(Params::parse(Some(&json!({ "screens": ["/"], "threshold": 5 }))).is_err());
    }
}
