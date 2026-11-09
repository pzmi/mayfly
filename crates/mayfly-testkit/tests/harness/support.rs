//! Test aggregates for the harnesses' own tests: `Door` keeps the contract (with a legacy
//! event version and an optional, enum-valued field), the others each break one rule on
//! purpose.

use std::cell::Cell;
use std::fmt;

use mayfly_control_domain::shape::Shape;
use mayfly_control_domain::{
    Actor, Aggregate, AggregateRegistry, AggregateVisitor, Command, CommandBody, DecodeError,
    DeterminismCase, DeterminismCases, DomainEvent, EventKind, EventSet, Expect, Rejection,
    event_set,
};
use mayfly_domain::{CommandId, Epoch, Millis};
use serde::{Deserialize, Serialize};

thread_local! {
    static TICKS: Cell<u32> = const { Cell::new(0) };
}

/// Hidden mutable state for the broken aggregates: a new value on every call. Per thread, so
/// two calls within one test always differ, whatever other tests run alongside.
pub fn tick() -> u32 {
    TICKS.with(|ticks| {
        let now = ticks.get();
        ticks.set(now.wrapping_add(1));
        now
    })
}

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

/// Why a door closed, when known.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CloseReason {
    Wind,
    Hand { by: u32 },
}

/// A door was closed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DoorClosed {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<CloseReason>,
}
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
#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum DoorBody {
    Open { by: u32 },
    Close,
}
impl CommandBody for DoorBody {
    const TYPES: &'static [&'static str] = &["open", "close"];

    fn command_type(&self) -> &'static str {
        match self {
            Self::Open { .. } => "open",
            Self::Close => "close",
        }
    }
}

/// The body of an aggregate that is asked nothing in particular.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Nothing;
impl CommandBody for Nothing {
    const TYPES: &'static [&'static str] = &["nothing"];

    fn command_type(&self) -> &'static str {
        "nothing"
    }
}

/// Why a door refused.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum DoorRejection {
    AlreadyOpen,
    AlreadyClosed,
}
impl Rejection for DoorRejection {
    const CODES: &'static [&'static str] = &["already_open", "already_closed"];

    fn code(&self) -> &'static str {
        match self {
            Self::AlreadyOpen => "already_open",
            Self::AlreadyClosed => "already_closed",
        }
    }
}

pub fn command<B>(body: B) -> Command<B> {
    Command::new(
        CommandId::from_u128(7),
        Millis::new(1_000),
        Epoch::FIRST,
        Actor::Startup,
        body,
    )
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
            (true, DoorBody::Close) => Ok(vec![DoorEvent::Closed(DoorClosed { reason: None })]),
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
            DeterminismCase {
                name: "close_closed_door",
                history: vec![],
                command: command(DoorBody::Close),
                expect: Expect::Refused,
            },
        ]
    }
}

/// `Door`'s decisions golden.
pub const DOOR_DECISIONS: &str = "\
# door: refusal codes, then the outcome of each determinism case
code already_closed
code already_open
case close_closed_door refused already_closed
case close_open_door accepted
  door_closed/v1 {}
