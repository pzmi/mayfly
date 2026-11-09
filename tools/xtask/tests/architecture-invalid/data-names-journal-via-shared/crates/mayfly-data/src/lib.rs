//! Fixture: the data plane takes the journal through the shared crate's re-export (checkpoint
//! finding M3): the data plane names no journal type, by any name.

pub fn record(_log: &dyn mayfly_protocol::Log) {}
