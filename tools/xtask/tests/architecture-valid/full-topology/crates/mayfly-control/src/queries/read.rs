use core::fmt;

use mayfly_domain::Revision;

use crate::reload_state::ReloadState;

pub struct Read;

impl Read {
    pub fn latest(&self, state: &ReloadState) -> u64 {
        state.current()
    }

    pub fn fill(&self, revision: &Revision, out: &mut Vec<u64>) {
        out.push(revision.0);
    }
}

impl fmt::Debug for Read {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Read")
    }
}
