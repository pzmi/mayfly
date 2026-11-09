//! Fixture: a production crate embeds another package's source as data, so the two
//! agree by construction rather than through an edge the policy sees.

pub const TART: &str = include_str!("../../mayfly-adapter-tart/src/lib.rs");
