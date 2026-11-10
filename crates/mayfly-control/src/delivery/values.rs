//! The values the delivery ledger stores and hands out (`doc/event-store.md` §3.1, §3.4).

use std::fmt;

use mayfly_domain::EventId;

use crate::journal::{JournalValueError, RecordedEvent, StorageError, StreamId, StreamRevision};

/// A durable consumer's name, `[a-z][a-z0-9_]{0,63}`: the key of its registration, marks,
/// checkpoints and leases (`directive_relay`, `attempt_saga`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ConsumerName(String);

impl ConsumerName {
    /// Validates `[a-z][a-z0-9_]{0,63}`.
    ///
    /// # Errors
    /// [`JournalValueError::InvalidConsumerName`] for anything else.
    pub fn new(value: &str) -> Result<Self, JournalValueError> {
        if crate::journal::valid_snake_name(value) {
            Ok(Self(value.to_owned()))
        } else {
            Err(JournalValueError::InvalidConsumerName)
        }
    }

    /// The validated text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ConsumerName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A lease's fence: strictly increasing per `(consumer, stream)` with every claim, never reset,
/// not even by [`DeliveryLedger::retire`](super::DeliveryLedger::retire) (ES-I14). It is also the
/// claim's token: a claim is the lease `(consumer, stream, fence)`, so a claim whose fence is not
/// the lease's newest, or whose lease ran out by the store clock, can neither renew, advance nor
/// release (contract cases DC-11, DC-12, DC-20, DC-22). Kept below `i64::MAX`, as both engines
/// store it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LeaseFence(u64);

impl LeaseFence {
    /// The fence `value`.
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// The number.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for LeaseFence {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A registered consumer as the ledger lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsumerState {
    /// Its name.
    pub name: ConsumerName,
    /// Whether its bootstrap reached the last stream that existed: from then on every stream is
    /// marked by the units that append to it.
    pub bootstrapped: bool,
}

/// A claimed stream: the worker holding it may deliver its events after `checkpoint` through
/// `marked`, while its lease lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claim {
    /// The consumer.
    pub consumer: ConsumerName,
    /// The stream.
    pub stream: StreamId,
    /// The lease's fence at the claim, and the claim's token.
    pub fence: LeaseFence,
    /// The last revision the consumer advanced to; `None` before its first.
    pub checkpoint: Option<StreamRevision>,
    /// The mark at the claim: the head the last unit (or the bootstrap) marked. The claim reads
    /// through it and never past it (`directivesFromJournal`), even when the head moved since:
    /// that newer mark survives the claim's advances (DC-08).
    pub marked: StreamRevision,
}

/// The idempotency key of one delivery: `(consumer, event_id)`. A consumer whose effect cannot
/// be fenced by [`Delivery::fence`] dedupes on it, since a takeover may repeat an event whose
/// effect was already in flight (`doc/event-store.md` §5.1).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DeliveryKey {
    /// The consumer.
    pub consumer: ConsumerName,
    /// The delivered event.
    pub event_id: EventId,
}

/// One event handed to a consumer.
#[derive(Debug, Clone, Copy)]
pub struct Delivery<'a> {
    /// The event.
    pub event: &'a RecordedEvent,
    /// The fence of the claim it is delivered under: a consumer that fences its own resource
    /// keeps the highest fence seen per stream and refuses a lower one (DC-21).
    pub fence: LeaseFence,
    /// The consumer and the event: the effect's idempotency key.
    pub key: &'a DeliveryKey,
}

/// Why a consumer did not handle an event: a fixed `snake_case` code and nothing else, so no
/// payload, secret or upstream text reaches the ledger or a log through it (ES-I13). The event
/// is delivered again after a backoff, and never skipped (ES-I15, DC-18).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConsumerError {
    code: &'static str,
}

impl ConsumerError {
    /// A failure named by `code`.
    #[must_use]
    pub const fn new(code: &'static str) -> Self {
        Self { code }
    }

    /// The code.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        self.code
    }
}

impl fmt::Display for ConsumerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "consumer failed: {}", self.code)
    }
}

impl std::error::Error for ConsumerError {}

/// Why `renew`, `advance` or `release` did nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaseError {
    /// The claim's lease ran out by the store clock or was taken over: no checkpoint moved, no
    /// mark was cleared, and the claim is over (ES-I14). It is not an error of the store.
    Lost,
    /// The store failed; nothing was written.
    Storage(StorageError),
}

impl fmt::Display for LeaseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Lost => f.write_str("the claim's lease was lost"),
            Self::Storage(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for LeaseError {}

impl From<StorageError> for LeaseError {
    fn from(e: StorageError) -> Self {
        Self::Storage(e)
    }
}
