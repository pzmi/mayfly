//! Instrumentation: it records what happened and decides nothing.

pub fn record(event: &str) -> bool {
    !event.is_empty()
}
