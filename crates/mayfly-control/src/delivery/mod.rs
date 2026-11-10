//! Durable delivery (`doc/event-store.md` §3.4, §5; ADR 0034): the at-least-once readers of
//! every stream, without a global order and without polling the store on a timer.
//!
//! - **Marks.** Every unit the journal commits marks each *registered* consumer for each stream
//!   it appended, at the stream's new head, in the same transaction as the events (ES-I1; the
//!   seam is inside `Journal::append`, so `AggregateStore::save` stays the only write path and
//!   the unit names no consumer). A stream written before a consumer registered is marked by
//!   its bootstrap.
//! - **Claims and leases.** A worker claims marked streams whose lease is absent or ran out by
//!   the **store clock** ([`StoreClock`]), never the worker's; each claim takes the next
//!   [`LeaseFence`]. A claim reads the stream after its checkpoint through its mark and, per
//!   event, renews the lease, hands the event to the consumer, then advances the checkpoint to
//!   that revision, clearing the mark only up to it (ES-I14, ES-I15).
//! - **Failures.** A consumer error releases the claim with a jittered backoff (`retry_after`)
//!   and leaves the checkpoint: the same event comes again, never the next (DC-18). A lost lease
//!   ends that claim, not the drain (DC-23).
//! - **Waking.** [`DeliveryWorker::catch_up`] at start-up and on a coarse timer of the
//!   supervision tree; [`Kicks`], the after-commit notification, for latency. A kick carries no
//!   obligation and is never replayed: a lost kick loses nothing, the mark is durable (§5.5).
//!
//! The model is `formal/event_store.qnt` (its consumer actions `register`, `bootstrap`,
//! `claim`, `effect`, `advance`, `release`, `failHandle`, `expire`, `crashWorker`, `retire`;
//! invariants `ES_I14_durableDelivery`, `ES_I15_noSkip`, `directivesFromJournal`). Where the code
//! differs from it, `doc/event-store.md` §5.1 says how and why ("As built, ES-05").
//!
//! Implementations of [`DeliveryLedger`]: `mayfly_testkit::delivery::MemoryLedger` (over
//! `MemoryJournal`) and the SQLite adapter's `SqliteLedger`; both pass the delivery contract,
//! `mayfly_testkit::contracts::delivery` (DC-01..DC-25), whose own tests show the broken
//! ledgers of `doc/event-store.md` §7.6 failing named cases.

mod kicks;
mod relay;
mod values;
mod worker;

use std::future::Future;

use mayfly_domain::Millis;

use crate::journal::{StorageError, StreamId, StreamRevision};

pub use kicks::Kicks;
pub use relay::{DirectiveRelay, DirectiveSink, RelayedDirective, SinkError};
pub use values::{
    Claim, ConsumerError, ConsumerName, ConsumerState, Delivery, DeliveryKey, LeaseError,
    LeaseFence,
};
pub use worker::{DeliveryConfig, DeliveryError, DeliveryWorker, DrainReport};

/// The clock that judges lease expiry and `retry_after` (`doc/event-store.md` §2.1): the
/// database's own on PostgreSQL, an injected one on SQLite and in memory, never a worker's.
/// Milliseconds since the Unix epoch, like the control plane's `Clock` (ADR 0020); a store
/// clock that steps back only delays claims, it never lets two workers hold one lease, since
/// every claim takes a strictly higher fence.
pub trait StoreClock: Send + Sync {
    /// The store's current instant.
    fn now(&self) -> Millis;
}

/// The delivery ledger: registrations, marks, checkpoints and fenced leases per
/// `(consumer, stream)` (`doc/event-store.md` §3.4, §4.1).
///
/// A native async trait used through generics (D8). Every method is one short transaction of
/// its own (§3.7's table): it takes a lease row, then its checkpoint, then its mark, and never a
/// stream head, so no delivery transaction waits on a writer of the journal (ES-I16). Nothing
/// here reads the journal in a global order (ES-I17): marks are found by `(consumer, stream_id)`
/// key, and `retry_at` is a filter, never an order.
///
/// Pinned by `crates/mayfly-testkit/src/contracts/delivery.rs` (DC-01..DC-25), run on memory by
/// `crates/mayfly-testkit/tests/delivery_contract.rs` and on SQLite, in the journal contract's
/// five modes, by `crates/mayfly-adapter-sqlite/tests/delivery_contract.rs`.
pub trait DeliveryLedger: Send + Sync {
    /// Every registered consumer, by name.
    ///
    /// # Errors
    /// [`StorageError`] when the store fails.
    fn registered(&self) -> impl Future<Output = Result<Vec<ConsumerState>, StorageError>> + Send;

