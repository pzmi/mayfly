//! The `VmProvider` contract's own tests (ver §5): the in-memory fake passes every case, and each
//! deliberately broken provider of ver §5 fails the cases that exist to catch it.

use mayfly_data::{VmError, VmProvider};
use mayfly_data_domain::Inventory;
use mayfly_domain::{Millis, OwnedVmName, ProviderInstanceId};
use mayfly_protocol::{BaseName, OwnershipProof, RunOptions};
use mayfly_testkit_data::FakeVmProvider;
use mayfly_testkit_data::provider_contract::{FakeWorld, ProviderCase, check_vm_provider_contract};

/// Deletes by prefix: every VM of the target's pool and host, and anything whose name starts
/// with the target's.
struct PrefixDeleteProvider(FakeVmProvider);

/// Reports a timeout as a definite failure.
struct AmbiguousAsDefiniteProvider(FakeVmProvider);

/// Stops and deletes by name without re-reading the instance id.
struct MacBlindProvider(FakeVmProvider);

/// Reads a failed listing as an empty one.
struct EmptyInventoryProvider(FakeVmProvider);

impl VmProvider for PrefixDeleteProvider {
    async fn inventory(&self) -> Inventory {
        self.0.inventory().await
    }

    async fn clone_vm(
        &self,
        base: &BaseName,
        name: &OwnedVmName,
    ) -> Result<ProviderInstanceId, VmError> {
        self.0.clone_vm(base, name).await
    }

    async fn start(&self, name: &OwnedVmName, options: &RunOptions) -> Result<(), VmError> {
        self.0.start(name, options).await
    }

    async fn stop(&self, proof: &OwnershipProof, grace: Millis) -> Result<(), VmError> {
        self.0.stop(proof, grace).await
    }

    async fn delete(&self, proof: &OwnershipProof) -> Result<(), VmError> {
        let name = proof.vm_name.to_string();
        let prefix = format!(
            "mf-{}-{}-",
            proof.vm_name.pool().as_str(),
            proof.host_id.as_str()
        );
        for listed in self.0.vms().keys() {
            if listed.starts_with(&prefix) || listed.starts_with(&name) {
                self.0.remove(listed);
            }
        }
        Ok(())
    }
}

fn definite(result: Result<(), VmError>) -> Result<(), VmError> {
    result.map_err(|_| VmError::Definite)
}

impl VmProvider for AmbiguousAsDefiniteProvider {
    async fn inventory(&self) -> Inventory {
        self.0.inventory().await
    }

    async fn clone_vm(
        &self,
        base: &BaseName,
        name: &OwnedVmName,
    ) -> Result<ProviderInstanceId, VmError> {
        self.0
            .clone_vm(base, name)
            .await
            .map_err(|_| VmError::Definite)
    }

    async fn start(&self, name: &OwnedVmName, options: &RunOptions) -> Result<(), VmError> {
        definite(self.0.start(name, options).await)
    }

    async fn stop(&self, proof: &OwnershipProof, grace: Millis) -> Result<(), VmError> {
        definite(self.0.stop(proof, grace).await)
    }

    async fn delete(&self, proof: &OwnershipProof) -> Result<(), VmError> {
        definite(self.0.delete(proof).await)
    }
}

impl VmProvider for MacBlindProvider {
    async fn inventory(&self) -> Inventory {
        self.0.inventory().await
    }

    async fn clone_vm(
        &self,
        base: &BaseName,
        name: &OwnedVmName,
    ) -> Result<ProviderInstanceId, VmError> {
        self.0.clone_vm(base, name).await
    }

    async fn start(&self, name: &OwnedVmName, options: &RunOptions) -> Result<(), VmError> {
        self.0.start(name, options).await
    }

    async fn stop(&self, proof: &OwnershipProof, _grace: Millis) -> Result<(), VmError> {
        self.0.set_running(&proof.vm_name.to_string(), false);
        Ok(())
    }

    async fn delete(&self, proof: &OwnershipProof) -> Result<(), VmError> {
        self.0.remove(&proof.vm_name.to_string());
        Ok(())
    }
}

impl VmProvider for EmptyInventoryProvider {
    async fn inventory(&self) -> Inventory {
        match self.0.inventory().await {
            Inventory::Incomplete { .. } => Inventory::Complete { vms: Vec::new() },
            complete @ Inventory::Complete { .. } => complete,
        }
    }

    async fn clone_vm(
        &self,
        base: &BaseName,
        name: &OwnedVmName,
    ) -> Result<ProviderInstanceId, VmError> {
        self.0.clone_vm(base, name).await
    }

    async fn start(&self, name: &OwnedVmName, options: &RunOptions) -> Result<(), VmError> {
        self.0.start(name, options).await
    }

    async fn stop(&self, proof: &OwnershipProof, grace: Millis) -> Result<(), VmError> {
        self.0.stop(proof, grace).await
    }

    async fn delete(&self, proof: &OwnershipProof) -> Result<(), VmError> {
        self.0.delete(proof).await
    }
}

#[test]
fn should_pass_every_case_when_the_provider_is_the_in_memory_fake() {
    // given
    let make = FakeWorld::new;

    // when
    let report = check_vm_provider_contract(&make);

    // then
    assert!(report.is_green(), "{report}");
}

#[test]
fn should_fail_the_sibling_case_when_a_provider_deletes_by_prefix() {
    // given
    let make = || FakeWorld::wrapped(PrefixDeleteProvider);

    // when
    let report = check_vm_provider_contract(&make);

    // then
    assert!(
        report
            .failed_cases()
            .contains(&ProviderCase::DeleteTouchesOnlyTarget),
        "{report}"
    );
}

#[test]
fn should_fail_the_hang_case_when_a_provider_reports_a_timeout_as_definite() {
    // given
    let make = || FakeWorld::wrapped(AmbiguousAsDefiniteProvider);

    // when
    let report = check_vm_provider_contract(&make);

    // then
    assert!(
        report
            .failed_cases()
            .contains(&ProviderCase::HangIsAmbiguous),
        "{report}"
    );
}

#[test]
fn should_fail_the_mismatch_cases_when_a_provider_skips_the_instance_id() {
    // given
    let make = || FakeWorld::wrapped(MacBlindProvider);

    // when
    let report = check_vm_provider_contract(&make);

    // then
    let failed = report.failed_cases();
    assert!(
        failed.contains(&ProviderCase::StopRefusesMismatch)
            && failed.contains(&ProviderCase::DeleteRefusesMismatch),
        "{report}"
    );
}

#[test]
fn should_fail_the_listing_cases_when_a_provider_reads_a_failed_listing_as_empty() {
    // given
    let make = || FakeWorld::wrapped(EmptyInventoryProvider);

    // when
    let report = check_vm_provider_contract(&make);

    // then
    let failed = report.failed_cases();
    assert!(
        failed.contains(&ProviderCase::ListingFailureIncomplete)
            && failed.contains(&ProviderCase::HangIsAmbiguous),
        "{report}"
    );
}
