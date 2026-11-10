//! `DeliveryWorker`: drains a consumer's claimed streams (`doc/event-store.md` §3.4, §5.1).
//!
//! It maps the model's consumer actions one to one (`formal/event_store.qnt`): a `claim` per
//! [`DeliveryLedger::claim`] of one stream, taken when the drain reaches it (the claim fixes the
//! revision it reads through, the model's `through`); per event, `effect` is
//! [`DeliveryLedger::renew`] then [`Consumer::handle`], and `advance` is
//! [`DeliveryLedger::advance`] to that event's revision, never to the head; a consumer error, or
//! a `handle` cut at its deadline, is `failHandle` ([`DeliveryLedger::release`] with a backoff);
//! the end of the read is `release`. The worker starts no task and holds nothing between calls
//! but its books (failure counts, reported short reads): the supervision tree (RT-02) owns the loop that calls it, so stopping
//! it is dropping the future.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::future::{Future, poll_fn};
use std::pin::pin;
use std::sync::{Mutex, PoisonError};
use std::task::Poll;

use mayfly_domain::Millis;

use super::kicks::Kicks;
use super::values::{ConsumerName, DeliveryKey};
use super::{Claim, Consumer, Delivery, DeliveryLedger, DeliveryTimer, LeaseError};
use crate::journal::{Journal, StorageError, StreamId, StreamRevision};
use crate::store::Rng;

/// How a worker claims and backs off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeliveryConfig {
    /// How long a claim's lease lives without a renewal (store clock).
    pub lease_ttl: Millis,
    /// The longest one `handle` may take: past it the worker drops the call and releases the
    /// claim with a backoff, so the event comes again rather than under a dead lease. Clamped to
    /// [`DeliveryConfig::handle_budget`], below the lease.
    pub handle_deadline: Millis,
    /// Streams claimed per round, each claimed only when the drain reaches it.
    pub claim_limit: u32,
    /// Streams marked per bootstrap page.
    pub bootstrap_page: u32,
    /// The first backoff after a consumer error; it doubles with each further failure of the
    /// stream, up to `backoff_cap`.
    pub backoff_base: Millis,
    /// The largest backoff.
    pub backoff_cap: Millis,
    /// Claim rounds per drain: a drain returns after this many, so one call never spins; the
    /// next kick or `catch_up` goes on where it stopped (the marks are durable).
    pub max_rounds: u32,
}

impl Default for DeliveryConfig {
    /// A 30 s lease, a 20 s handle deadline, 16 streams a round, pages of 256, a backoff from
    /// 100 ms to 60 s, 64 rounds.
    fn default() -> Self {
        Self {
            lease_ttl: Millis::new(30_000),
            handle_deadline: Millis::new(20_000),
            claim_limit: 16,
            bootstrap_page: 256,
            backoff_base: Millis::new(100),
            backoff_cap: Millis::new(60_000),
            max_rounds: 64,
        }
    }
}

impl DeliveryConfig {
    /// The deadline the worker puts on each `handle`: `handle_deadline`, at most three quarters
    /// of `lease_ttl` and at least 1 ms. The quarter left is the margin for the renewal before
    /// the call and the advance after it, which must land while the lease lives: a `handle`
    /// that ran past the lease could only end in a lost claim and the same event delivered
    /// again, round after round (pinned by DC-26, `slow_handle_cut`).
    #[must_use]
    pub const fn handle_budget(&self) -> Millis {
        let ceiling = self.lease_ttl.get() - self.lease_ttl.get() / 4;
        let budget = if self.handle_deadline.get() < ceiling {
            self.handle_deadline.get()
        } else {
            ceiling
        };
        Millis::new(if budget == 0 { 1 } else { budget })
    }
}

/// What a drain did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DrainReport {
    /// Events handled and advanced to.
    pub delivered: u64,
    /// Streams released after a consumer error, with the backoff each got, in drain order.
    pub failed: Vec<(StreamId, Millis)>,
    /// Streams whose claim was lost to expiry or a takeover, in drain order.
    pub lost: Vec<StreamId>,
    /// Streams whose journal read ended short of their mark, with the backoff each got: a
    /// storage inconsistency (the mark names a revision the journal does not return). Each is
    /// released with a backoff, so a drain never spins on it, and logged at ERROR on the first
    /// short read of a run (until the stream advances or is drained to its mark).
    pub inconsistent: Vec<(StreamId, Millis)>,
}

