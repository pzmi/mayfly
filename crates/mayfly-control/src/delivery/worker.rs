//! `DeliveryWorker`: drains a consumer's claimed streams (`doc/event-store.md` §3.4, §5.1).
//!
//! It maps the model's consumer actions one to one (`formal/event_store.qnt`): a `claim` per
//! [`DeliveryLedger::claim`] (the claim fixes the revision it reads through, the model's
//! `through`); per event, `effect` is [`DeliveryLedger::renew`] then [`Consumer::handle`], and
//! `advance` is [`DeliveryLedger::advance`] to that event's revision, never to the head; a
//! consumer error is `failHandle` ([`DeliveryLedger::release`] with a backoff); the end of the
//! read is `release`. The worker starts no task and holds nothing between calls but its failure
//! counts: the supervision tree (RT-02) owns the loop that calls it, so stopping it is dropping
//! the future.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Mutex, PoisonError};

use mayfly_domain::Millis;

use super::kicks::Kicks;
use super::values::{ConsumerName, DeliveryKey};
use super::{Claim, Consumer, Delivery, DeliveryLedger, LeaseError};
use crate::journal::{Journal, StorageError, StreamId};
use crate::store::Rng;

/// How a worker claims and backs off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeliveryConfig {
    /// How long a claim's lease lives without a renewal (store clock).
    pub lease_ttl: Millis,
    /// Streams claimed per round.
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
    /// A 30 s lease, 16 streams a round, pages of 256, a backoff from 100 ms to 60 s, 64 rounds.
    fn default() -> Self {
        Self {
            lease_ttl: Millis::new(30_000),
            claim_limit: 16,
            bootstrap_page: 256,
            backoff_base: Millis::new(100),
            backoff_cap: Millis::new(60_000),
            max_rounds: 64,
        }
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
}

impl DrainReport {
    fn absorb(&mut self, other: Self) {
        self.delivered += other.delivered;
        self.failed.extend(other.failed);
        self.lost.extend(other.lost);
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

/// Drains consumers over one journal and its ledger, with backoff jitter from `rng` (ver §3.3).
pub struct DeliveryWorker<J, L, R> {
    journal: J,
    ledger: L,
    rng: R,
    config: DeliveryConfig,
    /// Consecutive failures per `(consumer, stream)`: the backoff's exponent. In memory only; a
    /// restart starts the backoff again from its base, which delays nothing that matters.
    failures: Mutex<BTreeMap<(ConsumerName, StreamId), u32>>,
}

impl<J, L, R> fmt::Debug for DeliveryWorker<J, L, R> {
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
}

fn name_of<C: Consumer>() -> Result<ConsumerName, DeliveryError> {
    ConsumerName::new(C::NAME).map_err(|_| DeliveryError::InvalidConsumerName)
}

impl<J: Journal, L: DeliveryLedger, R: Rng> DeliveryWorker<J, L, R> {
    /// A worker over `journal` and `ledger`.
    pub fn new(journal: J, ledger: L, rng: R, config: DeliveryConfig) -> Self {
        Self {
            journal,
            ledger,
            rng,
            config,
            failures: Mutex::new(BTreeMap::new()),
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
    /// claimable or `max_rounds` rounds ran. Each claim reads the stream after its checkpoint
    /// through its mark and delivers event by event: renew, handle, advance to that revision. A
    /// consumer error releases the claim with a backoff and leaves the checkpoint (DC-18); a
    /// lost lease ends that claim only (DC-23).
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
        for _ in 0..self.config.max_rounds {
            let claims = self
                .ledger
                .claim(&name, self.config.lease_ttl, self.config.claim_limit, only)
                .await?;
            if claims.is_empty() {
                break;
            }
            for claim in claims {
                let stream = claim.stream.clone();
                match self.deliver(consumer, &claim, &mut report).await? {
                    Ended::Done => self.forget(&name, &stream),
                    Ended::Failed(after) => report.failed.push((stream, after)),
                    Ended::Lost => report.lost.push(stream),
                }
            }
        }
        Ok(report)
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
            let handled = consumer
                .handle(Delivery {
                    event,
                    fence: claim.fence,
                    key: &key,
                })
                .await;
            if handled.is_err() {
                let after = self.retry_after(&claim.consumer, &claim.stream);
                return match self.ledger.release(claim, Some(after)).await {
                    Ok(()) => Ok(Ended::Failed(after)),
                    Err(LeaseError::Lost) => Ok(Ended::Lost),
                    Err(LeaseError::Storage(e)) => Err(e.into()),
                };
            }
            match self.ledger.advance(claim, event.revision, ttl).await {
                Ok(()) => report.delivered += 1,
                Err(LeaseError::Lost) => return Ok(Ended::Lost),
                Err(LeaseError::Storage(e)) => return Err(e.into()),
            }
        }
        match self.ledger.release(claim, None).await {
            Ok(()) => Ok(Ended::Done),
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
        let attempt = {
            let mut failures = self.failures.lock().unwrap_or_else(PoisonError::into_inner);
            let n = failures
                .entry((consumer.clone(), stream.clone()))
                .or_insert(0);
            *n = n.saturating_add(1);
            *n
        };
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

    fn forget(&self, consumer: &ConsumerName, stream: &StreamId) {
        self.failures
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&(consumer.clone(), stream.clone()));
    }
}
