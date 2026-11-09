//! Fixture: composition, which may spawn processes, re-exports `Command` under another name.

pub use std::process::Command as Spawner;