impl DrainReport {
    fn absorb(&mut self, other: Self) {
        self.delivered += other.delivered;
        self.failed.extend(other.failed);
        self.lost.extend(other.lost);
        self.inconsistent.extend(other.inconsistent);
    }
}

/// Why a delivery call stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryError {
    /// The consumer's `NAME` is not `[a-z][a-z0-9_]{0,63}`.
    InvalidConsumerName,
    /// The store failed; what was advanced stays advanced, the rest is still marked.
    Storage(StorageError),
}

impl fmt::Display for DeliveryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConsumerName => f.write_str("invalid consumer name"),
            Self::Storage(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for DeliveryError {}

impl From<StorageError> for DeliveryError {
    fn from(e: StorageError) -> Self {
        Self::Storage(e)
    }
}

/// The most `(consumer, stream)` entries each of a worker's books keeps (failure counts, and
/// inconsistencies reported). Past it, a new entry evicts the least recently touched one, never
/// a fixed key range: an evicted stream's next backoff starts again from its base (or its next
/// short read is reported again), which is harmless, and the books stay bounded whatever fails.
const MAX_BOOK_ENTRIES: usize = 1024;

/// A consumer's stream: what the worker's books are keyed by.
type BookKey = (ConsumerName, StreamId);

/// A map of at most `cap` entries that, when full, forgets its least recently touched entry.
///
/// Eviction by recency rather than by key: evicting the smallest key (`pop_first`, ES-05F)
/// always took the low-key streams' counts, so a low-key stream that kept failing lost its
/// backoff escalation whenever many others failed. A linear scan finds the oldest entry; it runs
/// only when a new key arrives at a full map, which a healthy worker never sees. Pinned by
/// `tests::should_evict_the_least_recently_touched_key_when_the_book_is_full`.
#[derive(Debug)]
struct Recent<V> {
    entries: BTreeMap<BookKey, (u64, V)>,
    tick: u64,
    cap: usize,
}

impl<V: Default> Recent<V> {
    const fn new(cap: usize) -> Self {
        Self {
            entries: BTreeMap::new(),
            tick: 0,
            cap,
        }
    }

    /// The entry of `key`, created with `V::default()` when absent (evicting the least recently
    /// touched entry of a full map), and whether it was created; either way it becomes the most
    /// recently touched.
    fn touch(&mut self, key: BookKey) -> (&mut V, bool) {
        self.tick = self.tick.wrapping_add(1);
        let fresh = !self.entries.contains_key(&key);
        if fresh && self.entries.len() >= self.cap.max(1) {
            let oldest = self
                .entries
                .iter()
                .min_by_key(|(_, (touched, _))| *touched)
                .map(|(k, _)| k.clone());
            if let Some(oldest) = oldest {
                self.entries.remove(&oldest);
            }
        }
        let entry = self.entries.entry(key).or_insert((0, V::default()));
        entry.0 = self.tick;
        (&mut entry.1, fresh)
    }

    fn remove(&mut self, key: &BookKey) {
        self.entries.remove(key);
    }
}

/// What a worker remembers between drains, in memory only (a restart starts it afresh, which
/// delays nothing that matters).
#[derive(Debug)]
struct Books {
    /// Consecutive failures per `(consumer, stream)`, handler errors, cuts and short reads
    /// alike: the backoff's exponent.
    failures: Recent<u32>,
    /// The streams whose short read (`delivery_mark_beyond_journal`) is already reported, so
    /// the ERROR comes once per run of short reads, whatever failed before it and whether or
    /// not the failure count was evicted meanwhile (ES-05F keyed it on the count being 1).
    reported: Recent<()>,
}

impl Books {
    const fn new(cap: usize) -> Self {
        Self {
            failures: Recent::new(cap),
            reported: Recent::new(cap),
        }
    }

    /// Counts one more consecutive failure of `key` and returns the count.
    fn fail(&mut self, key: BookKey) -> u32 {
        let (n, _) = self.failures.touch(key);
        *n = n.saturating_add(1);
        *n
    }

    /// Whether a short read of `key` is the first of its run (and so is to be reported); it is
    /// remembered as reported until the stream advances or is drained to its mark.
    fn first_inconsistency(&mut self, key: BookKey) -> bool {
        self.reported.touch(key).1
    }

    /// The stream advanced or was drained to its mark: its failure run and its report end.
    fn delivered(&mut self, key: &BookKey) {
        self.failures.remove(key);
        self.reported.remove(key);
    }

    /// The stream was lost to another worker: its failure count goes (the backoff is the
    /// holder's now), its report stays (a short read is the store's, not the claim's).
    fn lost(&mut self, key: &BookKey) {
        self.failures.remove(key);
    }
}

/// Drains consumers over one journal and its ledger, with backoff jitter from `rng` (ver §3.3)
/// and each `handle` bounded by `timer` ([`DeliveryConfig::handle_budget`]).
pub struct DeliveryWorker<J, L, R, T> {
    journal: J,
    ledger: L,
    rng: R,
    timer: T,
    config: DeliveryConfig,
    /// Failure counts and reported inconsistencies per `(consumer, stream)`: a count goes when
    /// its stream advances, is drained to its mark or is lost to another worker, and each book
    /// holds at most [`MAX_BOOK_ENTRIES`].
    books: Mutex<Books>,
}

impl<J, L, R, T> fmt::Debug for DeliveryWorker<J, L, R, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeliveryWorker")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

/// How one claim ended.
enum Ended {
    Done,
    Failed(Millis),
    Lost,
    Inconsistent(Millis),
}

/// `work`, unless `deadline` comes first (`None`). `work` is polled first, so one that is ready
/// is never cut, and a cut drops it: a consumer's effect may stop half way, which its
/// idempotency by `Delivery::key` already covers (DC-15).
async fn within<W: Future>(deadline: impl Future<Output = ()>, work: W) -> Option<W::Output> {
    let mut work = pin!(work);
    let mut deadline = pin!(deadline);
    poll_fn(|cx| {
        if let Poll::Ready(out) = work.as_mut().poll(cx) {
            return Poll::Ready(Some(out));
        }
        if deadline.as_mut().poll(cx).is_ready() {
            return Poll::Ready(None);
        }
        Poll::Pending
    })
    .await
}

fn name_of<C: Consumer>() -> Result<ConsumerName, DeliveryError> {
    ConsumerName::new(C::NAME).map_err(|_| DeliveryError::InvalidConsumerName)
}

impl<J: Journal, L: DeliveryLedger, R: Rng, T: DeliveryTimer> DeliveryWorker<J, L, R, T> {
    /// A worker over `journal` and `ledger`; `timer` paces the deadline of each `handle`.
    pub fn new(journal: J, ledger: L, rng: R, timer: T, config: DeliveryConfig) -> Self {
        Self {
            journal,
            ledger,
            rng,
            timer,
            config,
            books: Mutex::new(Books::new(MAX_BOOK_ENTRIES)),
        }
    }

    /// The ledger.
    pub const fn ledger(&self) -> &L {
        &self.ledger
    }

    /// Registers `C` and finishes its bootstrap if it has not finished: every stream that
    /// exists is marked at its head unless already delivered through it (DC-04, DC-05).
    ///
    /// # Errors
    /// [`DeliveryError`].
    pub async fn register<C: Consumer>(&self) -> Result<ConsumerName, DeliveryError> {
        let name = name_of::<C>()?;
        self.ledger.register(&name).await?;
        let done = self
            .ledger
            .registered()
            .await?
            .into_iter()
            .any(|c| c.name == name && c.bootstrapped);
        if !done {
            let mut after: Option<StreamId> = None;
            while let Some(last) = self
                .ledger
                .bootstrap(&name, after.as_ref(), self.config.bootstrap_page)
                .await?
            {
                after = Some(last);
            }
        }
        Ok(name)
    }

    /// Registers `consumer`, finishes its bootstrap, and drains every marked stream: what the
    /// daemon runs at start-up (after the safe-mode check) and on the supervision tree's coarse
    /// timer (`doc/event-store.md` §5.1). A long-down consumer catches up every stream in order
    /// (DC-19); a consumer registered by another build is marked by this one's units already
    /// (DC-24), since marks follow the ledger's registrations, not this build's code.
    ///
    /// # Errors
    /// [`DeliveryError`].
    pub async fn catch_up<C: Consumer>(&self, consumer: &C) -> Result<DrainReport, DeliveryError> {
        self.register::<C>().await?;
        self.drain(consumer, None).await
    }

    /// Drains the streams kicked since the last call, each claimed alone (DC-10). A kick that
    /// was lost (a full queue, a crash before the notification) costs only latency: its
    /// stream stays marked for the next `catch_up`.
    ///
    /// # Errors
    /// [`DeliveryError`].
    pub async fn drain_kicked<C: Consumer>(
        &self,
        consumer: &C,
        kicks: &Kicks,
    ) -> Result<DrainReport, DeliveryError> {
        let mut report = DrainReport::default();
        for stream in kicks.take() {
            report.absorb(self.drain(consumer, Some(&stream)).await?);
        }
        Ok(report)
    }

    /// Claims and delivers marked streams of `consumer` (only `only`, when given) until none is
    /// claimable or `max_rounds` rounds of up to `claim_limit` claims ran. A stream is claimed
    /// alone, when the drain reaches it, so a slow stream never lets the leases of the rest of
    /// its round run out unused (DC-27). A stream the drain gave up on (a failure, a cut, a
    /// short read, a lost lease) is not handled again in the same drain: when the ledger's
    /// round-robin discovery comes back round to it, the walk of the stream space is complete,
    /// so the drain releases that claim untouched and returns, and a failing stream whose
    /// backoff passes while others are handled costs each drain one claim, never most of its
    /// `claim_limit * max_rounds` (DC-28). Each claim reads the stream after its checkpoint
    /// through its mark and delivers event by event: renew, handle (cut at
    /// [`DeliveryConfig::handle_budget`], DC-26), advance to that revision. A consumer error or
    /// a cut releases the claim with a backoff and leaves the checkpoint (DC-18); a lost lease
    /// ends that claim only (DC-23); a read that ends short of the mark is a storage
    /// inconsistency, released with a backoff ([`DrainReport::inconsistent`]).
    ///
    /// # Errors
    /// [`DeliveryError`]: the store failed. What was advanced stays advanced.
    pub async fn drain<C: Consumer>(
        &self,
        consumer: &C,
        only: Option<&StreamId>,
    ) -> Result<DrainReport, DeliveryError> {
        let name = name_of::<C>()?;
        let mut report = DrainReport::default();
        let mut gave_up: BTreeSet<StreamId> = BTreeSet::new();
        let mut walked = false;
        for _ in 0..self.config.max_rounds {
            let mut claimed = false;
            for _ in 0..self.config.claim_limit.max(1) {
                let claims = self
                    .ledger
                    .claim(&name, self.config.lease_ttl, 1, only)
                    .await?;
                if claims.is_empty() {
                    break;
                }
                claimed = true;
                for claim in claims {
                    let stream = claim.stream.clone();
                    if gave_up.contains(&stream) {
                        walked = true;
                        self.hand_back(&claim).await?;
                        continue;
                    }
                    match self.deliver(consumer, &claim, &mut report).await? {
                        Ended::Done => self.books().delivered(&(name.clone(), stream)),
                        Ended::Failed(after) => {
                            gave_up.insert(stream.clone());
                            report.failed.push((stream, after));
                        }
                        Ended::Lost => {
                            self.books().lost(&(name.clone(), stream.clone()));
                            gave_up.insert(stream.clone());
                            report.lost.push(stream);
                        }
                        Ended::Inconsistent(after) => {
                            gave_up.insert(stream.clone());
                            report.inconsistent.push((stream, after));
                        }
                    }
                }
                if walked {
                    return Ok(report);
                }
            }
            if !claimed {
                break;
            }
        }
        Ok(report)
    }

    /// Ends a claim of a stream this drain gave up on without reading it: the lease ends now and
    /// nothing else moves (its `retry_at` had passed, or the claim would not have been taken).
    async fn hand_back(&self, claim: &Claim) -> Result<(), DeliveryError> {
        match self.ledger.release(claim, None).await {
            Ok(()) | Err(LeaseError::Lost) => Ok(()),
            Err(LeaseError::Storage(e)) => Err(e.into()),
        }
    }

    fn books(&self) -> std::sync::MutexGuard<'_, Books> {
        self.books.lock().unwrap_or_else(PoisonError::into_inner)
    }

    async fn deliver<C: Consumer>(
        &self,
        consumer: &C,
        claim: &Claim,
        report: &mut DrainReport,
    ) -> Result<Ended, DeliveryError> {
        let ttl = self.config.lease_ttl;
        let events = match self
            .journal
            .read_stream(&claim.stream, claim.checkpoint, Some(claim.marked))
            .await
        {
            Ok(events) => events,
            Err(e) => {
                // The lease ends now rather than at its expiry; the mark is still there.
                let _ = self.ledger.release(claim, None).await;
                return Err(e.into());
            }
        };
        let budget = self.config.handle_budget();
        let mut through = claim.checkpoint;
        for event in &events {
            match self.ledger.renew(claim, ttl).await {
                Ok(()) => {}
                Err(LeaseError::Lost) => return Ok(Ended::Lost),
                Err(LeaseError::Storage(e)) => return Err(e.into()),
            }
            let key = DeliveryKey {
                consumer: claim.consumer.clone(),
                event_id: event.event_id,
            };
            let handled = within(
                self.timer.sleep(budget),
                consumer.handle(Delivery {
                    event,
                    fence: claim.fence,
                    key: &key,
                }),
            )
            .await;
            if !matches!(handled, Some(Ok(()))) {
                let after = self.retry_after(&claim.consumer, &claim.stream);
                return match self.ledger.release(claim, Some(after)).await {
                    Ok(()) => Ok(Ended::Failed(after)),
                    Err(LeaseError::Lost) => Ok(Ended::Lost),
                    Err(LeaseError::Storage(e)) => Err(e.into()),
                };
            }
            match self.ledger.advance(claim, event.revision, ttl).await {
                Ok(()) => {
                    report.delivered += 1;
                    through = Some(event.revision);
                    self.books()
                        .delivered(&(claim.consumer.clone(), claim.stream.clone()));
                }
                Err(LeaseError::Lost) => return Ok(Ended::Lost),
                Err(LeaseError::Storage(e)) => return Err(e.into()),
            }
        }
        if through < Some(claim.marked) {
            return self.inconsistent(claim, through).await;
        }
        match self.ledger.release(claim, None).await {
            Ok(()) => Ok(Ended::Done),
            Err(LeaseError::Lost) => Ok(Ended::Lost),
            Err(LeaseError::Storage(e)) => Err(e.into()),
        }
    }

    /// The journal returned the stream only through `through`, short of the claim's mark: the
    /// mark names an event the journal does not have (a damaged or wrongly restored store).
    /// Releasing without a backoff would leave the mark claimable at once and the drain would
    /// claim the stream again every round; so the claim is released with the failure backoff,
    /// and the first short read of a run is logged at ERROR (nothing here can repair it), also
    /// when a handler failure came before it ([`Books::first_inconsistency`]). Pinned by
    /// `tests/store/delivery.rs::should_back_off_and_report_when_the_journal_ends_short_of_the_mark`
    /// and `tests::should_report_the_first_short_read_when_a_handler_failure_came_before_it`.
    async fn inconsistent(
        &self,
        claim: &Claim,
        through: Option<StreamRevision>,
    ) -> Result<Ended, DeliveryError> {
        let after = self.retry_after(&claim.consumer, &claim.stream);
        let first = self
            .books()
            .first_inconsistency((claim.consumer.clone(), claim.stream.clone()));
        if first {
            tracing::error!(
                consumer = claim.consumer.as_str(),
                stream = claim.stream.as_str(),
                marked = claim.marked.get(),
                read_through = through.map(StreamRevision::get),
                "delivery_mark_beyond_journal"
            );
        }
        match self.ledger.release(claim, Some(after)).await {
            Ok(()) => Ok(Ended::Inconsistent(after)),
            Err(LeaseError::Lost) => Ok(Ended::Lost),
            Err(LeaseError::Storage(e)) => Err(e.into()),
        }
    }

    /// The backoff after the stream's next consecutive failure: full jitter in `1..=d`, where
    /// `d` doubles from `backoff_base` per failure up to `backoff_cap`, drawn from the `Rng`
    /// port. At least 1 ms, so the store clock must pass it before the stream is claimable
    /// again and a drain never retries a failing event in a tight loop (pinned by
    /// `tests/store/delivery.rs::should_back_off_with_jitter_from_the_rng_when_a_consumer_keeps_failing`).
    fn retry_after(&self, consumer: &ConsumerName, stream: &StreamId) -> Millis {
        let attempt = self.books().fail((consumer.clone(), stream.clone()));
        let base = self.config.backoff_base.get().max(1);
        let cap = self.config.backoff_cap.get().max(base);
        let mut ceiling = base;
        for _ in 1..attempt {
            if ceiling >= cap {
                break;
            }
            ceiling = ceiling.saturating_mul(2);
        }
        Millis::new(1 + self.rng.below(ceiling.min(cap)))
    }
}

