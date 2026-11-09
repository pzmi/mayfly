//! The aggregate contract (plan §5.1) and the determinism cases every aggregate exports
//! (ver §3.1).

use std::fmt::Debug;

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::command::Command;
use crate::event::EventSet;

/// Why `decide` refused a command. Each aggregate has its own refusal type; `code` is the
/// stable `snake_case` name a command receipt and the operator see.
///
/// A refusal is recorded in a refusal receipt (ADR 0034, E3) as its serde form and answers a
/// repeat at the same revisions from there, so it serialises and deserialises; it carries codes
/// and ids only, never free text (ES-I13). The control plane's `AggregateStore` pins the round
/// trip for every refused determinism case of the registry (`mayfly-control`'s
/// `should_round_trip_every_refusal_when_registry_cases_are_encoded`).
pub trait Rejection: Clone + Debug + PartialEq + Serialize + DeserializeOwned {
    /// Every code [`Rejection::code`] can return. The determinism harness refuses a case whose
    /// refusal code is not listed and a listed code that no refused case returns, and pins the
    /// codes and outcomes in the aggregate's decisions golden, so a renamed code shows in review
    /// (`mayfly_testkit::determinism`, pinned by its
    /// `should_fail_when_refusal_code_is_not_declared`,
    /// `should_fail_when_a_declared_code_has_no_refused_case` and
    /// `should_fail_when_decisions_differ_from_the_golden`).
    const CODES: &'static [&'static str];

    /// A fixed `snake_case` code, never formatted from data.
    fn code(&self) -> &'static str;
}

/// An aggregate's command body: what a command asks, with its stable type name and the
/// canonical form its intent digest is computed from (`doc/event-store.md` §3.7 step 4).
///
/// `command_type` names the body's variant as a `snake_case` name recorded in the command
/// receipt; `TYPES` lists every name it can return. The determinism harness refuses a case whose
/// type is not listed, a listed type no case decides and a malformed list
/// (`mayfly_testkit::determinism`, pinned by its `should_fail_when_command_type_is_not_declared`,
/// `should_fail_when_a_declared_command_type_has_no_case` and
/// `should_fail_when_command_types_are_malformed`). The serde form is the canonical intent
/// encoding: `AggregateStore` hashes it, so a body type that does not serialise cannot be
/// stored at all.
pub trait CommandBody: Clone + Debug + Serialize {
    /// Every name [`CommandBody::command_type`] can return.
    const TYPES: &'static [&'static str];

    /// The body's type, a fixed `snake_case` name, never formatted from data.
    fn command_type(&self) -> &'static str;
}

/// One event-sourced aggregate: a pure decision function over a state folded from its events.
///
/// Implementations read no clock, mint no id, draw no randomness and log nothing: time and
/// ids arrive in the [`Command`] envelope (fnd §6.2). The implementing type is a marker; the
/// data lives in [`Aggregate::State`].
pub trait Aggregate {
    /// The stream prefix (`attempt`, `host`, …): streams are named `<NAME>-<id>` (plan §5.1).
    const NAME: &'static str;
    /// The highest stream epoch this build decides for the aggregate (`doc/event-store.md`
    /// §6.2, ES-I12): the journal refuses an append to a stream holding an event that needs
    /// more, and a batch whose events need more. Raised by a **prepare** release that reads a
    /// new event or field before an **activate** release writes it (§6.1). The golden harness
    /// fails a sample that needs more than this (`mayfly_testkit::golden`, pinned by its
    /// `should_fail_when_a_sample_needs_more_than_the_aggregate_supports`).
    const SUPPORTED_EPOCH: u32 = 1;
    /// The folded state.
    type State: Clone + Debug + PartialEq;
    /// The aggregate's command bodies.
    type Body: CommandBody;
    /// The aggregate's events, declared with [`crate::event_set!`].
    type Event: EventSet;
    /// The aggregate's refusals.
    type Rejection: Rejection;

    /// The state of a stream with no events.
    fn initial() -> Self::State;

    /// Decides what happens: the events to append, or a refusal. Deterministic, refusals
    /// included (`mayfly_testkit::determinism::verify`; its own test
    /// `should_fail_when_decide_is_not_deterministic`).
    ///
    /// # Errors
    /// The aggregate's [`Rejection`] when the command is not allowed in `state`.
    fn decide(
        state: &Self::State,
        command: Command<Self::Body>,
    ) -> Result<Vec<Self::Event>, Self::Rejection>;

    /// Applies one recorded event. Total: an event in the journal happened, so `evolve` never
    /// refuses it.
    fn evolve(state: Self::State, event: &Self::Event) -> Self::State;
}

/// Folds `events` onto the initial state: how a stream is loaded.
pub fn replay<'e, A: Aggregate>(events: impl IntoIterator<Item = &'e A::Event>) -> A::State
where
    A::Event: 'e,
{
    events.into_iter().fold(A::initial(), A::evolve)
}

/// The outcome a determinism case declares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Expect {
    /// `decide` returns events (possibly none).
    Accepted,
    /// `decide` returns a rejection.
    Refused,
}

/// One determinism case (ver §3.1): fold `history`, decide `command`, expect `expect`.
///
/// `Debug` and `Clone` are written by hand: a derive would bound them on the marker type `A`,
/// which implements neither, instead of on its events and bodies.
pub struct DeterminismCase<A: Aggregate> {
    /// Unique within the aggregate; shown when the case fails.
    pub name: &'static str,
    /// The events folded from [`Aggregate::initial`] before deciding.
    pub history: Vec<A::Event>,
    /// The command decided.
    pub command: Command<A::Body>,
    /// The declared outcome.
    pub expect: Expect,
}

impl<A: Aggregate> Debug for DeterminismCase<A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeterminismCase")
            .field("aggregate", &A::NAME)
            .field("name", &self.name)
            .field("history", &self.history)
            .field("command", &self.command)
            .field("expect", &self.expect)
            .finish()
    }
}

impl<A: Aggregate> Clone for DeterminismCase<A> {
    fn clone(&self) -> Self {
        Self {
            name: self.name,
            history: self.history.clone(),
            command: self.command.clone(),
            expect: self.expect,
        }
    }
}

/// The `DETERMINISM_CASES` of one aggregate module (ver §3.1): at least one accepted and one
/// refused case, at least one of them with a non-empty history, names unique. A registry
/// admits only aggregates that implement this, so a registered aggregate without cases does
/// not compile and one with an empty, one-sided or history-free list fails
/// `mayfly_testkit::determinism::verify` (pinned by the testkit's
/// `should_fail_when_aggregate_module_has_no_cases`,
/// `should_fail_when_aggregate_module_has_no_refused_case` and
/// `should_fail_when_no_case_has_a_history`).
pub trait DeterminismCases: Aggregate + Sized {
    /// The cases, built fresh on each call.
    fn determinism_cases() -> Vec<DeterminismCase<Self>>;
}

/// Receives each aggregate of a registry, as a type.
pub trait AggregateVisitor {
    /// Called once per registered aggregate.
    fn visit<A: DeterminismCases>(&mut self);
}

/// A list of aggregates, walked by visitors: the harnesses, the event decoder, safe mode's
/// known-kind check. Implemented by [`crate::ControlAggregates`] and by test registries.
pub trait AggregateRegistry {
    /// Calls `visitor.visit::<A>()` for every aggregate `A`, in a fixed order.
    fn accept<V: AggregateVisitor>(visitor: &mut V);
}
