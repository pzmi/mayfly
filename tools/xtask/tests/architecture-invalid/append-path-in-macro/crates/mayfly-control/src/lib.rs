//! Fixture: a second caller of `Journal::append` by path, inside a macro's tokens; the
//! collection's `Vec::append(&mut ..)` beside it is fine.

pub struct UnitOfWork;

pub trait Journal {
    fn append(&self, unit: UnitOfWork) -> bool;
}

pub fn shortcut<J: Journal>(journal: &J, mut a: Vec<u8>, mut b: Vec<u8>) {
    assert!(Vec::append(&mut a, &mut b) == ());
    assert!(Journal :: append(journal, UnitOfWork));
}
