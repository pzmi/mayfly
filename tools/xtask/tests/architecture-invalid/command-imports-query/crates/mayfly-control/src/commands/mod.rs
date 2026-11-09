//! Fixture: a command reuses the read side through the crate root's re-export.

use crate::Read;

pub struct Apply;

impl Apply {
    pub fn run(&self, read: &Read) -> u64 {
        read.latest()
    }
}
