//! The latest-state channel.

pub struct ReloadState;

impl ReloadState {
    pub fn current(&self) -> u64 {
        0
    }

    pub fn publish_next(&self) -> u64 {
        1
    }
}
