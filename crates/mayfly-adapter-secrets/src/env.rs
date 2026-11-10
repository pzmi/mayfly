//! `env:<NAME>`: a variable of the daemon's environment, captured once at start (ADR 0033).
//!
//! The capture is the source: a rotated variable needs a restart, which is what "read once at
//! start" means. The variable stays in the daemon's own environment, so what keeps it from a
//! child is that every child gets a cleared environment (ADR 0058, `ARCH-child-environment`),
//! and [`crate::UriSecretProvider::new`] strips the captured names from the few the runner passes.

use std::collections::BTreeMap;
use std::os::unix::ffi::OsStringExt;

use mayfly_control::secrets::{Refusal, SecretError, SecretProvider, secret_from_bytes};
use mayfly_domain::{Secret, SecretRef};

/// The captured variables an `env:` reference may name.
#[derive(Debug, Default)]
pub struct EnvProvider {
    values: BTreeMap<String, Secret<Vec<u8>>>,
}

impl EnvProvider {
    /// Captures `names` from the daemon's environment; an absent one resolves `NotFound`.
    #[must_use]
    pub fn capture<'a>(names: impl IntoIterator<Item = &'a str>) -> Self {
        let values = names
            .into_iter()
            .filter_map(|name| {
                std::env::var_os(name).map(|value| (name.to_owned(), Secret::new(value.into_vec())))
            })
            .collect();
        Self { values }
    }

    /// A provider over given values, as if captured (for composition tests and the contract).
    #[must_use]
    pub fn from_values(values: impl IntoIterator<Item = (String, Vec<u8>)>) -> Self {
        Self {
            values: values
                .into_iter()
                .map(|(name, value)| (name, Secret::new(value)))
                .collect(),
        }
    }
}

impl EnvProvider {
    /// The names captured.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.values.keys().map(String::as_str)
    }
}

impl SecretProvider for EnvProvider {
    async fn resolve(&self, reference: &SecretRef) -> Result<Secret<String>, SecretError> {
        let SecretRef::Env { name } = reference else {
            return Err(SecretError::new(reference, Refusal::Unsupported));
        };
        match self.values.get(name) {
            Some(value) => secret_from_bytes(reference, value.expose().clone()),
            None => Err(SecretError::new(reference, Refusal::NotFound)),
        }
    }
}
