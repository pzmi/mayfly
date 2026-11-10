//! `MemoryLedger`: the `DeliveryLedger` port in memory, over the same store as a
//! `MemoryJournal`, so the dispatch marks a unit writes commit with it (`doc/event-store.md`
//! §3.4, §5.1). It passes the delivery contract (`tests/delivery_contract.rs`:
//! `should_pass_delivery_contract_when_the_ledger_is_in_memory`).
//!
//! Every operation runs under the journal's one lock in one poll, so it is one transaction; the
//! store clock is the injected [`StoreClock`] (a `FakeClock` in tests), read inside it.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};

use mayfly_control::delivery::{
    Claim, ConsumerName, ConsumerState, DeliveryLedger, LeaseError, LeaseFence, StoreClock,
};
use mayfly_control::journal::{MAX_PAGE, StorageError, StreamId, StreamRevision};
use mayfly_domain::Millis;

use crate::clock::FakeClock;
use crate::contracts::delivery::{ClockHand, DeliveryFactory, DeliveryStore};
use crate::contracts::journal::InjectedFault;
use crate::journal::{MemoryJournal, State};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Mark {
    marked: StreamRevision,
    retry_at: Option<Millis>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Lease {
    until: Millis,
    fence: u64,
}

type Key = (ConsumerName, StreamId);

/// The ledger's rows inside the memory store.
#[derive(Debug, Clone, Default)]
pub(crate) struct DeliveryRows {
    /// Registered consumers and whether their bootstrap finished.
    consumers: BTreeMap<ConsumerName, bool>,
    marks: BTreeMap<Key, Mark>,
    checkpoints: BTreeMap<Key, StreamRevision>,
    leases: BTreeMap<Key, Lease>,
}

impl DeliveryRows {
    /// Marks every registered consumer for `stream` at `head` (a mark only rises): what a
    /// committed unit does for each stream it appended.
    pub(crate) fn mark_all(&mut self, stream: &StreamId, head: StreamRevision) {
        let names: Vec<ConsumerName> = self.consumers.keys().cloned().collect();
        for name in names {
            self.mark(name, stream.clone(), head);
        }
    }

    fn mark(&mut self, consumer: ConsumerName, stream: StreamId, head: StreamRevision) {
        self.marks
            .entry((consumer, stream))
            .and_modify(|m| m.marked = m.marked.max(head))
            .or_insert(Mark {
                marked: head,
                retry_at: None,
            });
    }

    /// The claim's lease, if it is still the claim's and alive at `now`.
    fn held(&self, claim: &Claim, now: Millis) -> Result<Key, LeaseError> {
        let key = (claim.consumer.clone(), claim.stream.clone());
        match self.leases.get(&key) {
            Some(lease) if lease.fence == claim.fence.get() && lease.until > now => Ok(key),
            _ => Err(LeaseError::Lost),
        }
    }
}

/// The in-memory ledger: a handle on a [`MemoryJournal`]'s store and a store clock. Clones share
/// the store, the clock and the round-robin position.
#[derive(Clone)]
pub struct MemoryLedger<C> {
    journal: MemoryJournal,
    clock: C,
    /// The last stream claimed per consumer, where the next claim's discovery starts: fairness
    /// only, never a decision (the marks decide).
    cursor: Arc<Mutex<BTreeMap<ConsumerName, StreamId>>>,
}

impl<C> fmt::Debug for MemoryLedger<C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MemoryLedger").finish_non_exhaustive()
    }
}

fn plus(now: Millis, by: Millis) -> Millis {
    Millis::new(now.get().saturating_add(by.get()))
}

impl<C: StoreClock> MemoryLedger<C> {
    /// The ledger of `journal`'s store, judging leases by `clock`.
    #[must_use]
    pub fn new(journal: &MemoryJournal, clock: C) -> Self {
        Self {
            journal: journal.clone(),
            clock,
            cursor: Arc::default(),
        }
    }

    fn with<T>(&self, f: impl FnOnce(&mut State, Millis) -> T) -> T {
        let mut shared = self.journal.lock();
        let now = self.clock.now();
        f(&mut shared.state, now)
    }

    /// The lease of `(consumer, stream)`: its fence and until when it lives. For broken ledgers
    /// built over this one (the `UnfencedLedger` the contract must catch).
    #[must_use]
    pub fn lease(
        &self,
        consumer: &ConsumerName,
        stream: &StreamId,
    ) -> Option<(LeaseFence, Millis)> {
        self.with(|s, _| {
            s.delivery
                .leases
                .get(&(consumer.clone(), stream.clone()))
                .map(|l| (LeaseFence::new(l.fence), l.until))
        })
    }

