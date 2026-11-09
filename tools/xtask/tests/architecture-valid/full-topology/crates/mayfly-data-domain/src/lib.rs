//! Positive fixture: the data-plane domain is pure and names only the shared kernel.

use mayfly_domain::Revision;

pub fn fenced(current: &Revision, directive: &Revision) -> bool {
    directive.0 >= current.0
}
