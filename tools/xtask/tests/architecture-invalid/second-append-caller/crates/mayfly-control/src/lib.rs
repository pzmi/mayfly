//! A second caller of `Journal::append` beside the store.

pub struct UnitOfWork;

pub trait Journal {
    fn append(&self, unit: UnitOfWork) -> bool;
}

pub struct AggregateStore<J>(pub J);

impl<J: Journal> AggregateStore<J> {
    pub fn save(&self) -> bool {
        self.0.append(UnitOfWork)
    }
}

pub fn shortcut<J: Journal>(journal: &J) -> bool {
    journal.append(UnitOfWork)
}
