//! Fixture: a shared crate defines a journal type and re-exports it under a neutral name.

pub use self::inner::Journal as Log;

mod inner {
    pub trait Journal {}
}
