//! Live logs as SSE (**Observer**): a run's timeline and a task's activity.
//! Every entry with `seq > after`, then live ones, each with `id: <seq>` so
//! an `EventSource` reconnect resumes from `Last-Event-ID`. Nothing is
//! skipped or repeated: subscribe first, page the log, then take live
//! entries above the cursor; on lag, page the log again.

use std::collections::VecDeque;
use std::convert::Infallible;
use std::future::Future;
use std::time::Duration;

use axum::response::sse::{Event, KeepAlive, Sse};
use futures_util::Stream;
use futures_util::stream;
use serde::Serialize;
use sun_core::model::{EventRecord, TaskActivity};
use sun_core::{RunId, TaskId};
use sun_engine::Engine;
use tokio::sync::broadcast::Receiver;
use tokio::sync::broadcast::error::RecvError;

use crate::wire;

const PAGE: u32 = 500;

/// One append-only log a stream follows.
pub(crate) trait Feed: Send + 'static {
    /// An entry, as stored and broadcast.
    type Item: Clone + Send + 'static;
    /// Its position in the log.
    fn seq(item: &Self::Item) -> i64;
    /// True when a broadcast entry belongs to this log.
    fn mine(&self, item: &Self::Item) -> bool;
    /// Entries with `seq > after`, oldest first; `None` ends the stream.
    fn page(&self, after: i64) -> impl Future<Output = Option<Vec<Self::Item>>> + Send;
    /// What the client receives.
    fn view(item: Self::Item) -> impl Serialize;
}

/// A run's event log.
pub(crate) struct RunFeed {
    pub(crate) engine: Engine,
    pub(crate) run_id: RunId,
}

impl Feed for RunFeed {
    type Item = EventRecord;

    fn seq(item: &EventRecord) -> i64 {
        item.seq
    }

    fn mine(&self, item: &EventRecord) -> bool {
        item.run_id == self.run_id
    }

    async fn page(&self, after: i64) -> Option<Vec<EventRecord>> {
        // A failed read ends the stream; the client reconnects.
        self.engine.events(self.run_id, after, PAGE).await.ok()
    }

    fn view(item: EventRecord) -> impl Serialize {
        wire::event_view(item)
    }
}

/// A task's activity log.
pub(crate) struct ActivityFeed {
    pub(crate) engine: Engine,
    pub(crate) task_id: TaskId,
}

impl Feed for ActivityFeed {
    type Item = TaskActivity;

    fn seq(item: &TaskActivity) -> i64 {
        item.seq
    }

    fn mine(&self, item: &TaskActivity) -> bool {
        item.task_id == self.task_id
    }

    async fn page(&self, after: i64) -> Option<Vec<TaskActivity>> {
        self.engine.activity(self.task_id, after).await.ok()
    }

    fn view(item: TaskActivity) -> impl Serialize {
        item
    }
}

struct Cursor<F: Feed> {
    feed: F,
    after: i64,
    live: Receiver<F::Item>,
    ready: VecDeque<F::Item>,
    /// Read the log before taking live entries (start, and after a lag).
    catch_up: bool,
}

impl<F: Feed> Cursor<F> {
    /// The next entry to send, or `None` when the engine is gone.
    async fn next(&mut self) -> Option<F::Item> {
        loop {
            if let Some(item) = self.ready.pop_front() {
                self.after = F::seq(&item);
                return Some(item);
            }
            if self.catch_up {
                let page = self.feed.page(self.after).await?;
                self.catch_up = page.len() >= PAGE as usize;
                self.ready.extend(page);
                continue;
            }
            match self.live.recv().await {
                Ok(item) if self.feed.mine(&item) && F::seq(&item) > self.after => {
                    self.ready.push_back(item);
                }
                Ok(_) => {}
                Err(RecvError::Lagged(_)) => self.catch_up = true,
                Err(RecvError::Closed) => return None,
            }
        }
    }
}

/// Streams `feed` from `after`; `live` must be subscribed before any paging.
pub(crate) fn stream<F: Feed>(
    feed: F,
    live: Receiver<F::Item>,
    after: i64,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let cursor = Cursor {
        feed,
        after,
        live,
        ready: VecDeque::new(),
        catch_up: true,
    };
    let events = stream::unfold(cursor, |mut cursor| async move {
        let item = cursor.next().await?;
        let seq = F::seq(&item);
        let event = Event::default()
            .id(seq.to_string())
            .json_data(F::view(item))
            .unwrap_or_else(|_| Event::default().id(seq.to_string()).data("{}"));
        Some((Ok(event), cursor))
    });
    Sse::new(events).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}