case open_closed_door accepted
  door_opened/v2 {\"by\":3}
case open_open_door refused already_open
";

/// A `Door` case as another door-shaped aggregate's case.
pub fn recase<A: Aggregate<Event = DoorEvent, Body = DoorBody>>(
    case: DeterminismCase<Door>,
) -> DeterminismCase<A> {
    DeterminismCase {
        name: case.name,
        history: case.history,
        command: case.command,
        expect: case.expect,
    }
}

/// A door that decides and evolves like `Door` but lists other cases, built from `Door`'s
/// (`$cases`, already recased) by `$build`.
macro_rules! door_with_cases {
    ($(#[$meta:meta])* $name:ident = $aggregate:literal, |$cases:ident| $build:expr) => {
        $(#[$meta])*
        pub struct $name;
        impl Aggregate for $name {
            const NAME: &'static str = $aggregate;
            type State = bool;
            type Body = DoorBody;
            type Event = DoorEvent;
            type Rejection = DoorRejection;

            fn initial() -> bool {
                Door::initial()
            }

            fn decide(
                open: &bool,
                command: Command<DoorBody>,
            ) -> Result<Vec<DoorEvent>, DoorRejection> {
                Door::decide(open, command)
            }

            fn evolve(open: bool, event: &DoorEvent) -> bool {
                Door::evolve(open, event)
            }
        }
        impl DeterminismCases for $name {
            fn determinism_cases() -> Vec<DeterminismCase<Self>> {
                let $cases = Door::determinism_cases().into_iter().map(recase::<Self>);
                $build
            }
        }
    };
}

/// A door that decides like `Door` but refuses with `$refuse(DoorRejection)`, over `Door`'s
/// cases.
macro_rules! door_refusing {
    ($(#[$meta:meta])* $name:ident = $aggregate:literal, $rejection:ty, $refuse:expr) => {
        $(#[$meta])*
        pub struct $name;
        impl Aggregate for $name {
            const NAME: &'static str = $aggregate;
            type State = bool;
            type Body = DoorBody;
            type Event = DoorEvent;
            type Rejection = $rejection;

            fn initial() -> bool {
                Door::initial()
            }

            fn decide(open: &bool, command: Command<DoorBody>) -> Result<Vec<DoorEvent>, $rejection> {
                Door::decide(open, command).map_err($refuse)
            }

            fn evolve(open: bool, event: &DoorEvent) -> bool {
                Door::evolve(open, event)
            }
        }
        impl DeterminismCases for $name {
            fn determinism_cases() -> Vec<DeterminismCase<Self>> {
                Door::determinism_cases().into_iter().map(recase).collect()
            }
        }
    };
}

/// A door whose body is `$body`, a `DoorBody` behind another `CommandBody` impl, over
/// `Door`'s cases.
macro_rules! door_typed {
    ($(#[$meta:meta])* $name:ident = $aggregate:literal, $body:ident) => {
        $(#[$meta])*
        pub struct $name;
        impl Aggregate for $name {
            const NAME: &'static str = $aggregate;
            type State = bool;
            type Body = $body;
            type Event = DoorEvent;
            type Rejection = DoorRejection;

            fn initial() -> bool {
                Door::initial()
            }

            fn decide(open: &bool, c: Command<$body>) -> Result<Vec<DoorEvent>, DoorRejection> {
                Door::decide(
                    open,
                    Command::new(c.command_id, c.issued_at, c.run_epoch, c.actor, c.body.0),
                )
            }

            fn evolve(open: bool, event: &DoorEvent) -> bool {
                Door::evolve(open, event)
            }
        }
        impl DeterminismCases for $name {
            fn determinism_cases() -> Vec<DeterminismCase<Self>> {
                Door::determinism_cases()
                    .into_iter()
                    .map(|c| DeterminismCase {
                        name: c.name,
                        history: c.history,
                        command: Command::new(
                            c.command.command_id,
                            c.command.issued_at,
                            c.command.run_epoch,
                            c.command.actor,
                            $body(c.command.body),
                        ),
                        expect: c.expect,
                    })
                    .collect()
            }
        }
    };
}

/// A door body whose `TYPES` leave out `close`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Unlisted(pub DoorBody);
impl CommandBody for Unlisted {
    const TYPES: &'static [&'static str] = &["open"];

    fn command_type(&self) -> &'static str {
        self.0.command_type()
    }
}

door_typed!(
    /// A door whose cases decide a command type its `TYPES` do not list.
    UnlistedDoor = "unlisted_door",
    Unlisted
);

/// A door body whose `TYPES` list a type no case decides.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Boastful(pub DoorBody);
impl CommandBody for Boastful {
    const TYPES: &'static [&'static str] = &["open", "close", "slam"];

    fn command_type(&self) -> &'static str {
        self.0.command_type()
    }
}

door_typed!(
    /// A door that declares a command type none of its cases decides.
    BoastfulDoor = "boastful_door",
    Boastful
);

/// A door body whose `TYPES` hold a malformed type and one twice.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Mangled(pub DoorBody);
impl CommandBody for Mangled {
    const TYPES: &'static [&'static str] = &["Open", "close", "close", "open"];

    fn command_type(&self) -> &'static str {
        self.0.command_type()
    }
}

door_typed!(
    /// A door whose command `TYPES` are malformed.
    MangledDoor = "mangled_door",
    Mangled
);

door_with_cases!(
    /// A door whose module lists only accepted cases.
    OptimistDoor = "optimist_door",
    |cases| cases.filter(|c| c.expect == Expect::Accepted).collect()
);

door_with_cases!(
    /// A door whose module lists no cases at all.
    SilentDoor = "silent_door",
    |cases| {
        let _ = cases;
        Vec::new()
    }
);

door_with_cases!(
    /// A door whose cases declare the opposite of what `decide` does.
    ConfusedDoor = "confused_door",
    |cases| cases
        .map(|c| DeterminismCase {
            expect: match c.expect {
                Expect::Accepted => Expect::Refused,
                Expect::Refused => Expect::Accepted,
            },
            ..c
        })
        .collect()
);

door_with_cases!(
    /// A door whose module declares its first case twice.
    EchoDoor = "echo_door",
    |cases| {
        let mut cases: Vec<_> = cases.collect();
        cases.push(cases[0].clone());
        cases
    }
);

door_with_cases!(
    /// A door whose cases all start from a closed door.
    ForgetfulDoor = "forgetful_door",
    |cases| cases.filter(|c| c.history.is_empty()).collect()
);

/// A door whose `decide` reads hidden mutable state: each call opens it "by" someone new.
pub struct FlakyDoor;
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
        let by = tick();
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

/// A refusal that carries hidden mutable state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Grudge {
    pub count: u32,
}
impl Rejection for Grudge {
    const CODES: &'static [&'static str] = &["grudge"];

    fn code(&self) -> &'static str {
        "grudge"
    }
}

