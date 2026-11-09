//! Test aggregates for the harnesses' own tests: `Door` keeps the contract (with a legacy
//! event version), the others each break one rule on purpose.

use std::sync::atomic::{AtomicU32, Ordering};

use mayfly_control_domain::{
    Aggregate, AggregateRegistry, AggregateVisitor, Command, DeterminismCase, DeterminismCases,
    DomainEvent, Expect, Rejection, event_set,
};
use mayfly_domain::{CommandId, Millis};
use serde::{Deserialize, Serialize};

/// A door was opened, by someone.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DoorOpened {
    pub by: u32,
}
impl DomainEvent for DoorOpened {
    const EVENT_TYPE: &'static str = "door_opened";
    const SCHEMA_VERSION: u32 = 2;
}

/// Version 1 did not say who opened the door.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DoorOpenedV1 {}
impl DomainEvent for DoorOpenedV1 {
    const EVENT_TYPE: &'static str = "door_opened";
    const SCHEMA_VERSION: u32 = 1;
}

/// A door was closed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DoorClosed {}
impl DomainEvent for DoorClosed {
    const EVENT_TYPE: &'static str = "door_closed";
    const SCHEMA_VERSION: u32 = 1;
}

fn upcast_door_opened_v1(_old: DoorOpenedV1) -> DoorEvent {
    DoorEvent::Opened(DoorOpened { by: 0 })
}

event_set! {
    /// What happened to a door.
    pub enum DoorEvent {
        /// Opened.
        Opened(DoorOpened),
        /// Closed.
        Closed(DoorClosed),
    }
    legacy {
        DoorOpenedV1 => upcast_door_opened_v1,
    }
}

/// What a door can be asked to do.
#[derive(Debug, Clone, PartialEq)]
pub enum DoorBody {
    Open { by: u32 },
    Close,
}

/// Why a door refused.
#[derive(Debug, Clone, PartialEq)]
pub enum DoorRejection {
    AlreadyOpen,
    AlreadyClosed,
}
impl Rejection for DoorRejection {
    fn code(&self) -> &'static str {
        match self {
            Self::AlreadyOpen => "already_open",
            Self::AlreadyClosed => "already_closed",
        }
    }
}

pub fn command<B>(body: B) -> Command<B> {
    Command::new(CommandId::from_u128(7), Millis::new(1_000), body)
}

pub struct Door;
impl Aggregate for Door {
    const NAME: &'static str = "door";
    type State = bool;
    type Body = DoorBody;
    type Event = DoorEvent;
    type Rejection = DoorRejection;

    fn initial() -> bool {
        false
    }

    fn decide(open: &bool, command: Command<DoorBody>) -> Result<Vec<DoorEvent>, DoorRejection> {
        match (open, command.body) {
            (false, DoorBody::Open { by }) => Ok(vec![DoorEvent::Opened(DoorOpened { by })]),
            (true, DoorBody::Open { .. }) => Err(DoorRejection::AlreadyOpen),
            (true, DoorBody::Close) => Ok(vec![DoorEvent::Closed(DoorClosed {})]),
            (false, DoorBody::Close) => Err(DoorRejection::AlreadyClosed),
        }
    }

    fn evolve(_open: bool, event: &DoorEvent) -> bool {
        match event {
            DoorEvent::Opened(_) => true,
            DoorEvent::Closed(_) => false,
        }
    }
}
impl DeterminismCases for Door {
    fn determinism_cases() -> Vec<DeterminismCase<Self>> {
        vec![
            DeterminismCase {
                name: "open_closed_door",
                history: vec![],
                command: command(DoorBody::Open { by: 3 }),
                expect: Expect::Accepted,
            },
            DeterminismCase {
                name: "close_open_door",
                history: vec![DoorEvent::Opened(DoorOpened { by: 1 })],
                command: command(DoorBody::Close),
                expect: Expect::Accepted,
            },
            DeterminismCase {
                name: "open_open_door",
                history: vec![DoorEvent::Opened(DoorOpened { by: 1 })],
                command: command(DoorBody::Open { by: 3 }),
                expect: Expect::Refused,
            },
        ]
    }
}

/// A door whose `decide` reads hidden mutable state: each call opens it "by" someone new.
pub struct FlakyDoor;
static CALLS: AtomicU32 = AtomicU32::new(0);
impl Aggregate for FlakyDoor {
    const NAME: &'static str = "flaky_door";
    type State = bool;
    type Body = DoorBody;
    type Event = DoorEvent;
    type Rejection = DoorRejection;

    fn initial() -> bool {
        Door::initial()
    }

    fn decide(open: &bool, command: Command<DoorBody>) -> Result<Vec<DoorEvent>, DoorRejection> {
        let by = CALLS.fetch_add(1, Ordering::Relaxed);
        Door::decide(open, command).map(|events| {
            events
                .into_iter()
                .chain([DoorEvent::Opened(DoorOpened { by })])
                .collect()
        })
    }

    fn evolve(open: bool, event: &DoorEvent) -> bool {
        Door::evolve(open, event)
    }
}
impl DeterminismCases for FlakyDoor {
    fn determinism_cases() -> Vec<DeterminismCase<Self>> {
        Door::determinism_cases().into_iter().map(recase).collect()
    }
}

/// A door whose module lists only accepted cases.
pub struct OptimistDoor;
impl Aggregate for OptimistDoor {
    const NAME: &'static str = "optimist_door";
    type State = bool;
    type Body = DoorBody;
    type Event = DoorEvent;
    type Rejection = DoorRejection;

