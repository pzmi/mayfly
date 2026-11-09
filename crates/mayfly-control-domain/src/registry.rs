//! The one registry of the control plane's aggregates, and through them of its events.

use crate::aggregate::{AggregateRegistry, AggregateVisitor, DeterminismCases};
use crate::attempt::Attempt;
use crate::event::{EventKind, EventSet};

/// Every aggregate of the control plane. An aggregate's events are registered by its
/// `Event` set, so this list is also the event registry (plan §5.3): the golden harness walks
/// it for fixtures and the determinism harness for cases.
///
/// Each aggregate adds one `visitor.visit::<Aggregate>()` line (`ARCH-aggregate-registered`).
#[derive(Debug, Clone, Copy, Default)]
pub struct ControlAggregates;

impl AggregateRegistry for ControlAggregates {
    fn accept<V: AggregateVisitor>(visitor: &mut V) {
        visitor.visit::<Attempt>();
    }
}

/// Every event kind a registry decodes, current and legacy, in registration order. A journal
/// row whose kind is not in this list is unknown: the daemon starts in safe mode (fnd §6.8).
#[must_use]
pub fn known_kinds<R: AggregateRegistry>() -> Vec<EventKind> {
    struct Kinds(Vec<EventKind>);
    impl AggregateVisitor for Kinds {
        fn visit<A: DeterminismCases>(&mut self) {
            self.0.extend_from_slice(<A::Event as EventSet>::KINDS);
            self.0
                .extend_from_slice(<A::Event as EventSet>::LEGACY_KINDS);
        }
    }
    let mut kinds = Kinds(Vec::new());
    R::accept(&mut kinds);
    kinds.0
}
