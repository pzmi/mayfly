//! A projection spec that keys a row by another stream it names.

pub struct StreamId(pub String);

pub trait ProjectionSpec {
    fn apply(event: &str) -> Vec<(String, u32)>;
}

pub struct FailuresPerPool;

impl ProjectionSpec for FailuresPerPool {
    fn apply(event: &str) -> Vec<(String, u32)> {
        let pool = StreamId(format!("pool-{event}"));
        vec![(pool.0, 1)]
    }
}
