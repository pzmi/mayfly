//! Fixture: `matches!` over a domain enum is a match with an implicit catch-all, so a new
//! variant would silently count as "not busy".

pub enum Phase {
    Idle,
    Busy,
    Done,
}

pub fn decide(phase: &Phase) -> bool {
    matches!(phase, Phase::Busy)
}