door_refusing!(
    /// A door whose refusals differ from call to call.
    GrudgingDoor = "grudging_door",
    Grudge,
    |_| Grudge { count: tick() }
);

/// A refusal whose `PartialEq` and `Debug` hide the field its code reads: only `code()`
/// tells two of them apart.
#[derive(Clone, Serialize, Deserialize)]
pub struct Moody {
    pub mood: u32,
}
impl PartialEq for Moody {
    fn eq(&self, _other: &Self) -> bool {
        true
    }
}
impl fmt::Debug for Moody {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Moody")
    }
}
impl Rejection for Moody {
    const CODES: &'static [&'static str] = &["calm", "cross"];

    fn code(&self) -> &'static str {
        if self.mood.is_multiple_of(2) {
            "calm"
        } else {
            "cross"
        }
    }
}

door_refusing!(
    /// A door whose refusal code changes from call to call, invisibly to `==` and `Debug`.
    MoodyDoor = "moody_door",
    Moody,
    |_| Moody { mood: tick() }
);

/// A refusal type whose `CODES` leave out `already_open`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Taciturn(pub DoorRejection);
impl Rejection for Taciturn {
    const CODES: &'static [&'static str] = &["already_closed"];

    fn code(&self) -> &'static str {
        self.0.code()
    }
}

door_refusing!(
    /// A door that refuses with a code its `CODES` do not list.
    TaciturnDoor = "taciturn_door",
    Taciturn,
    Taciturn
);

/// A refusal type whose `CODES` list a code no refusal returns.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Wordy(pub DoorRejection);
impl Rejection for Wordy {
    const CODES: &'static [&'static str] = &["already_open", "already_closed", "jammed"];

    fn code(&self) -> &'static str {
        self.0.code()
    }
}

door_refusing!(
    /// A door that declares a code none of its cases is refused with.
    WordyDoor = "wordy_door",
    Wordy,
    Wordy
);

/// A refusal type whose `CODES` list a malformed code and one twice.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Garbled(pub DoorRejection);
impl Rejection for Garbled {
    const CODES: &'static [&'static str] = &[
        "Already-Open",
        "already_closed",
        "already_closed",
        "already_open",
    ];

    fn code(&self) -> &'static str {
        self.0.code()
    }
}

