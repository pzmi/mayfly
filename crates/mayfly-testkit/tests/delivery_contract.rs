//! The delivery contract's own tests (ver §5): `MemoryLedger` passes it, and each deliberately
//! broken ledger of `doc/event-store.md` §7.6 fails the cases named for its fault. Each test
//! pins the exact set of failed cases, which must include the ones §7.6 names: a change that
//! lets a broken ledger through, or makes it fail for an unrelated reason, shows here.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};

use mayfly_control::delivery::{Claim, ConsumerName, ConsumerState, DeliveryLedger, LeaseError};
use mayfly_control::journal::{Journal, StorageError, StreamId, StreamRevision};
use mayfly_domain::Millis;
use mayfly_testkit::block_on;
use mayfly_testkit::clock::FakeClock;
use mayfly_testkit::contracts::delivery::{
    ClockHand, DeliveryFactory, DeliveryStore, check_delivery_contract,
};
use mayfly_testkit::contracts::journal::InjectedFault;
use mayfly_testkit::delivery::{MemoryDeliveryFactory, MemoryLedger};
use mayfly_testkit::journal::MemoryJournal;

/// The fault a broken ledger (or worker step) carries, named as in `doc/event-store.md` §7.6.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flaw {
    /// `UnfencedLedger`: an advance ignores the claim's fence and moves the checkpoint under
    /// whichever lease is live.
    Unfenced,
    /// `ClearAllMarksLedger`: an advance deletes the mark whatever its revision.
    ClearAllMarks,
    /// `HeadTrustingWorker`: advances to the stream's head, not to the revision it delivered
    /// (the model's negative world `headTrustingReader`).
    HeadTrusting,
    /// `SkippingLedger`: after a second failure at one checkpoint, a release moves past the
    /// failing event.
    Skipping,
    /// `GreedyClaimLedger`: a claim takes every claimable stream whatever its limit, so the
    /// worker holds a whole round's leases before it reaches them (the batch claim ES-05F
    /// removed from the worker).
    GreedyClaim,
}

/// A memory ledger with `flaw`. Clones share the store and the skip counts.
#[derive(Clone)]
struct Flawed {
    inner: MemoryLedger<FakeClock>,
    journal: MemoryJournal,
    flaw: Flaw,
    failures: Arc<Mutex<BTreeMap<FailureKey, u32>>>,
}

/// A stream of a consumer at a checkpoint: what the `SkippingLedger` counts failures by.
type FailureKey = (ConsumerName, StreamId, Option<StreamRevision>);

impl DeliveryLedger for Flawed {
    async fn registered(&self) -> Result<Vec<ConsumerState>, StorageError> {
        self.inner.registered().await
    }

    async fn register(&self, consumer: &ConsumerName) -> Result<(), StorageError> {
        self.inner.register(consumer).await
    }

    async fn bootstrap(
        &self,
        consumer: &ConsumerName,
        after: Option<&StreamId>,
        limit: u32,
    ) -> Result<Option<StreamId>, StorageError> {
        self.inner.bootstrap(consumer, after, limit).await
    }

    async fn claim(
        &self,
        consumer: &ConsumerName,
        ttl: Millis,
        limit: u32,
        only: Option<&StreamId>,
    ) -> Result<Vec<Claim>, StorageError> {
        let limit = if self.flaw == Flaw::GreedyClaim {
            limit.max(16)
        } else {
            limit
        };
        self.inner.claim(consumer, ttl, limit, only).await
    }

    async fn renew(&self, claim: &Claim, ttl: Millis) -> Result<(), LeaseError> {
        self.inner.renew(claim, ttl).await
    }

    async fn advance(
        &self,
        claim: &Claim,
        to: StreamRevision,
        ttl: Millis,
    ) -> Result<(), LeaseError> {
        match self.flaw {
            Flaw::Unfenced => {
                let current = self.inner.lease(&claim.consumer, &claim.stream);
                let unfenced = Claim {
                    fence: current.map_or(claim.fence, |(fence, _)| fence),
                    ..claim.clone()
                };
                self.inner.advance(&unfenced, to, ttl).await
            }
            Flaw::ClearAllMarks => {
                self.inner.advance(claim, to, ttl).await?;
                self.inner.clear_mark(&claim.consumer, &claim.stream);
                Ok(())
            }
            Flaw::HeadTrusting => {
                let head = self
                    .journal
                    .head(&claim.stream)
                    .await
                    .map_err(LeaseError::Storage)?
                    .map_or(to, |h| h.revision);
                self.inner.advance(claim, head, ttl).await
            }
            Flaw::Skipping | Flaw::GreedyClaim => self.inner.advance(claim, to, ttl).await,
        }
    }

