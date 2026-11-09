//! Time as values.

pub use std::time::Duration as Span;

pub struct Deadline(pub std::time::Instant);
