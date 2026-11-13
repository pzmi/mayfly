//! Fixture: the secret wrapper and an alias of a type that holds it, re-exported from a
//! module.

pub struct Secret<T>(T);

mod jit {
    pub type JitConfig = Option<crate::Secret<String>>;
}

pub use jit::JitConfig;
