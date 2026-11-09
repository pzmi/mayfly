//! Fixture: a pure crate passes the lint level to a macro as a metavariable, so the expanded
//! `#[allow(clippy::disallowed_methods)]` switched the semantic layer off for the function
//! while no `allow` was written (third checkpoint review H1).

macro_rules! lvl {
    ($l:ident) => {
        #[$l(clippy::disallowed_methods)]
        pub fn stamp() -> u64 {
            0
        }
    };
}

lvl!(allow);