door_refusing!(
    /// A door whose `CODES` are malformed.
    GarbledDoor = "garbled_door",
    Garbled,
    Garbled
);

/// A door whose state remembers a fresh value on every `evolve`.
pub struct DriftingDoor;
impl Aggregate for DriftingDoor {
    const NAME: &'static str = "drifting_door";
    type State = (bool, u32);
    type Body = DoorBody;
    type Event = DoorEvent;
    type Rejection = DoorRejection;

    fn initial() -> (bool, u32) {
        (false, 0)
    }

    fn decide(
        state: &(bool, u32),
        command: Command<DoorBody>,
    ) -> Result<Vec<DoorEvent>, DoorRejection> {
        Door::decide(&state.0, command)
    }

    fn evolve(state: (bool, u32), event: &DoorEvent) -> (bool, u32) {
        (Door::evolve(state.0, event), tick())
    }
}
impl DeterminismCases for DriftingDoor {
    fn determinism_cases() -> Vec<DeterminismCase<Self>> {
        Door::determinism_cases().into_iter().map(recase).collect()
    }
}

/// A state whose `PartialEq` ignores `draft`, which `evolve` draws fresh and `decide` reads;
/// its derived `Debug` shows it.
#[derive(Debug, Clone)]
pub struct VeiledState {
    pub open: bool,
    pub draft: u32,
}
impl PartialEq for VeiledState {
    fn eq(&self, other: &Self) -> bool {
        self.open == other.open
    }
}

/// A door whose equal-looking states hide a field `decide` reads.
pub struct VeiledDoor;
impl Aggregate for VeiledDoor {
    const NAME: &'static str = "veiled_door";
    type State = VeiledState;
    type Body = DoorBody;
    type Event = DoorEvent;
    type Rejection = DoorRejection;

    fn initial() -> VeiledState {
        VeiledState {
            open: false,
            draft: 0,
        }
    }

    fn decide(
        state: &VeiledState,
        command: Command<DoorBody>,
    ) -> Result<Vec<DoorEvent>, DoorRejection> {
        let by = state.draft;
        Door::decide(&state.open, command).map(|events| {
            events
                .into_iter()
                .map(|event| match event {
                    DoorEvent::Opened(_) => DoorEvent::Opened(DoorOpened { by }),
                    other => other,
                })
                .collect()
        })
    }

    fn evolve(state: VeiledState, event: &DoorEvent) -> VeiledState {
        VeiledState {
            open: Door::evolve(state.open, event),
            draft: tick(),
        }
    }
}
impl DeterminismCases for VeiledDoor {
    fn determinism_cases() -> Vec<DeterminismCase<Self>> {
        Door::determinism_cases().into_iter().map(recase).collect()
    }
}

/// An event whose `PartialEq` and `Debug` hide `by`: only its encoding shows it.
#[derive(Clone, Serialize, Deserialize)]
pub struct MutedOpened {
    pub by: u32,
}
impl PartialEq for MutedOpened {
    fn eq(&self, _other: &Self) -> bool {
        true
    }
}
impl fmt::Debug for MutedOpened {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("MutedOpened")
    }
}
impl DomainEvent for MutedOpened {
    const EVENT_TYPE: &'static str = "muted_opened";
    const SCHEMA_VERSION: u32 = 1;
}

event_set! {
    /// What happened to a muted door.
    pub enum MutedEvent {
        /// Opened.
        Opened(MutedOpened),
    }
}

/// A door that opens "by" someone new each time, invisibly to `==` and `Debug`.
pub struct MutedDoor;
impl Aggregate for MutedDoor {
    const NAME: &'static str = "muted_door";
    type State = ();
    type Body = DoorBody;
    type Event = MutedEvent;
    type Rejection = DoorRejection;

    fn initial() {}

    fn decide(_state: &(), command: Command<DoorBody>) -> Result<Vec<MutedEvent>, DoorRejection> {
        match command.body {
            DoorBody::Open { .. } => Ok(vec![MutedEvent::Opened(MutedOpened { by: tick() })]),
            DoorBody::Close => Err(DoorRejection::AlreadyClosed),
        }
    }

