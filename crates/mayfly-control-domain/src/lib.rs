//! `mayfly-control-domain` (control plane): the pure event-sourcing kernel and the aggregates
//! (`Attempt` in [`attempt`], `Host` in [`host`], `Pool` in [`pool`]; `ScaleSet` to follow),
//! their commands, events and policies (the capacity policy in [`capacity`]).
//!
//! Pure (fnd §2.3): no clock, randomness, ids, I/O or logging. Time and ids arrive in the
//! [`Command`] envelope. `serde` is used for its traits and derive only; formats live in the
//! journal adapter and the testkit's golden harness (ADR 0018).
//!
//! An aggregate plugs in by implementing [`Aggregate`] and [`DeterminismCases`], declaring its
//! events with [`event_set!`], and adding one line to [`ControlAggregates`]; the testkit's
//! golden and determinism harnesses then cover it. [`shape`] traces an event payload's fields
//! and variants, so the golden harness can require a sample of each (ADR 0018).

mod aggregate;
pub mod attempt;
pub mod capacity;
mod command;
pub mod event;
pub mod host;
pub mod pool;
mod registry;
pub mod shape;

pub use aggregate::{
    Aggregate, AggregateRegistry, AggregateVisitor, CommandBody, DeterminismCase, DeterminismCases,
    Expect, Rejection, replay,
};
pub use command::{Actor, Command};
pub use event::{DecodeError, DomainEvent, EventKind, EventSet};
pub use registry::{ControlAggregates, known_kinds};

/// Re-exported for [`event_set!`], so a crate declaring events needs no direct `serde` path.
#[doc(hidden)]
pub use serde as __serde;
