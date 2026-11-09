//! Fixture: a crate compiles a file through a renamed `include!`, which the loader never
//! followed, so the clock read in it was never checked (second checkpoint review H1).

use std::include as inc;
inc!("clock.rs");