    async fn release(&self, claim: &Claim, retry_after: Option<Millis>) -> Result<(), LeaseError> {
        if self.flaw == Flaw::Skipping && retry_after.is_some() {
            let tries = {
                let mut failures = self.failures.lock().unwrap_or_else(PoisonError::into_inner);
                let n = failures
                    .entry((
                        claim.consumer.clone(),
                        claim.stream.clone(),
                        claim.checkpoint,
                    ))
                    .or_insert(0);
                *n += 1;
                *n
            };
            if tries >= 2 {
                let past = claim
                    .checkpoint
                    .map_or(StreamRevision::new(0), |c| StreamRevision::new(c.get() + 1));
                self.inner.advance(claim, past, Millis::new(1)).await?;
                return self.inner.release(claim, None).await;
            }
        }
        self.inner.release(claim, retry_after).await
    }

    async fn retire(&self, consumer: &ConsumerName) -> Result<(), StorageError> {
        self.inner.retire(consumer).await
    }
}

/// The memory factory, its ledgers carrying `flaw`.
struct FlawedFactory {
    inner: MemoryDeliveryFactory,
    flaw: Flaw,
}

impl FlawedFactory {
    fn wrap(
        &self,
        store: DeliveryStore<MemoryJournal, MemoryLedger<FakeClock>>,
    ) -> DeliveryStore<MemoryJournal, Flawed> {
        DeliveryStore {
            ledger: Flawed {
                inner: store.ledger,
                journal: store.journal.clone(),
                flaw: self.flaw,
                failures: Arc::default(),
            },
            journal: store.journal,
        }
    }

    fn unwrap(
        store: DeliveryStore<MemoryJournal, Flawed>,
    ) -> DeliveryStore<MemoryJournal, MemoryLedger<FakeClock>> {
        DeliveryStore {
            journal: store.journal,
            ledger: store.ledger.inner,
        }
    }
}

impl DeliveryFactory for FlawedFactory {
    type Journal = MemoryJournal;
    type Ledger = Flawed;

    async fn fresh(&self) -> DeliveryStore<MemoryJournal, Flawed> {
        self.wrap(self.inner.fresh().await)
    }

    async fn reopen(
        &self,
        store: DeliveryStore<MemoryJournal, Flawed>,
    ) -> DeliveryStore<MemoryJournal, Flawed> {
        store
    }

    async fn second_handle(
        &self,
        store: &DeliveryStore<MemoryJournal, Flawed>,
    ) -> DeliveryStore<MemoryJournal, Flawed> {
        let inner = DeliveryStore {
            journal: store.journal.clone(),
            ledger: store.ledger.inner.clone(),
        };
        let mut other = self.wrap(self.inner.second_handle(&inner).await);
        other.ledger.failures = Arc::clone(&store.ledger.failures);
        other
    }

    async fn inject(
        &self,
        store: &DeliveryStore<MemoryJournal, Flawed>,
        fault: InjectedFault,
    ) -> bool {
        store.journal.inject(fault)
    }

    async fn crash(
        &self,
        store: DeliveryStore<MemoryJournal, Flawed>,
    ) -> DeliveryStore<MemoryJournal, Flawed> {
        let failures = Arc::clone(&store.ledger.failures);
        let mut back = self.wrap(self.inner.crash(Self::unwrap(store)).await);
        back.ledger.failures = failures;
        back
    }

    fn clock(&self) -> ClockHand {
        self.inner.clock()
    }
}

fn failed_with(flaw: Flaw) -> Vec<&'static str> {
    let factory = FlawedFactory {
        inner: MemoryDeliveryFactory::new(),
        flaw,
    };
    block_on(check_delivery_contract(&factory)).failed_ids()
}

#[test]
fn should_pass_delivery_contract_when_the_ledger_is_in_memory() {
    // given
    let factory = MemoryDeliveryFactory::new();

    // when
    let report = block_on(check_delivery_contract(&factory));

    // then
    assert!(report.is_green(), "{report}");
}

#[test]
fn should_fail_the_takeover_cases_when_the_ledger_ignores_the_fence() {
    // given
    let flaw = Flaw::Unfenced;

    // when
    let failed = failed_with(flaw);

    // then
    assert_eq!(failed, ["DC-11", "DC-23"]);
}

#[test]
fn should_fail_the_mark_cases_when_the_ledger_clears_every_mark() {
    // given
    let flaw = Flaw::ClearAllMarks;

    // when
    let failed = failed_with(flaw);

    // then
    assert_eq!(failed, ["DC-08", "DC-13", "DC-18"]);
}

#[test]
fn should_fail_the_no_skip_cases_when_the_worker_advances_to_the_head() {
    // given
    let flaw = Flaw::HeadTrusting;

    // when
    let failed = failed_with(flaw);

    // then
    assert_eq!(failed, ["DC-08", "DC-13", "DC-18"]);
}

#[test]
fn should_fail_the_failing_consumer_case_when_the_ledger_skips_after_retries() {
    // given
    let flaw = Flaw::Skipping;

    // when
    let failed = failed_with(flaw);

    // then
    assert_eq!(failed, ["DC-18"]);
}

#[test]
fn should_fail_the_round_cases_when_a_claim_takes_more_than_its_limit() {
    // given
    let flaw = Flaw::GreedyClaim;

    // when
    let failed = failed_with(flaw);

    // then
    assert_eq!(failed, ["DC-23", "DC-27"]);
}
