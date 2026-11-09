//! Fixture: a second caller of `Journal::append` through a generic parameter's path.

pub struct UnitOfWork;

pub trait Journal {
    fn append(&self, unit: UnitOfWork) -> bool;
}

pub fn shortcut<T: Journal>(journal: &T) -> bool {
    T::append(journal, UnitOfWork)
}
