//! Append-only on-disk buffer of session events the kernel hasn't acked yet,
//! plus the outbound channel of the current connection.
//!
//! Layout: `<dir>/<session_id>.jsonl` (one `HostEvent` per line, append-only)
//! and `<dir>/<session_id>.ack` (the highest acked `seq`). A session's files
//! are deleted once it is finished and fully acked. When the buffer is over
//! its cap, the oldest finished sessions are dropped first; open sessions are
//! never dropped.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use sun_core::host_protocol::HostFrame;
use sun_core::{Error, HostEvent, Result, SessionId, SunEvent};
use tokio::sync::mpsc::UnboundedSender;

/// Default cap of the buffer directory.
pub const DEFAULT_CAP: u64 = 256 * 1024 * 1024;

/// The buffer. All methods are synchronous and short; file writes happen under
/// one lock so `seq` order on disk equals send order.
pub struct Buffer {
    dir: PathBuf,
    cap: u64,
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    logs: BTreeMap<SessionId, Log>,
    total: u64,
    outbound: Option<UnboundedSender<HostFrame>>,
}

struct Log {
    last_seq: u64,
    acked: u64,
    open: bool,
    bytes: u64,
}

impl Buffer {
    /// Opens (or creates) the buffer in `dir`. Sessions found on disk are from
    /// a previous host process, so they are finished.
    pub fn open(dir: impl Into<PathBuf>, cap: u64) -> Result<Self> {
        let dir = dir.into();
        fs::create_dir_all(&dir).map_err(io(&dir))?;
        let mut inner = Inner::default();
        for entry in fs::read_dir(&dir).map_err(io(&dir))? {
            let path = entry.map_err(io(&dir))?.path();
            if path.extension().is_none_or(|e| e != "jsonl") {
                continue;
            }
            let Some(id) = path
                .file_stem()
                .and_then(|s| s.to_str())
                .and_then(|s| s.parse::<SessionId>().ok())
            else {
                continue;
            };
            let bytes = fs::metadata(&path).map_err(io(&path))?.len();
            let last_seq = read_events(&path)?.last().map_or(0, |e| e.seq);
            let acked = fs::read_to_string(path.with_extension("ack"))
                .ok()
                .and_then(|s| s.trim().parse().ok())
                .unwrap_or(0);
            inner.total += bytes;
            inner.logs.insert(
                id,
                Log {
                    last_seq,
                    acked,
                    open: false,
                    bytes,
                },
            );
            if acked >= last_seq {
                remove(&dir, &mut inner, id);
            }
        }
        Ok(Self {
            dir,
            cap,
            inner: Mutex::new(inner),
        })
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Registers a new, open session.
    pub fn begin(&self, session: SessionId) {
        self.lock().logs.entry(session).or_insert(Log {
            last_seq: 0,
            acked: 0,
            open: true,
            bytes: 0,
        });
    }

    /// Numbers `event`, appends it to disk and sends it if connected.
    /// Returns its `seq`.
    pub fn append(&self, session: SessionId, event: SunEvent) -> Result<u64> {
        let mut inner = self.lock();
        let log = inner
            .logs
            .get_mut(&session)
            .ok_or_else(|| Error::NotFound(format!("buffered session {session}")))?;
        let host_event = HostEvent {
            session_id: session,
            seq: log.last_seq + 1,
            event,
        };
        let mut line = serde_json::to_vec(&host_event)
            .map_err(|e| Error::Protocol(format!("encode event: {e}")))?;
        line.push(b'\n');
        let path = self.dir.join(format!("{session}.jsonl"));
        // ponytail: no fsync per event; the buffer protects against kernel
        // restarts, not host power loss. fsync here if that matters.
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .and_then(|mut f| f.write_all(&line))
            .map_err(io(&path))?;
        log.last_seq = host_event.seq;
        log.bytes += line.len() as u64;
        inner.total += line.len() as u64;
        let seq = host_event.seq;
        send(&mut inner, HostFrame::Event(host_event));
        self.enforce_cap(&mut inner);
        Ok(seq)
    }

    /// Marks a session finished: it may be dropped once acked or over the cap.
    pub fn finish(&self, session: SessionId) {
        let mut inner = self.lock();
        if let Some(log) = inner.logs.get_mut(&session) {
            log.open = false;
            if log.acked >= log.last_seq {
                remove(&self.dir, &mut inner, session);
            }
        }
    }

    /// The kernel stored every event of `session` up to `seq`.
    pub fn ack(&self, session: SessionId, seq: u64) -> Result<()> {
        let mut inner = self.lock();
        let Some(log) = inner.logs.get_mut(&session) else {
            return Ok(()); // already dropped: nothing left to ack
        };
        if seq <= log.acked {
            return Ok(());
        }
        log.acked = seq.min(log.last_seq);
        if !log.open && log.acked >= log.last_seq {
            remove(&self.dir, &mut inner, session);
            return Ok(());
        }
        let path = self.dir.join(format!("{session}.ack"));
        fs::write(&path, log.acked.to_string()).map_err(io(&path))
    }

    /// Starts a connection: from now on frames go to `outbound`. Returns the
    /// `last_ack` map for `hello` and the unacked events to resend after it,
    /// taken under the same lock so nothing is lost or sent twice.
    pub fn connect(
        &self,
        outbound: UnboundedSender<HostFrame>,
    ) -> Result<(BTreeMap<SessionId, u64>, Vec<HostFrame>)> {
        let mut inner = self.lock();
        let mut last_ack = BTreeMap::new();
        let mut resend = Vec::new();
        for (id, log) in &inner.logs {
            last_ack.insert(*id, log.acked);
            if log.last_seq > log.acked {
                let path = self.dir.join(format!("{id}.jsonl"));
                resend.extend(
                    read_events(&path)?
                        .into_iter()
                        .filter(|e| e.seq > log.acked)
                        .map(HostFrame::Event),
                );
            }
        }
        inner.outbound = Some(outbound);
        Ok((last_ack, resend))
    }

    /// Sends an unbuffered frame (reply, stats) if connected.
    pub fn send(&self, frame: HostFrame) {
        send(&mut self.lock(), frame);
    }

    fn enforce_cap(&self, inner: &mut Inner) {
        while inner.total > self.cap {
            let Some(oldest) = inner
                .logs
                .iter()
                .find(|(_, log)| !log.open)
                .map(|(id, _)| *id)
            else {
                return; // only open sessions left: never drop them
            };
            remove(&self.dir, inner, oldest);
        }
    }
}

fn send(inner: &mut Inner, frame: HostFrame) {
    if let Some(tx) = &inner.outbound
        && tx.send(frame).is_err()
    {
        inner.outbound = None; // connection gone; frames wait on disk
    }
}

fn remove(dir: &Path, inner: &mut Inner, session: SessionId) {
    if let Some(log) = inner.logs.remove(&session) {
        inner.total -= log.bytes;
    }
    let _ = fs::remove_file(dir.join(format!("{session}.jsonl")));
    let _ = fs::remove_file(dir.join(format!("{session}.ack")));
}

/// Every parseable event in a session file; a torn last line is skipped.
fn read_events(path: &Path) -> Result<Vec<HostEvent>> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(io(path)(e)),
    };
    Ok(text
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect())
}

