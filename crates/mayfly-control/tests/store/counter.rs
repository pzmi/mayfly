//! `Counter`: a small aggregate whose decisions are easy to steer, the newer build of it that
//! writes what this build cannot decide, and an aggregate whose stream prefix overlaps it.

use mayfly_control_domain::{
    Actor, Aggregate, AggregateRegistry, AggregateVisitor, Command, CommandBody, DeterminismCase,
    DeterminismCases, DomainEvent, Expect, Rejection, event_set,
};
use mayfly_domain::{CommandId, Epoch, Millis};
use serde::{Deserialize, Serialize};

/// The counter went up.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Incremented {
    pub by: u32,
}
impl DomainEvent for Incremented {
    const EVENT_TYPE: &'static str = "counter_incremented";
    const SCHEMA_VERSION: u32 = 1;
}

/// The counter went back to zero.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WentToZero {}
impl DomainEvent for WentToZero {
    const EVENT_TYPE: &'static str = "counter_reset";
    const SCHEMA_VERSION: u32 = 1;
}

/// A newer build's event an older build must not skip: it needs epoch 2.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Sealed {}
impl DomainEvent for Sealed {
    const EVENT_TYPE: &'static str = "counter_sealed";
    const SCHEMA_VERSION: u32 = 1;
    const REQUIRED_EPOCH: u32 = 2;
}

/// A newer build's event this build does not know, at epoch 1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Noted {}
impl DomainEvent for Noted {
    const EVENT_TYPE: &'static str = "counter_noted";
    const SCHEMA_VERSION: u32 = 1;
}

event_set! {
    /// What happened to a counter, as this build knows it.
    pub enum CounterEvent {
        /// Up.
        Incremented(Incremented),
        /// Zero.
        WentToZero(WentToZero),
    }
}

event_set! {
    /// What happened to a counter, as the newer build knows it.
    pub enum NewerCounterEvent {
        /// Up.
        Incremented(Incremented),
        /// Zero.
        WentToZero(WentToZero),
        /// Sealed (epoch 2).
        Sealed(Sealed),
        /// Noted.
        Noted(Noted),
    }
}

/// What a counter is asked.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum CounterBody {
    /// Add `by`, refused past `limit` (a value the caller read elsewhere, such as a pool's).
    Increment { by: u32, limit: u32 },
    /// Back to zero.
    Reset,
    /// Seal (newer build only).
    Seal,
    /// Note (newer build only).
    Note,
}
impl CommandBody for CounterBody {
    const TYPES: &'static [&'static str] = &["increment", "reset", "seal", "note"];

    fn command_type(&self) -> &'static str {
        match self {
            Self::Increment { .. } => "increment",
            Self::Reset => "reset",
            Self::Seal => "seal",
            Self::Note => "note",
        }
    }
}

/// Why a counter refused.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CounterRejection {
    AtLimit,
    Unsupported,
}
impl Rejection for CounterRejection {
    const CODES: &'static [&'static str] = &["at_limit", "unsupported"];

    fn code(&self) -> &'static str {
        match self {
            Self::AtLimit => "at_limit",
            Self::Unsupported => "unsupported",
        }
    }
}

/// A command with id `id`.
pub fn command(id: u128, body: CounterBody) -> Command<CounterBody> {
    Command::new(
        CommandId::from_u128(id),
        Millis::new(1_000),
        Epoch::FIRST,
        Actor::Startup,
        body,
    )
}

fn decide_count(count: u32, body: CounterBody) -> Result<Vec<CounterEvent>, CounterRejection> {
    match body {
        CounterBody::Increment { by: 0, .. } => Ok(Vec::new()),
        CounterBody::Increment { by, limit } if count.saturating_add(by) > limit => {
            Err(CounterRejection::AtLimit)
        }
        CounterBody::Increment { by, .. } => {
            Ok(vec![CounterEvent::Incremented(Incremented { by })])
        }
        CounterBody::Reset => Ok(vec![CounterEvent::WentToZero(WentToZero {})]),
        CounterBody::Seal | CounterBody::Note => Err(CounterRejection::Unsupported),
    }
}

macro_rules! counter {
    ($name:ident = $aggregate:literal) => {
        /// A counter aggregate of this build.
        pub struct $name;
        impl Aggregate for $name {
            const NAME: &'static str = $aggregate;
            type State = u32;
            type Body = CounterBody;
            type Event = CounterEvent;
            type Rejection = CounterRejection;

            fn initial() -> u32 {
                0
            }

            fn decide(
                count: &u32,
                command: Command<CounterBody>,
            ) -> Result<Vec<CounterEvent>, CounterRejection> {
                decide_count(*count, command.body)
            }

            fn evolve(count: u32, event: &CounterEvent) -> u32 {
                match event {
                    CounterEvent::Incremented(e) => count.saturating_add(e.by),
                    CounterEvent::WentToZero(_) => 0,
                }
            }
        }
        impl DeterminismCases for $name {
            fn determinism_cases() -> Vec<DeterminismCase<Self>> {
                vec![DeterminismCase {
                    name: "increment",
                    history: Vec::new(),
                    command: command(1, CounterBody::Increment { by: 1, limit: 9 }),
                    expect: Expect::Accepted,
                }]
            }
        }
    };
}

counter!(Counter = "counter");
counter!(CounterLog = "counter_log");

/// The newer build of `Counter`: supports epoch 2 and decides `Seal` and `Note`.
pub struct NewerCounter;
impl Aggregate for NewerCounter {
    const NAME: &'static str = "counter";
    const SUPPORTED_EPOCH: u32 = 2;
    type State = u32;
    type Body = CounterBody;
    type Event = NewerCounterEvent;
    type Rejection = CounterRejection;

    fn initial() -> u32 {
        0
    }

    fn decide(
        count: &u32,
        command: Command<CounterBody>,
    ) -> Result<Vec<NewerCounterEvent>, CounterRejection> {
        match command.body {
            CounterBody::Seal => Ok(vec![NewerCounterEvent::Sealed(Sealed {})]),
            CounterBody::Note => Ok(vec![NewerCounterEvent::Noted(Noted {})]),
            body @ (CounterBody::Increment { .. } | CounterBody::Reset) => {
                decide_count(*count, body).map(|events| {
                    events
                        .into_iter()
                        .map(|e| match e {
                            CounterEvent::Incremented(e) => NewerCounterEvent::Incremented(e),
                            CounterEvent::WentToZero(e) => NewerCounterEvent::WentToZero(e),
                        })
                        .collect()
                })
            }
        }
    }

    fn evolve(count: u32, event: &NewerCounterEvent) -> u32 {
        match event {
            NewerCounterEvent::Incremented(e) => count.saturating_add(e.by),
            NewerCounterEvent::WentToZero(_) => 0,
            NewerCounterEvent::Sealed(_) | NewerCounterEvent::Noted(_) => count,
        }
    }
}
impl DeterminismCases for NewerCounter {
    fn determinism_cases() -> Vec<DeterminismCase<Self>> {
        Vec::new()
    }
}

/// This build's aggregates.
pub struct Counters;
impl AggregateRegistry for Counters {
    fn accept<V: AggregateVisitor>(visitor: &mut V) {
        visitor.visit::<Counter>();
        visitor.visit::<CounterLog>();
    }
}

/// The newer build's aggregates.
pub struct NewerCounters;
impl AggregateRegistry for NewerCounters {
    fn accept<V: AggregateVisitor>(visitor: &mut V) {
        visitor.visit::<NewerCounter>();
    }
}
