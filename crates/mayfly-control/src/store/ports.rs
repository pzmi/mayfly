//! The ports `AggregateStore` reads time and randomness through, and the notifications it
//! gives after a commit (`doc/event-store.md` §3.7 step 6, §5.5).

use std::future::Future;

use mayfly_domain::{CommandId, Millis};

use crate::journal::{Appended, StoredKind, StreamId};

/// The control plane's clock (ADR 0020): wall-clock milliseconds since the Unix epoch, the
/// only source of `recorded_at` and of a command's `issued_at`. Nothing orders or decides by
/// it; it also paces the revision retry's backoff.
///
/// The production adapter reads the system clock and sleeps on the runtime's timer; tests use
/// `mayfly_testkit::clock::FakeClock`, which advances only when told or slept on, so no test
/// waits (ver §3.2).
pub trait Clock: Send + Sync {
    /// The current instant.
    fn now(&self) -> Millis;

    /// Waits `duration` (a fake clock advances itself instead).
    fn sleep(&self, duration: Millis) -> impl Future<Output = ()> + Send;
}

/// Uniform randomness for jitter (ver §3.3): the production adapter draws from the operating
/// system's generator; tests use the seeded `mayfly_testkit::clock::SeededRng`, which prints
/// nothing and repeats for the same seed.
pub trait Rng: Send + Sync {
    /// A value in `0..bound`, or 0 when `bound` is 0.
    fn below(&self, bound: u64) -> u64;
}

/// What a committed save wrote: the after-commit kick's content (`doc/event-store.md` §5.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Committed {
    /// The command whose unit committed.
    pub command_id: CommandId,
    /// What it appended, per stream: the streams a consumer's worker is kicked for.
    pub appended: Appended,
}

/// Notifications `AggregateStore` gives, each **at most once** and carrying no obligation
/// (`doc/event-store.md` §5.5): anything that must happen is a durable consumer, which finds
/// its work through the dispatch marks the commit wrote, so a notification lost with its
/// process loses nothing.
///
/// Implementations return at once (a kick into a bounded channel that drops when full, a
/// flag); they never block the writer and never fail it.
pub trait StoreNotifier: Send + Sync {
    /// A unit that appended at least one event committed: kick the workers of its streams and
    /// feed `events --follow`. Called once per committed save and never for a replayed receipt
    /// (pinned by `tests/store/save.rs::should_kick_once_after_commit_and_never_when_a_receipt_answers`).
    fn committed(&self, commit: &Committed);

    /// A stored event of `stream` has a kind this build does not decode: its command was
    /// refused, and the daemon must enter safe mode (`doc/event-store.md` §6.3 rule 4; pinned
    /// by `tests/store/save.rs::should_raise_safe_mode_when_a_stream_holds_an_unknown_kind`).
    fn unknown_kind(&self, stream: &StreamId, kind: &StoredKind);
}

/// A notifier that drops every notification: for a store nothing reacts to yet.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoNotifier;

impl StoreNotifier for NoNotifier {
    fn committed(&self, _commit: &Committed) {}

    fn unknown_kind(&self, _stream: &StreamId, _kind: &StoredKind) {}
}
