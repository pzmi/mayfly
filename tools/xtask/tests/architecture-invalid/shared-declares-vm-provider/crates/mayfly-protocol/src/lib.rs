//! Fixture: a shared crate declares the data plane's provider port (second checkpoint
//! review M3).

pub trait VmProvider {
    fn start(&self, name: &str);
}
