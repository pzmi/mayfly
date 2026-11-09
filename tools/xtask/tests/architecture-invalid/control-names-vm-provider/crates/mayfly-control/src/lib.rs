//! Fixture: the control plane names the data plane's VM provider port; it decides, and only
//! the data plane talks to providers.

pub fn plan<P: mayfly_domain::VmProvider>(_provider: &P) {}
