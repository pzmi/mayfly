//! Fixture: a journal port with a global read beside its per-stream one.

pub struct StreamId;
pub struct RecordedEvent;

pub trait Journal {
    /// One stream's events: allowed.
    fn read_stream(&self, stream: &StreamId) -> Vec<RecordedEvent>;
    /// Every stream's events in one call: a global order.
    fn read_all(&self) -> Vec<RecordedEvent>;
}
