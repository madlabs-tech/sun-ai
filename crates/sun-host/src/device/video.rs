//! H.264 video of a live device: an encoder process (`adb
//! screenrecord`, `idb video-stream`) whose Annex B stdout is cut into
//! access units and handed to the viewer that pulls them (`device.video`).
//!
//! **Design:** long-poll pull, like `device.screen`, so the host keeps no
//! subscription to the kernel: each viewer stream owns one encoder, which
//! stops when nobody pulled it for [`STALE`].

use std::process::Stdio;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::process::{Child, ChildStdout, Command};
use tokio::sync::Notify;
use tokio::time::Instant;

/// A stream nobody pulled for this long is stopped.
const STALE: Duration = Duration::from_secs(5);
/// Bytes still waiting for a pull before the viewer counts as gone.
const MAX_QUEUED: usize = 8 * 1024 * 1024;
/// Encoders write a picture at once: this long without bytes ends it.
// ponytail: an idle gap marks the end of the last picture (Annex B has no
// length); a slow pipe could split one. Use scrcpy's framed packets if so.
const PICTURE_END: Duration = Duration::from_millis(8);

/// One viewer's stream of one device.
pub(super) struct Feed {
    pub(super) task: String,
    state: Mutex<State>,
    ready: Notify,
}

struct State {
    frames: Vec<Vec<u8>>,
    queued: usize,
    pulled: Instant,
    ended: Option<String>,
}

impl Feed {
    /// Starts `encoder`, which writes H.264 Annex B to stdout.
    pub(super) fn start(task: &str, mut encoder: Command) -> std::io::Result<Arc<Self>> {
        encoder
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = encoder.spawn()?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| std::io::Error::other("no stdout"))?;
        let feed = Arc::new(Self {
            task: task.to_owned(),
            state: Mutex::new(State {
                frames: Vec::new(),
                queued: 0,
                pulled: Instant::now(),
                ended: None,
            }),
            ready: Notify::new(),
        });
        tokio::spawn(Arc::clone(&feed).read(child, stdout));
        Ok(feed)
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Ended and its viewer gone too (else it still owes them the reason).
    pub(super) fn gone(&self) -> bool {
        let state = self.state();
        state.ended.is_some() && state.pulled.elapsed() >= STALE
    }

    /// The frames since the last pull, waiting up to `wait` for one. `Err`
    /// once the encoder stopped and every frame was taken.
    pub(super) async fn pull(&self, wait: Duration) -> Result<Vec<Vec<u8>>, String> {
        let deadline = Instant::now() + wait;
        loop {
            let ready = self.ready.notified();
            {
                let mut state = self.state();
                state.pulled = Instant::now();
                if !state.frames.is_empty() {
                    state.queued = 0;
                    return Ok(std::mem::take(&mut state.frames));
                }
                if let Some(reason) = &state.ended {
                    return Err(reason.clone());
                }
            }
            if tokio::time::timeout_at(deadline, ready).await.is_err() {
                return Ok(Vec::new());
            }
        }
    }

    /// Queues `frames`; `false` once the viewer is gone (unpulled or behind).
    fn push(&self, frames: Vec<Vec<u8>>) -> bool {
        let mut state = self.state();
        if !frames.is_empty() {
            state.queued += frames.iter().map(Vec::len).sum::<usize>();
            state.frames.extend(frames);
            self.ready.notify_waiters();
        }
        state.pulled.elapsed() < STALE && state.queued <= MAX_QUEUED
    }

    async fn read(self: Arc<Self>, child: Child, mut stdout: ChildStdout) {
        let mut units = AccessUnits::default();
        let mut buf = vec![0; 64 * 1024];
        let reason = loop {
            let wait = if units.pending() { PICTURE_END } else { STALE };
            let frames = match tokio::time::timeout(wait, stdout.read(&mut buf)).await {
                Ok(Ok(0)) => {
                    self.push(units.flush());
                    let out = child.wait_with_output().await;
                    let said = out.map(|o| String::from_utf8_lossy(&o.stderr).trim().to_owned());
                    break match said.unwrap_or_default() {
                        said if said.is_empty() => "the device's video stream ended".into(),
                        said => format!("the device's video stream ended: {said}"),
                    };
                }
                Ok(Ok(n)) => units.push(&buf[..n]),
                Ok(Err(e)) => break format!("the device's video stream failed: {e}"),
                Err(_) => units.flush(),
            };
            if units.overflowed() {
                break "the device's video is not H.264".into();
            }
            if !self.push(frames) {
                break "nobody is watching".into();
            }
        };
        self.state().ended = Some(reason);
        self.ready.notify_waiters();
        // Dropping `child` (if still here) kills the encoder.
    }
}

/// Cuts an H.264 Annex B byte stream into access units (one picture each,
/// with the SPS/PPS/SEI before it): a unit ends where a non-VCL NAL (SEI,
/// SPS, PPS, AUD) or the first slice of the next picture follows a slice.
#[derive(Default)]
pub(super) struct AccessUnits {
    /// The unit being read (from its start code), or bytes before the first.
    pending: Vec<u8>,
    /// No start code is before this.
    scan: usize,
    /// `pending` begins with a start code.
    started: bool,
    /// The unit being read has a slice.
    has_slice: bool,
}

impl AccessUnits {
    /// Most bytes of one unit.
    const MAX: usize = 4 * 1024 * 1024;

    /// The unit being read has a slice, so it may be done ([`Self::flush`]).
    pub(super) fn pending(&self) -> bool {
        self.has_slice
    }

    /// No unit ended in [`Self::MAX`] bytes: the input is not H.264.
    pub(super) fn overflowed(&self) -> bool {
        self.pending.len() > Self::MAX
    }

    /// Adds bytes; returns the units they complete.
    pub(super) fn push(&mut self, bytes: &[u8]) -> Vec<Vec<u8>> {
        self.pending.extend_from_slice(bytes);
        let mut done = Vec::new();
        loop {
            let found = self.pending[self.scan..]
                .windows(3)
                .position(|w| w == [0, 0, 1]);
            let Some(mut at) = found.map(|p| self.scan + p) else {
                // A start code may straddle the next push.
                self.scan = self.scan.max(self.pending.len().saturating_sub(2));
                return done;
            };
            let (Some(&header), Some(&next)) = (self.pending.get(at + 3), self.pending.get(at + 4))
            else {
                self.scan = at; // wait for the NAL header
                return done;
            };
            // A four-byte start code's leading zero belongs to it.
            let start = if at > 0 && self.pending[at - 1] == 0 {
                at - 1
            } else {
                at
            };
            let kind = header & 0x1f;
            let slice = matches!(kind, 1 | 5);
            // `first_mb_in_slice` = 0 is exp-Golomb `1`: the top bit.
            let new_picture = matches!(kind, 6..=9) || (slice && next & 0x80 != 0);
            if !self.started {
                self.pending.drain(..start); // not H.264
                self.started = true;
                at -= start;
            } else if self.has_slice && new_picture {
                done.push(self.pending.drain(..start).collect());
                self.has_slice = false;
                at -= start;
            }
            self.has_slice |= slice;
            self.scan = at + 3;
        }
    }

    /// Ends the unit being read when it has a slice (the encoder paused).
    pub(super) fn flush(&mut self) -> Vec<Vec<u8>> {
        if !self.has_slice {
            return Vec::new();
        }
        vec![std::mem::take(self).pending]
    }
}
