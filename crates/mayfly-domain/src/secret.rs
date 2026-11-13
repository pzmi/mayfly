//! A value that must never be printed, serialised or persisted (ADR 0014, plan §13).
//!
//! `Secret<T>` formats as `<redacted>` under both `Debug` and `Display`, so a secret inside any
//! type that derives `Debug` (an error, a config, a provider) prints redacted too. It has no
//! `Serialize`, no `Clone` and no `PartialEq` (`ARCH-secret-serialize` keeps every type that
//! holds one from serialising it; `tests/secret.rs` fails to compile if the wrapper ever
//! implements `Serialize`); the only way to the value is [`Secret::expose`], named so a
//! reviewer sees each place a secret leaves its wrapper. Pinned by `tests/secret.rs`.
//!
//! Dropping a `Secret` frees its memory without wiping it: a crash dump or swapped page may
//! still hold the value (a limitation; nothing in mayfly writes one).

use std::fmt;

/// A secret value in memory only.
pub struct Secret<T>(T);

impl<T> Secret<T> {
    /// Wraps `value`.
    #[must_use]
    pub const fn new(value: T) -> Self {
        Self(value)
    }

    /// The value, for the one call that needs it (a request header, a child's stdin).
    #[must_use]
    pub const fn expose(&self) -> &T {
        &self.0
    }
}

impl<T> fmt::Debug for Secret<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

impl<T> fmt::Display for Secret<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}