    fn evolve(state: (), _event: &MutedEvent) {
        state
    }
}
impl DeterminismCases for MutedDoor {
    fn determinism_cases() -> Vec<DeterminismCase<Self>> {
        vec![
            DeterminismCase {
                name: "open",
                history: vec![MutedEvent::Opened(MutedOpened { by: 0 })],
                command: command(DoorBody::Open { by: 1 }),
                expect: Expect::Accepted,
            },
            DeterminismCase {
                name: "close",
                history: vec![],
                command: command(DoorBody::Close),
                expect: Expect::Refused,
            },
        ]
    }
}

/// Counts keyed by byte strings: a map JSON cannot write, since its keys are not strings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Tallied {
    pub counts: std::collections::BTreeMap<Vec<u8>, u8>,
}
impl DomainEvent for Tallied {
    const EVENT_TYPE: &'static str = "tallied";
    const SCHEMA_VERSION: u32 = 1;
}

event_set! {
    /// What happened to a tally.
    pub enum TallyEvent {
        /// Tallied.
        Tallied(Tallied),
    }
}

/// An aggregate whose accepted event does not encode.
pub struct Tally;
impl Aggregate for Tally {
    const NAME: &'static str = "tally";
    type State = ();
    type Body = DoorBody;
    type Event = TallyEvent;
    type Rejection = DoorRejection;

    fn initial() {}

    fn decide(_state: &(), command: Command<DoorBody>) -> Result<Vec<TallyEvent>, DoorRejection> {
        match command.body {
            DoorBody::Open { .. } => Ok(vec![TallyEvent::Tallied(Tallied {
                counts: [(vec![1], 1)].into(),
            })]),
            DoorBody::Close => Err(DoorRejection::AlreadyClosed),
        }
    }

    fn evolve(state: (), _event: &TallyEvent) {
        state
    }
}
impl DeterminismCases for Tally {
    fn determinism_cases() -> Vec<DeterminismCase<Self>> {
        vec![
            DeterminismCase {
                name: "count",
                history: vec![TallyEvent::Tallied(Tallied { counts: [].into() })],
                command: command(DoorBody::Open { by: 1 }),
                expect: Expect::Accepted,
            },
            DeterminismCase {
                name: "close",
                history: vec![],
                command: command(DoorBody::Close),
                expect: Expect::Refused,
            },
        ]
    }
}

/// A door whose `evolve` is not total: it jams on every close.
pub struct JammedDoor;
impl Aggregate for JammedDoor {
    const NAME: &'static str = "jammed_door";
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
        assert!(!matches!(event, DoorEvent::Closed(_)), "jammed");
        Door::evolve(open, event)
    }
}
impl DeterminismCases for JammedDoor {
    fn determinism_cases() -> Vec<DeterminismCase<Self>> {
        Vec::new()
    }
}

/// Never refuses; only its events matter.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Never;
impl Rejection for Never {
    const CODES: &'static [&'static str] = &["never"];

    fn code(&self) -> &'static str {
        "never"
    }
}

