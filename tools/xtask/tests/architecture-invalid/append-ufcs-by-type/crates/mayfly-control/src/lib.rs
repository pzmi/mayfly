//! Fixture: a second caller of `Journal::append` through the concrete journal type's path.

pub struct UnitOfWork;

pub trait Journal {
    fn append(&self, unit: UnitOfWork) -> bool;
}

pub struct MemoryJournal;

impl MemoryJournal {
    pub fn append(&self, _unit: UnitOfWork) -> bool {
        true
    }
}

pub fn shortcut(journal: &MemoryJournal) -> bool {
    MemoryJournal::append(journal, UnitOfWork)
}
