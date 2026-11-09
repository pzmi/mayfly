//! Fixture application: commands, queries and the latest-state channel.

pub mod commands;
pub mod queries;
pub mod reload_state;

pub use commands::Apply;
pub use queries::Read;
