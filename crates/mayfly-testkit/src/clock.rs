//! Fakes for `AggregateStore`'s ports (ver §3.2, §3.3): a clock that moves only when told or
//! slept on, a seeded random source, and a notifier that records what it was told.

use std::sync::{Arc, Mutex, PoisonError};

use mayfly_control::journal::{StoredKind, StreamId};
use mayfly_control::store::{Clock, Committed, Rng, StoreNotifier};
use mayfly_domain::Millis;

/// A clock at a given Unix-epoch instant (ADR 0020) that never moves by itself. `sleep`
/// returns at once and advances it by the duration, recording each sleep, so a backoff is
/// observable and no test waits. Clones share one clock.
#[derive(Debug, Clone, Default)]
pub struct FakeClock {
    shared: Arc<Mutex<(u64, Vec<Millis>)>>,
}

impl FakeClock {
    /// A clock reading `now`.
    #[must_use]
    pub fn at(now: Millis) -> Self {
        Self {
            shared: Arc::new(Mutex::new((now.get(), Vec::new()))),
        }
    }

    /// Moves the clock forward by `by`.
    pub fn advance(&self, by: Millis) {
        let mut shared = self.shared.lock().unwrap_or_else(PoisonError::into_inner);
        shared.0 = shared.0.saturating_add(by.get());
    }

    /// Every duration slept so far, in order.
    #[must_use]
    pub fn sleeps(&self) -> Vec<Millis> {
        self.shared
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .1
            .clone()
    }
}

impl Clock for FakeClock {
    fn now(&self) -> Millis {
        Millis::new(self.shared.lock().unwrap_or_else(PoisonError::into_inner).0)
    }

    async fn sleep(&self, duration: Millis) {
        let mut shared = self.shared.lock().unwrap_or_else(PoisonError::into_inner);
        shared.0 = shared.0.saturating_add(duration.get());
        shared.1.push(duration);
    }
}

/// A deterministic random source (SplitMix64) from an explicit seed: the same seed gives the
/// same draws (pinned by `tests/clock.rs::should_repeat_draws_when_the_seed_is_the_same`).
/// Clones share one sequence.
#[derive(Debug, Clone)]
pub struct SeededRng {
    state: Arc<Mutex<u64>>,
}

impl SeededRng {
    /// A source seeded with `seed`.
    #[must_use]
    pub fn new(seed: u64) -> Self {
        Self {
            state: Arc::new(Mutex::new(seed)),
        }
    }

    fn next(&self) -> u64 {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = *state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

impl Rng for SeededRng {
    fn below(&self, bound: u64) -> u64 {
        if bound == 0 {
            return 0;
        }
        self.next() % bound
    }
}

/// One notification a [`RecordingNotifier`] received.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notification {
    /// `StoreNotifier::committed`.
    Committed(Committed),
    /// `StoreNotifier::unknown_kind`.
    UnknownKind {
        /// The stream.
        stream: StreamId,
        /// The kind.
        kind: StoredKind,
    },
}

/// A notifier that records every notification, in order. Clones share one record.
#[derive(Debug, Clone, Default)]
pub struct RecordingNotifier {
    seen: Arc<Mutex<Vec<Notification>>>,
}

impl RecordingNotifier {
    /// A notifier that has seen nothing.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Everything seen so far, in order.
    #[must_use]
    pub fn seen(&self) -> Vec<Notification> {
        self.seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn push(&self, notification: Notification) {
        self.seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(notification);
    }
}

impl StoreNotifier for RecordingNotifier {
    fn committed(&self, commit: &Committed) {
        self.push(Notification::Committed(commit.clone()));
    }

    fn unknown_kind(&self, stream: &StreamId, kind: &StoredKind) {
        self.push(Notification::UnknownKind {
            stream: stream.clone(),
            kind: kind.clone(),
        });
    }
}
