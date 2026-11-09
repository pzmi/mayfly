//! Fixture: two aggregates, one missing from `ControlAggregates::accept`, so the golden and
//! determinism harnesses would skip it without failing.

pub trait Aggregate {}
pub trait AggregateVisitor {
    fn visit<A: Aggregate>(&mut self);
}
pub trait AggregateRegistry {
    fn accept<V: AggregateVisitor>(visitor: &mut V);
}

pub struct Attempt;
pub struct Host;

impl Aggregate for Attempt {}
impl Aggregate for Host {}

pub struct ControlAggregates;

impl AggregateRegistry for ControlAggregates {
    fn accept<V: AggregateVisitor>(visitor: &mut V) {
        visitor.visit::<Attempt>();
    }
}
