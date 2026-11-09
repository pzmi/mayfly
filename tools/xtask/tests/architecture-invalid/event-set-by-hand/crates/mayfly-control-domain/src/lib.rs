//! Fixture: an event set written by hand instead of with `event_set!`, so it has no generated
//! shape and the golden harness could not check its payloads.

pub trait EventSet {}

pub enum LampEvent {
    Lit,
}

impl EventSet for LampEvent {}
