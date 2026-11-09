use mayfly_domain::Revision;

use crate::observability;
use crate::reload_state::ReloadState;

pub enum Outcome {
    Published(u64),
    Unchanged,
}

pub struct Apply {
    pending: bool,
}

impl Apply {
    pub fn run(&mut self, state: &ReloadState, revision: &mut Revision) -> Outcome {
        observability::record("apply");
        if self.pending {
            self.pending = false;
            let next = state.publish_next();
            revision.0 = next;
            observability::record("published");
            Outcome::Published(next)
        } else {
            Outcome::Unchanged
        }
    }
}
