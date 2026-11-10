//! Time and ids for tests: a clock that moves only when told, ids in sequence.

use std::future::Future;
use std::sync::{Arc, Mutex, PoisonError};

use mayfly_data::{ObservationIds, ProcessClock};
use mayfly_domain::{Millis, ObservationId};
use tokio::sync::watch;

/// Clocks that move only when told: [`FakeClock::advance`] moves both, [`FakeClock::set_wall`]
/// steps the wall clock alone, as an NTP correction or an operator would. A sleeper waits on
/// the monotonic reading and wakes once it reaches its instant.
#[derive(Debug, Clone)]
pub struct FakeClock {
    now: Arc<watch::Sender<u64>>,
    wall_offset: Arc<Mutex<i128>>,
}

impl FakeClock {
    /// Clocks both reading `start`.
    #[must_use]
    pub fn new(start: Millis) -> Self {
        Self {
            now: Arc::new(watch::Sender::new(start.get())),
            wall_offset: Arc::default(),
        }
    }

    /// Moves both readings forward by `by`, waking every sleeper the monotonic one reaches.
    pub fn advance(&self, by: Millis) {
        self.now
            .send_modify(|now| *now = now.saturating_add(by.get()));
    }

    /// Steps the wall clock to `to`; the monotonic clock does not move.
    pub fn set_wall(&self, to: Millis) {
        let monotonic = i128::from(*self.now.borrow());
        *self
            .wall_offset
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = i128::from(to.get()) - monotonic;
    }
}

impl ProcessClock for FakeClock {
    fn wall(&self) -> Millis {
        let offset = *self
            .wall_offset
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let wall = i128::from(*self.now.borrow()) + offset;
        Millis::new(u64::try_from(wall.max(0)).unwrap_or(u64::MAX))
    }

    fn monotonic(&self) -> Millis {
        Millis::new(*self.now.borrow())
    }

    fn sleep_until(&self, at: Millis) -> impl Future<Output = ()> + Send {
        let mut reading = self.now.subscribe();
        async move {
            loop {
                // Read in its own statement: the guard must not live across the await, or the
                // clock could never advance.
                let now = *reading.borrow_and_update();
                if now >= at.get() || reading.changed().await.is_err() {
                    return;
                }
            }
        }
    }
}

/// Observation ids 1, 2, 3, …
#[derive(Debug, Clone, Default)]
pub struct SequentialIds {
    next: Arc<Mutex<u128>>,
}

impl ObservationIds for SequentialIds {
    fn next_id(&self) -> ObservationId {
        let mut next = self.next.lock().unwrap_or_else(PoisonError::into_inner);
        *next += 1;
        ObservationId::from_u128(*next)
    }
}