#[cfg(test)]
mod tests {
    use super::{BookKey, Books, ConsumerName, Recent, StreamId};

    fn key(stream: &str) -> BookKey {
        (
            ConsumerName::new("relay").unwrap_or_else(|e| panic!("{e}")),
            StreamId::new(stream).unwrap_or_else(|e| panic!("{e}")),
        )
    }

    #[test]
    fn should_evict_the_least_recently_touched_key_when_the_book_is_full() {
        // given: a full book whose lowest key was touched last
        let mut book: Recent<u32> = Recent::new(2);
        *book.touch(key("attempt-a")).0 = 1;
        *book.touch(key("attempt-b")).0 = 1;
        *book.touch(key("attempt-a")).0 += 1;

        // when
        let (_, fresh) = book.touch(key("attempt-c"));

        // then: `b` went, `a` kept its count
        assert!(fresh);
        let (count, created) = book.touch(key("attempt-a"));
        assert_eq!((*count, created), (2, false));
        assert!(book.touch(key("attempt-b")).1);
    }

    #[test]
    fn should_report_the_first_short_read_when_a_handler_failure_came_before_it() {
        // given: the stream's handler failed once
        let mut books = Books::new(8);
        books.fail(key("attempt-a"));

        // when: its next claim reads short, twice
        books.fail(key("attempt-a"));
        let first = books.first_inconsistency(key("attempt-a"));
        let second = books.first_inconsistency(key("attempt-a"));

        // then
        assert_eq!((first, second), (true, false));
    }

    #[test]
    fn should_report_a_short_read_again_when_the_stream_was_delivered_in_between() {
        // given
        let mut books = Books::new(8);
        books.first_inconsistency(key("attempt-a"));

        // when
        books.delivered(&key("attempt-a"));

        // then
        assert!(books.first_inconsistency(key("attempt-a")));
    }

    #[test]
    fn should_report_a_short_read_once_when_its_failure_count_was_evicted() {
        // given: a short read reported, then its failure count evicted by other failures
        let mut books = Books::new(1);
        books.fail(key("attempt-a"));
        books.first_inconsistency(key("attempt-a"));
        books.fail(key("attempt-b"));

        // when: the stream reads short again (its count starts afresh at 1)
        let attempt = books.fail(key("attempt-a"));
        let reported = books.first_inconsistency(key("attempt-a"));

        // then
        assert_eq!((attempt, reported), (1, false));
    }
}