    /// Forgets where `consumer`'s next discovery starts, so it starts again from the lowest
    /// key: for broken ledgers built over this one (the `CursorlessLedger` DC-28 must catch).
    pub fn forget_cursor(&self, consumer: &ConsumerName) {
        self.cursor
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(consumer);
    }

    /// Removes the mark of `(consumer, stream)` whatever its revision, judging nothing: for
    /// broken ledgers built over this one (the `ClearAllMarksLedger` DC-08 must catch).
    pub fn clear_mark(&self, consumer: &ConsumerName, stream: &StreamId) {
        self.with(|s, _| {
            s.delivery.marks.remove(&(consumer.clone(), stream.clone()));
        });
    }
}

impl<C: StoreClock> DeliveryLedger for MemoryLedger<C> {
    async fn registered(&self) -> Result<Vec<ConsumerState>, StorageError> {
        Ok(self.with(|s, _| {
            s.delivery
                .consumers
                .iter()
                .map(|(name, bootstrapped)| ConsumerState {
                    name: name.clone(),
                    bootstrapped: *bootstrapped,
                })
                .collect()
        }))
    }

    async fn register(&self, consumer: &ConsumerName) -> Result<(), StorageError> {
        self.with(|s, _| {
            s.delivery
                .consumers
                .entry(consumer.clone())
                .or_insert(false);
        });
        Ok(())
    }

    async fn bootstrap(
        &self,
        consumer: &ConsumerName,
        after: Option<&StreamId>,
        limit: u32,
    ) -> Result<Option<StreamId>, StorageError> {
        let limit = usize::try_from(limit.clamp(1, MAX_PAGE)).unwrap_or(1);
        self.with(|s, _| {
            if !s.delivery.consumers.contains_key(consumer) {
                return Err(StorageError::new("unknown_consumer"));
            }
            let page: Vec<(StreamId, StreamRevision)> = s
                .heads
                .values()
                .filter(|h| after.is_none_or(|a| h.stream > *a))
                .take(limit)
                .map(|h| (h.stream.clone(), h.revision))
                .collect();
            for (stream, head) in &page {
                let key = (consumer.clone(), stream.clone());
                if s.delivery.checkpoints.get(&key).is_none_or(|c| c < head) {
                    s.delivery.mark(consumer.clone(), stream.clone(), *head);
                }
            }
            if page.len() < limit {
                s.delivery.consumers.insert(consumer.clone(), true);
                Ok(None)
            } else {
                Ok(page.last().map(|(stream, _)| stream.clone()))
            }
        })
    }

    async fn claim(
        &self,
        consumer: &ConsumerName,
        ttl: Millis,
        limit: u32,
        only: Option<&StreamId>,
    ) -> Result<Vec<Claim>, StorageError> {
        let limit = usize::try_from(limit.min(MAX_PAGE)).unwrap_or(0);
        let cursor = self
            .cursor
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(consumer)
            .cloned();
        let claims = self.with(|s, now| {
            let rows = &mut s.delivery;
            let claimable = |key: &Key, mark: &Mark| {
                key.0 == *consumer
                    && only.is_none_or(|o| key.1 == *o)
                    && mark.retry_at.is_none_or(|at| at <= now)
                    && rows.leases.get(key).is_none_or(|l| l.until <= now)
                    && rows.checkpoints.get(key).is_none_or(|c| *c < mark.marked)
            };
            let after_cursor = |key: &Key| cursor.as_ref().is_none_or(|c| key.1 > *c);
            let mut picked: Vec<(Key, StreamRevision)> = rows
                .marks
                .iter()
                .filter(|(k, m)| after_cursor(k) && claimable(k, m))
                .chain(
                    rows.marks
                        .iter()
                        .filter(|(k, m)| !after_cursor(k) && claimable(k, m)),
                )
                .take(limit)
                .map(|(k, m)| (k.clone(), m.marked))
                .collect();
            picked.sort_by(|a, b| a.0.cmp(&b.0));
            let mut claims = Vec::with_capacity(picked.len());
            for (key, marked) in picked {
                let fence = rows.leases.get(&key).map_or(0, |l| l.fence) + 1;
                rows.leases.insert(
                    key.clone(),
                    Lease {
                        until: plus(now, ttl),
                        fence,
                    },
                );
                claims.push(Claim {
                    checkpoint: rows.checkpoints.get(&key).copied(),
                    consumer: key.0,
                    stream: key.1,
                    fence: LeaseFence::new(fence),
                    marked,
                });
            }
            claims
        });
        if let Some(last) = claims.last() {
            self.cursor
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(consumer.clone(), last.stream.clone());
        }
        Ok(claims)
    }

