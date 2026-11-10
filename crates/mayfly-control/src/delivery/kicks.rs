//! Kicks: the after-commit wake-up of a consumer's worker (`doc/event-store.md` §5.1, §5.5).

use std::collections::BTreeSet;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll, Waker};

use crate::journal::{StoredKind, StreamId};
use crate::store::{Committed, StoreNotifier};

#[derive(Debug, Default)]
struct Pending {
    streams: BTreeSet<StreamId>,
    waker: Option<Waker>,
    dropped: u64,
}

/// The streams committed units appended since a consumer's worker last looked: a bounded set
/// that `AggregateStore` fills after each commit (as its [`StoreNotifier`]) and the worker takes
/// ([`crate::delivery::DeliveryWorker::drain_kicked`]). Clones share one set.
///
/// A kick is a hint and carries no obligation (§5.5): when the set is full a kick is dropped
/// and counted, and a crash loses the set; either way the stream stays marked in the ledger and
/// the next `catch_up` delivers it. Nothing fills the set but a commit: no kick is ever derived
/// from the journal, so a restart replays none (the model's `reactorsNeverReplayed`; pinned by
/// `tests/store/delivery.rs::should_lose_only_latency_when_a_kick_is_lost`).
#[derive(Clone)]
pub struct Kicks {
    pending: Arc<Mutex<Pending>>,
    capacity: usize,
}

impl fmt::Debug for Kicks {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Kicks")
            .field("capacity", &self.capacity)
            .finish_non_exhaustive()
    }
}

impl Kicks {
    /// An empty set holding at most `capacity` streams.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            pending: Arc::default(),
            capacity,
        }
    }

    fn lock(&self) -> MutexGuard<'_, Pending> {
        self.pending.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Notes that `stream` moved; never blocks. A stream already pending is one kick.
    pub fn kick(&self, stream: &StreamId) {
        let waker = {
            let mut pending = self.lock();
            if !pending.streams.contains(stream) {
                if pending.streams.len() >= self.capacity {
                    pending.dropped += 1;
                    return;
                }
                pending.streams.insert(stream.clone());
            }
            pending.waker.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    /// Every pending stream, in `StreamId` order; the set is empty afterwards.
    #[must_use]
    pub fn take(&self) -> Vec<StreamId> {
        std::mem::take(&mut self.lock().streams)
            .into_iter()
            .collect()
    }

    /// Kicks dropped because the set was full.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.lock().dropped
    }

    /// Completes once at least one stream is pending (at once if one is).
    pub fn wait(&self) -> impl Future<Output = ()> + Send + '_ {
        Wait(self)
    }
}

struct Wait<'a>(&'a Kicks);

impl Future for Wait<'_> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let mut pending = self.0.lock();
        if pending.streams.is_empty() {
            pending.waker = Some(cx.waker().clone());
            Poll::Pending
        } else {
            Poll::Ready(())
        }
    }
}

impl StoreNotifier for Kicks {
    fn committed(&self, commit: &Committed) {
        for appended in &commit.appended.streams {
            self.kick(&appended.stream);
        }
    }

    fn unknown_kind(&self, _stream: &StreamId, _kind: &StoredKind) {}
}
