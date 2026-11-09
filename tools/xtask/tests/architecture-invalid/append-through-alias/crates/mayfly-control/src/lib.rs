//! Fixture: a second caller of `Journal::append` through a renamed import of the port.

use crate::port::Journal as Log;

pub struct UnitOfWork;

mod port {
    pub trait Journal {
        fn append(&self, unit: super::UnitOfWork) -> bool;
    }
}

pub fn shortcut<J: Log>(journal: &J) -> bool {
    Log::append(journal, UnitOfWork)
}
