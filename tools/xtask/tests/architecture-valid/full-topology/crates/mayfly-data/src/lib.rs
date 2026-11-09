//! Positive fixture: the data plane executes protocol directives through its domain.

use mayfly_protocol::Directive;

pub fn accept(current: &mayfly_domain::Revision, directive: &Directive) -> bool {
    mayfly_data_domain::fenced(current, &directive.revision)
}

/// The data plane's own vocabulary is its to use (`ARCH-plane-vocabulary`).
pub trait VmProvider {
    fn clone_vm(&self, directive: &Directive) -> bool;
}