pub(crate) fn io(path: &Path) -> impl Fn(std::io::Error) -> Error + '_ {
    move |e| Error::Unavailable(format!("{}: {e}", path.display()))
}

/// Writes `text` to `path` with `mode` (set before any byte is written), so a
/// file holding a secret is never readable by others, not even briefly.
pub(crate) fn write_file(path: &Path, text: &str, mode: u32) -> Result<()> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(mode)
        .open(path)
        .map_err(io(path))?;
    file.write_all(text.as_bytes()).map_err(io(path))?;
    // `mode` only applies on create; an existing file keeps its own.
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).map_err(io(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("sun-host-{name}-{}", SessionId::generate()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn text(n: u64) -> SunEvent {
        SunEvent::Error {
            message: format!("event {n}"),
        }
    }

    #[test]
    fn resends_unacked_events_after_reopen() {
        let dir = temp_dir("reopen");
        let (a, b) = (SessionId::generate(), SessionId::generate());
        {
            let buffer = Buffer::open(&dir, DEFAULT_CAP).unwrap();
            buffer.begin(a);
            buffer.begin(b);
            for n in 1..=3 {
                assert_eq!(buffer.append(a, text(n)).unwrap(), n);
            }
            buffer.append(b, text(1)).unwrap();
            buffer.ack(a, 2).unwrap();
            buffer.ack(b, 1).unwrap(); // b still open: kept
        }
        let buffer = Buffer::open(&dir, DEFAULT_CAP).unwrap();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let (last_ack, resend) = buffer.connect(tx).unwrap();
        // b was fully acked and is finished after a restart: deleted.
        assert_eq!(last_ack, BTreeMap::from([(a, 2)]));
        let seqs: Vec<u64> = resend
            .iter()
            .map(|f| match f {
                HostFrame::Event(e) => e.seq,
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(seqs, [3]);
        buffer.ack(a, 3).unwrap();
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
    }

    #[test]
    fn cap_drops_oldest_finished_session_never_the_open_one() {
        // Every line has the same length here: measure one.
        let probe = Buffer::open(temp_dir("probe"), DEFAULT_CAP).unwrap();
        let id = SessionId::generate();
        probe.begin(id);
        probe.append(id, text(1)).unwrap();
        let line = probe.lock().total;

        let buffer = Buffer::open(temp_dir("cap"), line * 7 / 2).unwrap();
        // Session ids are ULIDs: only ids from different milliseconds are ordered.
        let [old, newer, open] = [
            "01K00000000000000000000001",
            "01K00000000000000000000002",
            "01K00000000000000000000003",
        ]
        .map(|s| s.parse::<SessionId>().unwrap());
        for id in [old, newer, open] {
            buffer.begin(id);
            buffer.append(id, text(1)).unwrap();
        }
        buffer.finish(old);
        buffer.finish(newer);
        let ids = |buffer: &Buffer| buffer.lock().logs.keys().copied().collect::<Vec<_>>();

        buffer.append(open, text(2)).unwrap(); // 4 lines > 3.5: drop one
        assert_eq!(ids(&buffer), [newer, open], "oldest finished dropped first");
        for n in 3..=9 {
            buffer.append(open, text(n)).unwrap();
        }
        assert_eq!(ids(&buffer), [open], "the open session is never dropped");
    }
}
