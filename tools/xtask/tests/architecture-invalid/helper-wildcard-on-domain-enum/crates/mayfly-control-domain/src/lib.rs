//! Fixture: a helper that is not named like a transition function falls through a wildcard
//! arm over a domain enum, so a new phase would be swallowed instead of failing to compile.

pub enum Phase {
    Idle,
    Busy,
    Done,
}

pub fn decide(phase: Phase) -> bool {
    is_active(phase)
}

fn is_active(phase: Phase) -> bool {
    match phase {
        Phase::Busy => true,
        _ => false,
    }
}
