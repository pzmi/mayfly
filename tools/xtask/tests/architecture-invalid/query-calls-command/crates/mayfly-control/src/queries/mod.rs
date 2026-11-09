//! Fixture: a query runs a command handler to compute its answer.

use super::commands::Apply;

pub struct Read;

impl Read {
    pub fn latest(&self) -> u64 {
        let _ = Apply;
        0
    }
}
