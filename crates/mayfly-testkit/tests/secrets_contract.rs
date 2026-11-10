//! `MemorySecretProvider` passes the generic `SecretProvider` contract (ver §5); the adapters
//! and the deliberately broken providers run it from `mayfly-adapter-secrets/tests/contract.rs`.

use mayfly_domain::SecretRef;
use mayfly_testkit::block_on;
use mayfly_testkit::contracts::secrets::{SecretSourceFactory, check_secret_contract};
use mayfly_testkit::secrets::MemorySecretProvider;

#[derive(Default)]
struct MemoryFactory {
    provider: MemorySecretProvider,
    next: u32,
}

impl MemoryFactory {
    fn fresh_reference(&mut self) -> SecretRef {
        self.next += 1;
        SecretRef::Env {
            name: format!("MEMORY_{}", self.next),
        }
    }
}

impl SecretSourceFactory for MemoryFactory {
    type Provider = MemorySecretProvider;

    fn provider(&self) -> &MemorySecretProvider {
        &self.provider
    }

    fn install(&mut self, value: &str) -> SecretRef {
        let reference = self.fresh_reference();
        self.provider.set(&reference, value.as_bytes());
        reference
    }

    fn rotate(&mut self, reference: &SecretRef, value: &str) {
        self.provider.set(reference, value.as_bytes());
    }

    fn absent(&mut self) -> SecretRef {
        self.fresh_reference()
    }
}

#[test]
fn should_pass_secret_contract_when_provider_is_in_memory() {
    // given
    let mut factory = MemoryFactory::default();

    // when
    let report = block_on(check_secret_contract(&mut factory));

    // then
    assert!(report.is_green(), "{report}");
}

#[test]
fn should_refuse_as_not_found_when_a_value_is_removed() {
    // given
    let mut factory = MemoryFactory::default();
    let reference = factory.install("synthetic-removed");
    factory.provider.remove(&reference);

    // when
    let got = block_on(mayfly_control::secrets::SecretProvider::resolve(
        &factory.provider,
        &reference,
    ));

    // then
    assert_eq!(
        got.map(drop).map_err(|err| err.refusal),
        Err(mayfly_control::secrets::Refusal::NotFound)
    );
}
