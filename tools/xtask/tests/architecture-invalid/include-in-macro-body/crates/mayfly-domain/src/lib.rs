//! Fixture: a pure crate compiles a file from a `macro_rules!` body, whose tokens the
//! loader never followed, so the clock read in it was never checked (second checkpoint
//! review H1).

macro_rules! stamp {
    () => {
        include!("clock.rs")
    };
}

pub fn now() -> std::time::SystemTime {
    stamp!()
}