    async fn renew(&self, claim: &Claim, ttl: Millis) -> Result<(), LeaseError> {
        self.with(|s, now| {
            let key = s.delivery.held(claim, now)?;
            if let Some(lease) = s.delivery.leases.get_mut(&key) {
                lease.until = plus(now, ttl);
            }
            Ok(())
        })
    }

    async fn advance(
        &self,
        claim: &Claim,
        to: StreamRevision,
        ttl: Millis,
    ) -> Result<(), LeaseError> {
        self.with(|s, now| {
            let key = s.delivery.held(claim, now)?;
            let rows = &mut s.delivery;
            rows.checkpoints
                .entry(key.clone())
                .and_modify(|c| *c = (*c).max(to))
                .or_insert(to);
            if rows.marks.get(&key).is_some_and(|m| m.marked <= to) {
                rows.marks.remove(&key);
            }
            if let Some(lease) = rows.leases.get_mut(&key) {
                lease.until = plus(now, ttl);
            }
            Ok(())
        })
    }

    async fn release(&self, claim: &Claim, retry_after: Option<Millis>) -> Result<(), LeaseError> {
        self.with(|s, now| {
            let key = s.delivery.held(claim, now)?;
            let rows = &mut s.delivery;
            if let Some(lease) = rows.leases.get_mut(&key) {
                lease.until = now;
            }
            if let (Some(after), Some(mark)) = (retry_after, rows.marks.get_mut(&key)) {
                mark.retry_at = Some(plus(now, after));
            }
            Ok(())
        })
    }

    async fn retire(&self, consumer: &ConsumerName) -> Result<(), StorageError> {
        self.with(|s, _| {
            let rows = &mut s.delivery;
            rows.consumers.remove(consumer);
            rows.marks.retain(|(c, _), _| c != consumer);
            rows.checkpoints.retain(|(c, _), _| c != consumer);
            for ((c, _), lease) in &mut rows.leases {
                if c == consumer {
                    lease.until = Millis::new(0);
                }
            }
        });
        Ok(())
    }
}

/// Makes [`MemoryLedger`]s over [`MemoryJournal`]s for the delivery contract: reopening, a
/// second handle and a crash all hand back the same store (with a fresh round-robin position),
/// faults are injected with [`MemoryJournal::inject`], and one [`FakeClock`] is every store's
/// clock.
#[derive(Debug, Clone, Default)]
pub struct MemoryDeliveryFactory {
    clock: FakeClock,
}

impl MemoryDeliveryFactory {
    /// A factory whose store clock starts at 1 000 000 ms.
    #[must_use]
    pub fn new() -> Self {
        Self {
            clock: FakeClock::at(Millis::new(1_000_000)),
        }
    }

    /// The store clock.
    #[must_use]
    pub const fn store_clock(&self) -> &FakeClock {
        &self.clock
    }
}

impl DeliveryFactory for MemoryDeliveryFactory {
    type Journal = MemoryJournal;
    type Ledger = MemoryLedger<FakeClock>;

    async fn fresh(&self) -> DeliveryStore<MemoryJournal, MemoryLedger<FakeClock>> {
        let journal = MemoryJournal::new();
        let ledger = MemoryLedger::new(&journal, self.clock.clone());
        DeliveryStore { journal, ledger }
    }

    async fn reopen(
        &self,
        store: DeliveryStore<MemoryJournal, MemoryLedger<FakeClock>>,
    ) -> DeliveryStore<MemoryJournal, MemoryLedger<FakeClock>> {
        store
    }

    async fn second_handle(
        &self,
        store: &DeliveryStore<MemoryJournal, MemoryLedger<FakeClock>>,
    ) -> DeliveryStore<MemoryJournal, MemoryLedger<FakeClock>> {
        let journal = store.journal.clone();
        let ledger = MemoryLedger::new(&journal, self.clock.clone());
        DeliveryStore { journal, ledger }
    }

    async fn inject(
        &self,
        store: &DeliveryStore<MemoryJournal, MemoryLedger<FakeClock>>,
        fault: InjectedFault,
    ) -> bool {
        store.journal.inject(fault)
    }

    async fn crash(
        &self,
        store: DeliveryStore<MemoryJournal, MemoryLedger<FakeClock>>,
    ) -> DeliveryStore<MemoryJournal, MemoryLedger<FakeClock>> {
        store
    }

    fn clock(&self) -> ClockHand {
        let clock = self.clock.clone();
        Arc::new(move |by| clock.advance(by))
    }
}
