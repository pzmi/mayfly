//! Fixture: the control plane owns the journal port.

pub trait Journal {
    fn append(&mut self, event: u64);
}
