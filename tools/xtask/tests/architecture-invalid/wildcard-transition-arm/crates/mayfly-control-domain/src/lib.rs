//! Fixture: a transition function whose match falls through a wildcard arm, so a new state or
//! stimulus would be swallowed instead of failing to compile. The same wildcard in a function
//! that is not a transition function is allowed.

pub enum State {
    Idle,
    Busy,
}

pub enum Stimulus {
    Start,
    Stop,
}

pub fn decide(state: &State, stimulus: &Stimulus) -> bool {
    match (state, stimulus) {
        (State::Idle, Stimulus::Start) => true,
        _ => false,
    }
}

pub fn describe(state: &State) -> &'static str {
    match state {
        State::Idle => "idle",
        _ => "other",
    }
}
