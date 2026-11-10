//! `MemorySecretProvider`, the in-memory `SecretProvider` (ver §7.1): holds `Secret` values
//! only and follows the port's one byte rule, so it passes the same contract as the adapters
//! (`tests/secrets_contract.rs`).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};

use mayfly_control::secrets::{Refusal, SecretError, SecretProvider, secret_from_bytes};
use mayfly_domain::{Secret, SecretRef};

/// Values by reference text; a clone shares them, so a test rotates a value its provider sees.
#[derive(Debug, Clone, Default)]
pub struct MemorySecretProvider {
    values: Arc<Mutex<BTreeMap<String, Secret<Vec<u8>>>>>,
}

impl MemorySecretProvider {
    /// A provider that resolves nothing.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Puts `value` behind `reference`, replacing what was there (a rotation).
    pub fn set(&self, reference: &SecretRef, value: &[u8]) {
        self.values
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(reference.to_string(), Secret::new(value.to_vec()));
    }

    /// Removes `reference`'s value.
    pub fn remove(&self, reference: &SecretRef) {
        self.values
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&reference.to_string());
    }
}

impl SecretProvider for MemorySecretProvider {
    async fn resolve(&self, reference: &SecretRef) -> Result<Secret<String>, SecretError> {
        let bytes = self
            .values
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&reference.to_string())
            .map(|value| value.expose().clone());
        match bytes {
            Some(bytes) => secret_from_bytes(reference, bytes),
            None => Err(SecretError::new(reference, Refusal::NotFound)),
        }
    }
}
