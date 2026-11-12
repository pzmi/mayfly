//! The production adapters of the time and randomness ports both planes read (ADR 0020): the
//! only place the daemon reads the system clock, its monotonic clock or the operating system's
//! generator. Tests of the planes use the kits' fakes; these are exercised by the daemon's
//! integration test and pinned below where they promise something.
//!
//! - [`SystemClock`]: the control plane's `Clock` (wall-clock milliseconds since the Unix
//!   epoch, the runtime's timer for sleeps); also the delivery worker's `DeliveryTimer`.
//! - [`AnchoredStoreClock`]: the SQLite delivery ledger's store clock (`StoreClock`, ES-05F,
//!   CP-04): the wall clock read once at start-up, advanced by the monotonic clock, so an NTP
//!   step never ends leases early (a forward step would cause takeovers and duplicates).
//! - [`OsRng`]: the control plane's `Rng` (jitter only): a SplitMix64 sequence seeded from the
//!   operating system's generator.
//! - [`DataClock`]: the data plane's `ProcessClock`: wall, monotonic, and a sleep on the
//!   monotonic clock.
//! - [`RandomObservationIds`]: the data plane's `ObservationIds`: 64 random high bits per
//!   process and a counter, so no id repeats within a process and two processes collide only
//!   with a 2⁻⁶⁴ chance (and the inbox key carries the session's epoch besides, CP-04).

use std::future::Future;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use mayfly_control::delivery::StoreClock;
use mayfly_control::store::{Clock, Rng};
use mayfly_data::{ObservationIds, ProcessClock};
use mayfly_domain::{Millis, ObservationId};

use crate::identity::random_bytes;

fn wall_now() -> Millis {
    Millis::new(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX)),
    )
}

fn since(origin: Instant) -> Millis {
    Millis::new(u64::try_from(origin.elapsed().as_millis()).unwrap_or(u64::MAX))
}

/// The control plane's clock: the system's wall clock, sleeping on the runtime's timer.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Millis {
        wall_now()
    }

    fn sleep(&self, duration: Millis) -> impl Future<Output = ()> + Send {
        tokio::time::sleep(std::time::Duration::from_millis(duration.get()))
    }
}

/// The delivery ledger's store clock: the wall clock at construction plus the monotonic time
/// since. It never steps with the wall clock; it drifts from it by the host's clock corrections
/// over the daemon's life, which only shifts when leases end, never lets two workers hold one
/// (every claim takes a higher fence, ES-05). Pinned by
/// `tests::should_never_step_back_when_the_store_clock_is_read_twice`.
#[derive(Debug, Clone, Copy)]
pub struct AnchoredStoreClock {
    wall: Millis,
    origin: Instant,
}

impl AnchoredStoreClock {
    /// Anchored at the wall clock now.
    #[must_use]
    pub fn now_anchored() -> Self {
        Self {
            wall: wall_now(),
            origin: Instant::now(),
        }
    }
}

impl StoreClock for AnchoredStoreClock {
    fn now(&self) -> Millis {
        Millis::new(self.wall.get().saturating_add(since(self.origin).get()))
    }
}

/// Jitter from a SplitMix64 sequence seeded from the operating system's generator.
#[derive(Debug)]
pub struct OsRng {
    state: Mutex<u64>,
}

impl OsRng {
    /// Seeded from the operating system's generator.
    ///
    /// # Errors
    /// The generator could not be read.
    pub fn seeded() -> std::io::Result<Self> {
        Ok(Self {
            state: Mutex::new(u64::from_le_bytes(random_bytes()?)),
        })
    }
}

impl Rng for OsRng {
    fn below(&self, bound: u64) -> u64 {
        if bound == 0 {
            return 0;
        }
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = *state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        (z ^ (z >> 31)) % bound
    }
}

/// The data plane's clocks: the system's wall clock, and a monotonic clock counted from the
/// clock's creation.
#[derive(Debug, Clone, Copy)]
pub struct DataClock {
    origin: Instant,
}

impl DataClock {
    /// A monotonic clock starting at zero now.
    #[must_use]
    pub fn started() -> Self {
        Self {
            origin: Instant::now(),
        }
    }
}

impl ProcessClock for DataClock {
    fn wall(&self) -> Millis {
        wall_now()
    }

    fn monotonic(&self) -> Millis {
        since(self.origin)
    }

    fn sleep_until(&self, at: Millis) -> impl Future<Output = ()> + Send {
        let deadline = self
            .origin
            .checked_add(std::time::Duration::from_millis(at.get()))
            .unwrap_or_else(Instant::now);
        tokio::time::sleep_until(tokio::time::Instant::from_std(deadline))
    }
}

/// Observation ids: random high bits per process, a counter below.
#[derive(Debug)]
pub struct RandomObservationIds {
    high: u64,
    next: AtomicU64,
}

impl RandomObservationIds {
    /// A fresh id space from the operating system's generator.
    ///
    /// # Errors
    /// The generator could not be read.
    pub fn seeded() -> std::io::Result<Self> {
        Ok(Self {
            high: u64::from_le_bytes(random_bytes()?),
            next: AtomicU64::new(1),
        })
    }
}

impl ObservationIds for RandomObservationIds {
    fn next_id(&self) -> ObservationId {
        let low = self.next.fetch_add(1, Ordering::Relaxed);
        ObservationId::from_u128((u128::from(self.high) << 64) | u128::from(low))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    #[test]
    fn should_never_step_back_when_the_store_clock_is_read_twice() {
        // given
        let clock = AnchoredStoreClock::now_anchored();

        // when
        let first = clock.now();
        let second = clock.now();

        // then
        assert!(second >= first);
        assert!(first >= clock.wall);
    }

    #[test]
    fn should_stay_below_the_bound_when_jitter_is_drawn() {
        // given
        let rng = OsRng::seeded().unwrap_or_else(|e| panic!("{e}"));

        // when
        let drawn: Vec<u64> = (0..1000).map(|_| rng.below(7)).collect();

        // then
        assert!(drawn.iter().all(|n| *n < 7));
        assert_eq!(rng.below(0), 0);
        assert!(drawn.iter().collect::<BTreeSet<_>>().len() > 1);
    }

    #[test]
    fn should_mint_distinct_ids_when_observations_are_numbered() {
        // given
        let ids = RandomObservationIds::seeded().unwrap_or_else(|e| panic!("{e}"));

        // when
        let minted: BTreeSet<ObservationId> = (0..100).map(|_| ids.next_id()).collect();

        // then
        assert_eq!(minted.len(), 100);
    }
}
