//! Fixture: a command lets a metrics reading decide whether it publishes.

use crate::reload_state::ReloadState;

pub struct Apply;

impl Apply {
    pub fn run(&self, state: &ReloadState) -> bool {
        if crate::metrics::queue_depth() > 10 {
            return false;
        }
        state.publish_next();
        true
    }
}
