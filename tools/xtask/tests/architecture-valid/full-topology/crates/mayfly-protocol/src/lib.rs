//! Positive fixture: the CP<->DP messages, pure, over the shared kernel.

pub use mayfly_domain::Revision;

pub struct Directive {
    pub revision: Revision,
}