    /// Registers `consumer` (registering twice is one consumer, DC-05). From the commit of the
    /// registration on, every unit marks it; streams written before are left to
    /// [`DeliveryLedger::bootstrap`].
    ///
    /// # Errors
    /// [`StorageError`] when the store fails.
    fn register(
        &self,
        consumer: &ConsumerName,
    ) -> impl Future<Output = Result<(), StorageError>> + Send;

    /// Marks, for `consumer`, up to `limit` existing streams with an id above `after` at their
    /// heads, where the head is past its checkpoint: enumeration by key, in byte order of the
    /// id (DC-04). Returns the last stream of a full page, `None` at the end, when the consumer
    /// is recorded as bootstrapped. Idempotent: a mark only rises.
    ///
    /// # Errors
    /// [`StorageError`] when the store fails (`unknown_consumer` for one never registered).
    fn bootstrap(
        &self,
        consumer: &ConsumerName,
        after: Option<&StreamId>,
        limit: u32,
    ) -> impl Future<Output = Result<Option<StreamId>, StorageError>> + Send;

    /// Claims up to `limit` streams marked past their checkpoint whose lease is absent or ran
    /// out and whose `retry_at` has passed, by the store clock; only `only` when given (a kick,
    /// DC-10). Each claim takes the stream's next fence (DC-20) and a lease until store time
    /// plus `ttl`. Discovery is by key, round-robin from the last stream this handle claimed,
    /// never by time.
    ///
    /// # Errors
    /// [`StorageError`] when the store fails.
    fn claim(
        &self,
        consumer: &ConsumerName,
        ttl: Millis,
        limit: u32,
        only: Option<&StreamId>,
    ) -> impl Future<Output = Result<Vec<Claim>, StorageError>> + Send;

    /// Extends the claim's lease to store time plus `ttl`, while it lives.
    ///
    /// # Errors
    /// [`LeaseError::Lost`] once the lease ran out or was taken over (DC-22).
    fn renew(
        &self,
        claim: &Claim,
        ttl: Millis,
    ) -> impl Future<Output = Result<(), LeaseError>> + Send;

    /// Moves the checkpoint to `to` (only up, DC-13), clears the mark only if it is at most `to`
    /// (a mark set during processing survives, DC-08), and extends the lease, in one
    /// transaction fenced by the live lease (DC-11, DC-12).
    ///
    /// # Errors
    /// [`LeaseError::Lost`] when the lease is not the claim's or ran out: nothing moved.
    fn advance(
        &self,
        claim: &Claim,
        to: StreamRevision,
        ttl: Millis,
    ) -> impl Future<Output = Result<(), LeaseError>> + Send;

    /// Ends the claim; with `retry_after`, the mark stays unclaimable until store time passes
    /// now plus `retry_after` (DC-25). The checkpoint does not move.
    ///
    /// # Errors
    /// [`LeaseError::Lost`] when the lease is not the claim's or ran out: nothing changed.
    fn release(
        &self,
        claim: &Claim,
        retry_after: Option<Millis>,
    ) -> impl Future<Output = Result<(), LeaseError>> + Send;

    /// Retires `consumer`: its registration, marks and checkpoints are removed and its leases
    /// end, keeping only their fences, so a consumer registered again under the name is a new
    /// one that is bootstrapped and takes higher fences (DC-06; model `retire`).
    ///
    /// # Errors
    /// [`StorageError`] when the store fails.
    fn retire(
        &self,
        consumer: &ConsumerName,
    ) -> impl Future<Output = Result<(), StorageError>> + Send;
}

/// A durable consumer: handles each event of every stream at least once, in revision order
/// within a claim (ES-I14).
///
/// `handle` must be idempotent by [`Delivery::key`] or fence its own resource by
/// [`Delivery::fence`]: a takeover may repeat an event whose effect was in flight when its lease
/// ran out (DC-15). A consumer that issues commands treats a refused outcome as handled: a
/// refusal consumes no inbox key (ADR 0041 rule 6, the ES-03 note), so "not in the inbox" never
/// means "not yet decided", and a redelivery is answered by the refusal receipt.
pub trait Consumer: Send + Sync {
    /// The consumer's name, `[a-z][a-z0-9_]{0,63}`.
    const NAME: &'static str;

    /// Handles one event; an error delivers it again after a backoff (ES-I15).
    fn handle(
        &self,
        delivery: Delivery<'_>,
    ) -> impl Future<Output = Result<(), ConsumerError>> + Send;
}