    fn initial() -> bool {
        Door::initial()
    }

    fn decide(open: &bool, command: Command<DoorBody>) -> Result<Vec<DoorEvent>, DoorRejection> {
        Door::decide(open, command)
    }

    fn evolve(open: bool, event: &DoorEvent) -> bool {
        Door::evolve(open, event)
    }
}
impl DeterminismCases for OptimistDoor {
    fn determinism_cases() -> Vec<DeterminismCase<Self>> {
        Door::determinism_cases()
            .into_iter()
            .filter(|c| c.expect == Expect::Accepted)
            .map(recase)
            .collect()
    }
}

/// A door whose module lists no cases at all.
pub struct SilentDoor;
impl Aggregate for SilentDoor {
    const NAME: &'static str = "silent_door";
    type State = bool;
    type Body = DoorBody;
    type Event = DoorEvent;
    type Rejection = DoorRejection;

    fn initial() -> bool {
        Door::initial()
    }

    fn decide(open: &bool, command: Command<DoorBody>) -> Result<Vec<DoorEvent>, DoorRejection> {
        Door::decide(open, command)
    }

    fn evolve(open: bool, event: &DoorEvent) -> bool {
        Door::evolve(open, event)
    }
}
impl DeterminismCases for SilentDoor {
    fn determinism_cases() -> Vec<DeterminismCase<Self>> {
        Vec::new()
    }
}

/// A door whose cases declare the opposite of what `decide` does.
pub struct ConfusedDoor;
impl Aggregate for ConfusedDoor {
    const NAME: &'static str = "confused_door";
    type State = bool;
    type Body = DoorBody;
    type Event = DoorEvent;
    type Rejection = DoorRejection;

    fn initial() -> bool {
        Door::initial()
    }

    fn decide(open: &bool, command: Command<DoorBody>) -> Result<Vec<DoorEvent>, DoorRejection> {
        Door::decide(open, command)
    }

    fn evolve(open: bool, event: &DoorEvent) -> bool {
        Door::evolve(open, event)
    }
}
impl DeterminismCases for ConfusedDoor {
    fn determinism_cases() -> Vec<DeterminismCase<Self>> {
        Door::determinism_cases()
            .into_iter()
            .map(|c| DeterminismCase {
                expect: match c.expect {
                    Expect::Accepted => Expect::Refused,
                    Expect::Refused => Expect::Accepted,
                },
                ..recase(c)
            })
            .collect()
    }
}

fn recase<A: Aggregate<Event = DoorEvent, Body = DoorBody>>(
    case: DeterminismCase<Door>,
) -> DeterminismCase<A> {
    DeterminismCase {
        name: case.name,
        history: case.history,
        command: case.command,
        expect: case.expect,
    }
}

/// A gate swung (version 3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GateSwung {}
impl DomainEvent for GateSwung {
    const EVENT_TYPE: &'static str = "gate_swung";
    const SCHEMA_VERSION: u32 = 3;
}

/// A gate swung (version 1); version 2 was never given an upcaster.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GateSwungV1 {}
impl DomainEvent for GateSwungV1 {
    const EVENT_TYPE: &'static str = "gate_swung";
    const SCHEMA_VERSION: u32 = 1;
}

fn upcast_gate_swung_v1(_old: GateSwungV1) -> GateEvent {
    GateEvent::Swung(GateSwung {})
}

event_set! {
    /// What happened to a gate.
    pub enum GateEvent {
        /// Swung.
        Swung(GateSwung),
    }
    legacy {
        GateSwungV1 => upcast_gate_swung_v1,
    }
}

/// Never refuses; only its events matter.
#[derive(Debug, Clone, PartialEq)]
pub struct Never;
impl Rejection for Never {
    fn code(&self) -> &'static str {
        "never"
    }
}

/// An aggregate whose event chain skips a version.
pub struct Gate;
impl Aggregate for Gate {
    const NAME: &'static str = "gate";
    type State = ();
    type Body = ();
    type Event = GateEvent;
    type Rejection = Never;

    fn initial() {}

    fn decide(_state: &(), _command: Command<()>) -> Result<Vec<GateEvent>, Never> {
        Ok(vec![GateEvent::Swung(GateSwung {})])
    }

    fn evolve(state: (), _event: &GateEvent) {
        state
    }
}
impl DeterminismCases for Gate {
    fn determinism_cases() -> Vec<DeterminismCase<Self>> {
        Vec::new()
    }
}

macro_rules! registry {
    ($name:ident: $($aggregate:ty),*) => {
        pub struct $name;
        impl AggregateRegistry for $name {
            fn accept<V: AggregateVisitor>(visitor: &mut V) {
                let _ = &visitor;
                $( visitor.visit::<$aggregate>(); )*
            }
        }
    };
}

registry!(Doors: Door);
registry!(NoAggregates:);
registry!(DoorsTwice: Door, Door);
registry!(DoorsAndFlaky: Door, FlakyDoor);
registry!(DoorsAndOptimist: Door, OptimistDoor);
registry!(DoorsAndSilent: Door, SilentDoor);
registry!(DoorsAndConfused: Door, ConfusedDoor);
registry!(Gates: Gate);

/// The canonical fixtures of [`Door`]'s events.
pub const DOOR_OPENED_V2: &str = "{\n  \"by\": 4\n}\n";
pub const DOOR_OPENED_V1: &str = "{}\n";
pub const DOOR_OPENED_V1_EXPECTED: &str = "{\n  \"by\": 0\n}\n";
pub const DOOR_CLOSED_V1: &str = "{}\n";