/// An aggregate that exists only for its events: it decides nothing and has no cases.
macro_rules! event_holder {
    ($(#[$meta:meta])* $name:ident = $aggregate:literal, $event:ty) => {
        $(#[$meta])*
        pub struct $name;
        impl Aggregate for $name {
            const NAME: &'static str = $aggregate;
            type State = ();
            type Body = Nothing;
            type Event = $event;
            type Rejection = Never;

            fn initial() {}

            fn decide(_state: &(), _command: Command<Nothing>) -> Result<Vec<$event>, Never> {
                Ok(Vec::new())
            }

            fn evolve(state: (), _event: &$event) {
                state
            }
        }
        impl DeterminismCases for $name {
            fn determinism_cases() -> Vec<DeterminismCase<Self>> {
                Vec::new()
            }
        }
    };
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

/// An aggregate whose event chain skips a version.
pub struct Gate;
impl Aggregate for Gate {
    const NAME: &'static str = "gate";
    type State = ();
    type Body = Nothing;
    type Event = GateEvent;
    type Rejection = Never;

    fn initial() {}

    fn decide(_state: &(), _command: Command<Nothing>) -> Result<Vec<GateEvent>, Never> {
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

/// Version 1 of `door_opened` upcast into the wrong event type.
fn upcast_door_opened_v1_to_closed(_old: DoorOpenedV1) -> MisroutedEvent {
    MisroutedEvent::Closed(DoorClosed { reason: None })
}

event_set! {
    /// `DoorEvent` with a broken upcaster.
    pub enum MisroutedEvent {
        /// Opened.
        Opened(DoorOpened),
        /// Closed.
        Closed(DoorClosed),
    }
    legacy {
        DoorOpenedV1 => upcast_door_opened_v1_to_closed,
    }
}

event_holder!(
    /// A door whose legacy `door_opened` upcasts into `door_closed`.
    MisroutedDoor = "misrouted_door",
    MisroutedEvent
);

/// A kind whose type is not `snake_case`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Shouted {}
impl DomainEvent for Shouted {
    const EVENT_TYPE: &'static str = "Door-Opened";
    const SCHEMA_VERSION: u32 = 1;
}

/// A kind with version 0.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Zeroed {}
impl DomainEvent for Zeroed {
    const EVENT_TYPE: &'static str = "zeroed";
    const SCHEMA_VERSION: u32 = 0;
}

event_set! {
    /// Events with malformed kinds.
    pub enum MalformedEvent {
        /// Badly named.
        Shouted(Shouted),
        /// Badly numbered.
        Zeroed(Zeroed),
    }
}

event_holder!(
    /// An aggregate whose event kinds are malformed.
    Malformed = "malformed",
    MalformedEvent
);

/// A latch was set (version 2, current).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LatchSet {}
impl DomainEvent for LatchSet {
    const EVENT_TYPE: &'static str = "latch_set";
    const SCHEMA_VERSION: u32 = 2;
}

/// A latch was set (version 3), declared legacy by mistake.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LatchSetV3 {}
impl DomainEvent for LatchSetV3 {
    const EVENT_TYPE: &'static str = "latch_set";
    const SCHEMA_VERSION: u32 = 3;
}

/// A latch was set (version 1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LatchSetV1 {}
impl DomainEvent for LatchSetV1 {
    const EVENT_TYPE: &'static str = "latch_set";
    const SCHEMA_VERSION: u32 = 1;
}

fn upcast_latch_set_v3(_old: LatchSetV3) -> LatchEvent {
    LatchEvent::Set(LatchSet {})
}

fn upcast_latch_set_v1(_old: LatchSetV1) -> LatchEvent {
    LatchEvent::Set(LatchSet {})
}

event_set! {
    /// What happened to a latch: a legacy version newer than the current one.
    pub enum LatchEvent {
        /// Set.
        Set(LatchSet),
    }
    legacy {
        LatchSetV1 => upcast_latch_set_v1,
        LatchSetV3 => upcast_latch_set_v3,
    }
}

event_holder!(
    /// An aggregate whose legacy version is newer than its current one.
    Latch = "latch",
    LatchEvent
);

/// A hatch was sealed: an event of an activate release, needing epoch 2.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HatchSealed {}
impl DomainEvent for HatchSealed {
    const EVENT_TYPE: &'static str = "hatch_sealed";
    const SCHEMA_VERSION: u32 = 1;
    const REQUIRED_EPOCH: u32 = 2;
}

event_set! {
    /// What happened to a hatch.
    pub enum HatchEvent {
        /// Sealed.
        Sealed(HatchSealed),
    }
}

event_holder!(
    /// An aggregate supporting epoch 1 whose event needs epoch 2.
    Hatch = "hatch",
    HatchEvent
);

event_set! {
    /// What happened to a twin latch: two current versions of one type.
    pub enum TwinEvent {
        /// Set, version 1.
        Old(LatchSetV1),
        /// Set, version 2.
        New(LatchSet),
    }
}

event_holder!(
    /// An aggregate with two current versions of one event type.
    Twin = "twin",
    TwinEvent
);

/// A signal, internally tagged: serde reads it only from a self-describing format.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Signal {
    Bell,
}

/// A signal was given.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Signalled {
    pub signal: Signal,
}
impl DomainEvent for Signalled {
    const EVENT_TYPE: &'static str = "signalled";
    const SCHEMA_VERSION: u32 = 1;
}

event_set! {
    /// What happened to a signal post.
    pub enum SignalEvent {
        /// Signalled.
        Signalled(Signalled),
    }
}

event_holder!(
    /// An aggregate whose event payload cannot be traced.
    SignalPost = "signal_post",
    SignalEvent
);

/// `DoorEvent` behind an `EventSet` written by hand that reports no shape.
#[derive(Debug, Clone, PartialEq)]
pub struct Shapeless(pub DoorEvent);

impl EventSet for Shapeless {
    const KINDS: &'static [EventKind] = DoorEvent::KINDS;
    const LEGACY_KINDS: &'static [EventKind] = DoorEvent::LEGACY_KINDS;

    fn kind(&self) -> EventKind {
        self.0.kind()
    }

    fn encode<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.encode(serializer)
    }

    fn decode<'de, D: serde::Deserializer<'de>>(
        event_type: &str,
        schema_version: u32,
        deserializer: D,
    ) -> Result<Self, DecodeError<D::Error>> {
        DoorEvent::decode(event_type, schema_version, deserializer).map(Self)
    }

    fn shape(_kind: EventKind) -> Option<Shape> {
        None
    }
}

event_holder!(
    /// An aggregate whose event set has no shape for its kinds.
    ShapelessDoor = "shapeless_door",
    Shapeless
);

/// A ladder's rung was climbed; `step` is the field's old name.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RungClimbed {
    #[serde(alias = "step")]
    pub rung: u8,
}
impl DomainEvent for RungClimbed {
    const EVENT_TYPE: &'static str = "rung_climbed";
    const SCHEMA_VERSION: u32 = 1;
}

event_set! {
    /// What happened to a ladder.
    pub enum LadderEvent {
        /// Climbed.
        Climbed(RungClimbed),
    }
}

event_holder!(
    /// An aggregate whose event declares a serde alias.
    Ladder = "ladder",
    LadderEvent
);

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
registry!(DoorsAndEcho: Door, EchoDoor);
registry!(DoorsAndForgetful: Door, ForgetfulDoor);
registry!(DoorsAndGrudging: Door, GrudgingDoor);
registry!(DoorsAndMoody: Door, MoodyDoor);
registry!(DoorsAndTaciturn: Door, TaciturnDoor);
registry!(DoorsAndWordy: Door, WordyDoor);
registry!(DoorsAndGarbled: Door, GarbledDoor);
registry!(DoorsAndUnlisted: Door, UnlistedDoor);
registry!(DoorsAndBoastful: Door, BoastfulDoor);
registry!(DoorsAndMangled: Door, MangledDoor);
registry!(DoorsAndDrifting: Door, DriftingDoor);
registry!(DoorsAndVeiled: Door, VeiledDoor);
registry!(DoorsAndMuted: Door, MutedDoor);
registry!(DoorsAndTally: Door, Tally);
registry!(Jammed: JammedDoor);
registry!(Gates: Gate);
registry!(Misrouted: MisroutedDoor);
registry!(Malformeds: Malformed);
registry!(Latches: Latch);
registry!(Hatches: Hatch);
registry!(Twins: Twin);
registry!(SignalPosts: SignalPost);
registry!(Shapelesses: ShapelessDoor);
registry!(Ladders: Ladder);

/// The canonical fixtures of [`Door`]'s events.
pub const DOOR_OPENED_V2: &str = "{\n  \"by\": 4\n}\n";
pub const DOOR_OPENED_V1: &str = "{}\n";
pub const DOOR_OPENED_V1_EXPECTED: &str = "{\n  \"by\": 0\n}\n";
pub const DOOR_CLOSED_V1: &str = "{}\n";
pub const DOOR_CLOSED_V1_WIND: &str = "{\n  \"reason\": \"wind\"\n}\n";
pub const DOOR_CLOSED_V1_HAND: &str =
    "{\n  \"reason\": {\n    \"hand\": {\n      \"by\": 2\n    }\n  }\n}\n";
