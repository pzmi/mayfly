//! Fixture: a pure crate reads the clock through a qualified self type, which a plain path
//! match never saw (checkpoint finding H2).

pub fn stamp() -> bool {
    <std::time::SystemTime>::now() > std::time::SystemTime::UNIX_EPOCH
}
