//! Fixture: the data plane declares the control plane's journal port, which only a
//! reference to it was checked for (second checkpoint review M3).

pub trait Journal {
    fn append(&mut self, event: &str);
}
