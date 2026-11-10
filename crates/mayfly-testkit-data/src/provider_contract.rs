//! The `VmProvider` contract suite (ver §5, plan §8.1): one set of cases every VM provider
//! passes, written once over a [`ProviderWorld`] and run from each implementation's crate: the
//! in-memory [`FakeVmProvider`] here ([`FakeWorld`]), the Tart adapter over `mayfly-fake-tart`
//! (`mayfly-adapter-tart/tests/contract.rs`, DP-04), and the Tart adapter over real Tart in the
//! hardware lane (HW-01).
//!
//! Cases are named `VP-01` … ([`ProviderCase`]). A world is a provider plus what a test needs
//! around it: seeding a base or a foreign VM, reading what exists independently of the provider
//! (the oracle the provider's answers are checked against), injecting a hang or a failed
//! listing into the next call, rebuilding the provider over the same VMs (a daemon restart), and
//! reporting the signals any VM's run process received and the environment every child got.
//!
//! **No case relies on time.** A hang is ended by the provider's own deadline, which the world
//! configures; the suite only bounds a whole case ([`CASE_BOUND`], through
//! `mayfly_testkit_shared::poll::within`) so a provider whose future never completes fails the
//! case instead of hanging the run.
//!
//! [`check_vm_provider_contract`] returns a report of failed cases instead of panicking: the
//! suite's own tests use it to show the broken providers of ver §5 failing named cases
//! (`PrefixDeleteProvider`, `AmbiguousAsDefiniteProvider`, `MacBlindProvider`,
//! `EmptyInventoryProvider`; `tests/provider_contract.rs`). A world that cannot build what a
//! case needs fails that case: no case is skipped.

use std::cell::Cell;
use std::collections::BTreeSet;
use std::fmt::{self, Debug, Display};
use std::future::Future;
use std::time::Duration;

use mayfly_data::{Ambiguity, VmError, VmProvider};
use mayfly_data_domain::Inventory;
use mayfly_domain::{
    AttemptId, Generation, HostId, Millis, OwnedVmName, PoolName, ProviderInstanceId, Resources,
    Sample, Slot,
};
use mayfly_protocol::{BaseName, GuestOs, InventoryGap, OwnershipProof, RunOptions, VmObservation};
use mayfly_testkit_shared::child_env::inherited_variable;
use mayfly_testkit_shared::poll::within;

use crate::provider::{FakeVm, FakeVmProvider, Fault, Op};

/// The longest a whole case may take before it is reported as hung.
pub const CASE_BOUND: Duration = Duration::from_secs(120);

/// The names a child may receive (ADR 0058 rule 2, plan §8.2): the allowlist, `TART_HOME` and
/// `TART_NO_AUTO_PRUNE`, and `LLVM_PROFILE_FILE`, which only a coverage build passes.
pub const CHILD_ALLOWLIST: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "LANG",
    "TMPDIR",
    "LLVM_PROFILE_FILE",
    "TART_HOME",
    "TART_NO_AUTO_PRUNE",
];

/// The memory every case starts a VM with.
pub const START_MEMORY: u64 = 2 * 1024 * 1024 * 1024;

/// How the next listing fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListingFault {
    /// The listing call fails.
    Failed,
    /// It succeeds with output that does not parse.
    Garbled,
}

/// A provider and what a case needs around it.
pub trait ProviderWorld {
    /// The provider under test.
    type Provider: VmProvider;

    /// The provider as it is now.
    fn provider(&self) -> &Self::Provider;

    /// Replaces the provider with a new one over the same VMs and nothing remembered, as a
    /// restarted daemon builds it.
    fn restart(&mut self);

    /// Makes `base` a local image base, listed as a stopped VM.
    fn seed_base(&self, base: &BaseName);

    /// Makes a stopped VM named `name` that the provider did not create.
    fn seed_foreign(&self, name: &str);

    /// Whether a VM named `name` exists, and if so whether it runs, read independently of the
    /// provider: the oracle.
    fn exists(&self, name: &str) -> Option<bool>;

    /// The next call of `op` hangs until the provider's own deadline ends it.
    fn hang_next(&self, op: Op);

    /// The next listing fails as `fault` says.
    fn fail_next_listing(&self, fault: ListingFault);

    /// Every signal a VM's run process received, as names (`SIGTERM` …).
    fn signals_to_runs(&self) -> Vec<String>;

    /// The variable names each child process the provider ran received; empty when it runs
    /// none. Names a child's own runtime sets in itself (a coverage build's
    /// `__LLVM_PROFILE_RT_INIT_ONCE`) are not received, and are left out.
    fn child_environments(&self) -> Vec<BTreeSet<String>>;
}

/// One case of the contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ProviderCase {
    /// VP-01: a clone gets a fresh instance id (a random MAC, ADR 0026), different for each
    /// clone and from its base's, and the listing shows it stopped with that id, its guest and
    /// its disk described (ADR 0052).
    CloneGivesFreshIdentity,
    /// VP-02: a clone onto a name that exists is refused `Definite` and leaves that VM as it was.
    CloneOntoExistingRefused,
    /// VP-03: a clone from a base that is not a local base is refused `Definite` and creates
    /// nothing (ADR 0039).
    CloneNeedsLocalBase,
    /// VP-04: a start runs the VM; the listing shows it running with its memory described.
    StartRuns,
    /// VP-05: a start of a running VM is refused `Definite` and the VM keeps running.
    StartOfRunningRefused,
    /// VP-06: a stop with the proof stops the VM.
    StopByProof,
    /// VP-07: a stop whose proof names another instance id is refused and the VM keeps running.
    StopRefusesMismatch,
    /// VP-08: a delete with the proof removes the VM; a delete of a VM that is gone is done
    /// (ensure absent, ADR 0053).
    DeleteEnsuresAbsent,
    /// VP-09: a delete whose proof names another instance id is refused and the VM stays.
    DeleteRefusesMismatch,
    /// VP-10: a delete removes exactly its target: a sibling of the same pool and host, a VM
    /// whose name extends the target's, a foreign VM and the base stay.
    DeleteTouchesOnlyTarget,
    /// VP-11: a hung clone, start, stop or delete ends at the provider's deadline as
    /// `Ambiguous(Deadline)`, never `Definite` or success, and a listing taken next re-inspects
    /// the target exactly as it is; a hung listing is `Incomplete(Deadline)`. A start that passed
    /// its deadline sent no signal to its run process (ADR 0037: only the provider's stop ends
    /// it).
    HangIsAmbiguous,
    /// VP-12: a listing that fails or does not parse is `Incomplete`, never a shorter list (I25).
    ListingFailureIncomplete,
    /// VP-13: a complete listing shows exactly what exists, foreign VMs and bases included, each
    /// running or not as it is.
    InventoryExact,
    /// VP-14: after a restart a new provider lists a VM the old one started as running with its
    /// instance id, and stops it with its proof (ADR 0037's re-adoption, at the port).
    RestartReadopts,
    /// VP-15: no signal ever reaches a VM's run process: stops go through the provider's own
    /// stop, across a restart too (ADR 0037); VP-11 checks the same after a start's deadline.
    NoSignalToRun,
    /// VP-16: every child gets a cleared environment plus the allowlist (ADR 0058): a variable
    /// of the test's own environment reaches none, and no child gets a name outside
    /// [`CHILD_ALLOWLIST`].
    ChildEnvironmentCleared,
}

impl ProviderCase {
    /// Every case, in id order.
    pub const ALL: [Self; 16] = [
        Self::CloneGivesFreshIdentity,
        Self::CloneOntoExistingRefused,
        Self::CloneNeedsLocalBase,
        Self::StartRuns,
        Self::StartOfRunningRefused,
        Self::StopByProof,
        Self::StopRefusesMismatch,
        Self::DeleteEnsuresAbsent,
        Self::DeleteRefusesMismatch,
        Self::DeleteTouchesOnlyTarget,
        Self::HangIsAmbiguous,
        Self::ListingFailureIncomplete,
        Self::InventoryExact,
        Self::RestartReadopts,
        Self::NoSignalToRun,
        Self::ChildEnvironmentCleared,
    ];

    /// The case's id, `VP-01` …
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::CloneGivesFreshIdentity => "VP-01",
            Self::CloneOntoExistingRefused => "VP-02",
            Self::CloneNeedsLocalBase => "VP-03",
            Self::StartRuns => "VP-04",
            Self::StartOfRunningRefused => "VP-05",
            Self::StopByProof => "VP-06",
            Self::StopRefusesMismatch => "VP-07",
            Self::DeleteEnsuresAbsent => "VP-08",
            Self::DeleteRefusesMismatch => "VP-09",
            Self::DeleteTouchesOnlyTarget => "VP-10",
            Self::HangIsAmbiguous => "VP-11",
            Self::ListingFailureIncomplete => "VP-12",
            Self::InventoryExact => "VP-13",
            Self::RestartReadopts => "VP-14",
            Self::NoSignalToRun => "VP-15",
            Self::ChildEnvironmentCleared => "VP-16",
        }
    }
}

impl Display for ProviderCase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {self:?}", self.id())
    }
}

/// A failed case and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaseFailure {
    /// The case.
    pub case: ProviderCase,
    /// What was expected and what the provider did, at the first check that failed.
    pub detail: String,
}

/// The failed cases of one run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProviderContractReport {
    /// Each failed case, in id order.
    pub failures: Vec<CaseFailure>,
}

impl ProviderContractReport {
    /// Whether every case passed.
    #[must_use]
    pub fn is_green(&self) -> bool {
        self.failures.is_empty()
    }

    /// The failed cases.
    #[must_use]
    pub fn failed_cases(&self) -> BTreeSet<ProviderCase> {
        self.failures.iter().map(|f| f.case).collect()
    }
}

impl Display for ProviderContractReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "VmProvider contract: {} case(s) failed",
            self.failures.len()
        )?;
        for failure in &self.failures {
            writeln!(f, "- {}: {}", failure.case, failure.detail)?;
        }
        Ok(())
    }
}

/// Runs every case, each in a fresh world from `make` on its own runtime.
pub fn check_vm_provider_contract<W: ProviderWorld>(
    make: &dyn Fn() -> W,
) -> ProviderContractReport {
    let mut report = ProviderContractReport::default();
    for case in ProviderCase::ALL {
        if let Err(detail) = check_case(make, case) {
            report.failures.push(CaseFailure { case, detail });
        }
    }
    report
}

/// Runs every case and panics with the report unless all pass: the form an implementation's
/// own test calls.
///
/// # Panics
/// When a case fails; the message lists each failed case and why.
pub fn run_vm_provider_contract<W: ProviderWorld>(make: &dyn Fn() -> W) {
    let report = check_vm_provider_contract(make);
    assert!(report.is_green(), "{report}");
}

/// Runs one case in a fresh world on a fresh current-thread runtime with a real clock.
///
/// # Errors
/// What was expected and what the provider did, at the first check that failed.
pub fn check_case<W: ProviderWorld>(make: &dyn Fn() -> W, case: ProviderCase) -> Check {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("the case's runtime did not build: {e}"))?;
    let mut world = make();
    runtime.block_on(async {
        // Boxed: a case's future nests every provider call it awaits, which can outgrow a test
        // thread's stack in a debug build.
        within(
            CASE_BOUND,
            "the case: a provider future never completed",
            Box::pin(run_case(&mut world, case)),
        )
        .await
        .unwrap_or_else(|timed_out| Err(timed_out.to_string()))
    })
}

async fn run_case<W: ProviderWorld>(w: &mut W, case: ProviderCase) -> Check {
    match case {
        ProviderCase::CloneGivesFreshIdentity => clone_gives_fresh_identity(w).await,
        ProviderCase::CloneOntoExistingRefused => clone_onto_existing_refused(w).await,
        ProviderCase::CloneNeedsLocalBase => clone_needs_local_base(w).await,
        ProviderCase::StartRuns => start_runs(w).await,
        ProviderCase::StartOfRunningRefused => start_of_running_refused(w).await,
        ProviderCase::StopByProof => stop_by_proof(w).await,
        ProviderCase::StopRefusesMismatch => stop_refuses_mismatch(w).await,
        ProviderCase::DeleteEnsuresAbsent => delete_ensures_absent(w).await,
        ProviderCase::DeleteRefusesMismatch => delete_refuses_mismatch(w).await,
        ProviderCase::DeleteTouchesOnlyTarget => delete_touches_only_target(w).await,
        ProviderCase::HangIsAmbiguous => hang_is_ambiguous(w).await,
        ProviderCase::ListingFailureIncomplete => listing_failure_incomplete(w).await,
        ProviderCase::InventoryExact => inventory_exact(w).await,
        ProviderCase::RestartReadopts => restart_readopts(w).await,
        ProviderCase::NoSignalToRun => no_signal_to_run(w).await,
        ProviderCase::ChildEnvironmentCleared => child_environment_cleared(w).await,
    }
}

// ---------------------------------------------------------------------------------------------
// Names, proofs and checks

/// A case's outcome.
pub type Check = Result<(), String>;

/// The VM of attempt `n`, in slot `n`, generation 1, on the sample pool and host.
#[must_use]
pub fn vm(n: u32) -> OwnedVmName {
    OwnedVmName::new(
        PoolName::sample(),
        HostId::sample(),
        Slot::new(n),
        Generation::FIRST,
        AttemptId::from_u128(u128::from(n)),
    )
}

/// The proof of [`vm`]`(n)` with `instance_id`.
#[must_use]
pub fn proof(n: u32, instance_id: &ProviderInstanceId) -> OwnershipProof {
    let name = vm(n);
    OwnershipProof {
        attempt_id: AttemptId::from_u128(u128::from(n)),
        host_id: HostId::sample(),
        slot: name.slot(),
        generation: Generation::FIRST,
        vm_name: name,
        instance_id: instance_id.clone(),
    }
}

/// The base every case clones from.
///
/// # Panics
/// Never: the name is valid.
#[must_use]
pub fn base() -> BaseName {
    BaseName::new("mfb-contract").expect("a valid base name")
}

fn options() -> RunOptions {
    RunOptions {
        resources: Resources::new(START_MEMORY, 0),
    }
}

/// An instance id no provider hands out in a case.
fn stranger() -> ProviderInstanceId {
    ProviderInstanceId::new("02:00:5e:00:00:99").expect("a valid instance id")
}

const GRACE: Millis = Millis::new(5_000);

fn eq<T: PartialEq + Debug>(what: &str, got: &T, want: &T) -> Check {
    if got == want {
        Ok(())
    } else {
        Err(format!("{what}: got {got:?}, want {want:?}"))
    }
}

fn ensure(what: &str, holds: bool) -> Check {
    if holds { Ok(()) } else { Err(what.to_owned()) }
}

async fn step<T: Debug, F: Future<Output = Result<T, VmError>>>(
    what: &str,
    call: F,
) -> Result<T, String> {
    call.await
        .map_err(|err| format!("{what}: got {err:?}, want success"))
}

async fn listing<W: ProviderWorld>(w: &W) -> Result<Vec<VmObservation>, String> {
    match w.provider().inventory().await {
        Inventory::Complete { vms } => Ok(vms),
        Inventory::Incomplete { reason } => Err(format!(
            "listing: got Incomplete({reason:?}), want a complete listing"
        )),
    }
}

fn listed<'a>(vms: &'a [VmObservation], name: &str) -> Option<&'a VmObservation> {
    vms.iter().find(|vm| vm.name == name)
}

async fn started<W: ProviderWorld>(w: &W, n: u32) -> Result<ProviderInstanceId, String> {
    let id = step("clone", w.provider().clone_vm(&base(), &vm(n))).await?;
    step("start", w.provider().start(&vm(n), &options())).await?;
    Ok(id)
}

/// A listing taken now agrees with the oracle about `name`.
async fn reinspect<W: ProviderWorld>(w: &W, name: &str) -> Check {
    let vms = listing(w).await?;
    eq(
        &format!("re-inspected {name} (listed, running)"),
        &listed(&vms, name).map(|vm| vm.running),
        &w.exists(name),
    )
}

// ---------------------------------------------------------------------------------------------
// Cases

async fn clone_gives_fresh_identity<W: ProviderWorld>(w: &mut W) -> Check {
    w.seed_base(&base());
    let a = step("first clone", w.provider().clone_vm(&base(), &vm(1))).await?;
    let b = step("second clone", w.provider().clone_vm(&base(), &vm(2))).await?;
    ensure("two clones got the same instance id", a != b)?;
    let vms = listing(w).await?;
    let base_id = listed(&vms, base().as_str()).and_then(|v| v.instance_id.clone());
    ensure(
        "a clone kept its base's instance id",
        base_id.is_none_or(|id| id != a && id != b),
    )?;
    for (n, id) in [(1, a), (2, b)] {
        let name = vm(n).to_string();
        let seen = listed(&vms, &name).ok_or_else(|| format!("{name} is not listed"))?;
        eq(
            &format!("{name}'s instance id"),
            &seen.instance_id,
            &Some(id),
        )?;
        eq(&format!("{name} running"), &seen.running, &false)?;
        ensure(
            &format!("{name}'s guest is not described"),
            seen.guest.is_some(),
        )?;
        ensure(
            &format!("{name}'s disk is not described"),
            seen.disk_bytes.is_some(),
        )?;
    }
    Ok(())
}

async fn clone_onto_existing_refused<W: ProviderWorld>(w: &mut W) -> Check {
    w.seed_base(&base());
    let a = step("clone", w.provider().clone_vm(&base(), &vm(1))).await?;
    let again = w.provider().clone_vm(&base(), &vm(1)).await;
    eq(
        "clone onto an existing name",
        &again,
        &Err(VmError::Definite),
    )?;
    let vms = listing(w).await?;
    eq(
        "the existing VM's instance id",
        &listed(&vms, &vm(1).to_string()).and_then(|v| v.instance_id.clone()),
        &Some(a),
    )
}

async fn clone_needs_local_base<W: ProviderWorld>(w: &mut W) -> Check {
    let other = BaseName::new("mfb-other").map_err(|_| "a valid base name".to_owned())?;
    w.seed_base(&other);
    let cloned = w.provider().clone_vm(&base(), &vm(1)).await;
    eq(
        "clone from a base that is not local",
        &cloned,
        &Err(VmError::Definite),
    )?;
    eq("the clone exists", &w.exists(&vm(1).to_string()), &None)
}

async fn start_runs<W: ProviderWorld>(w: &mut W) -> Check {
    w.seed_base(&base());
    started(w, 1).await?;
    let name = vm(1).to_string();
    eq(
        "the started VM (exists, running)",
        &w.exists(&name),
        &Some(true),
    )?;
    let vms = listing(w).await?;
    let seen = listed(&vms, &name).ok_or_else(|| format!("{name} is not listed"))?;
    eq("listed running", &seen.running, &true)?;
    eq("listed memory", &seen.memory_bytes, &Some(START_MEMORY))
}

async fn start_of_running_refused<W: ProviderWorld>(w: &mut W) -> Check {
    w.seed_base(&base());
    started(w, 1).await?;
    let again = w.provider().start(&vm(1), &options()).await;
    eq("start of a running VM", &again, &Err(VmError::Definite))?;
    eq(
        "the VM (exists, running)",
        &w.exists(&vm(1).to_string()),
        &Some(true),
    )
}

async fn stop_by_proof<W: ProviderWorld>(w: &mut W) -> Check {
    w.seed_base(&base());
    let id = started(w, 1).await?;
    step("stop", w.provider().stop(&proof(1, &id), GRACE)).await?;
    eq(
        "the stopped VM (exists, running)",
        &w.exists(&vm(1).to_string()),
        &Some(false),
    )?;
    reinspect(w, &vm(1).to_string()).await
}

async fn stop_refuses_mismatch<W: ProviderWorld>(w: &mut W) -> Check {
    w.seed_base(&base());
    started(w, 1).await?;
    let stopped = w.provider().stop(&proof(1, &stranger()), GRACE).await;
    eq(
        "stop with another instance id",
        &stopped,
        &Err(VmError::Definite),
    )?;
    eq(
        "the VM (exists, running)",
        &w.exists(&vm(1).to_string()),
        &Some(true),
    )
}

async fn delete_ensures_absent<W: ProviderWorld>(w: &mut W) -> Check {
    w.seed_base(&base());
    let id = step("clone", w.provider().clone_vm(&base(), &vm(1))).await?;
    step("delete", w.provider().delete(&proof(1, &id))).await?;
    eq(
        "the deleted VM exists",
        &w.exists(&vm(1).to_string()),
        &None,
    )?;
    step(
        "delete of a VM that is gone",
        w.provider().delete(&proof(1, &id)),
    )
    .await?;
    reinspect(w, &vm(1).to_string()).await
}

async fn delete_refuses_mismatch<W: ProviderWorld>(w: &mut W) -> Check {
    w.seed_base(&base());
    step("clone", w.provider().clone_vm(&base(), &vm(1))).await?;
    let deleted = w.provider().delete(&proof(1, &stranger())).await;
    eq(
        "delete with another instance id",
        &deleted,
        &Err(VmError::Definite),
    )?;
    eq(
        "the VM (exists, running)",
        &w.exists(&vm(1).to_string()),
        &Some(false),
    )
}

async fn delete_touches_only_target<W: ProviderWorld>(w: &mut W) -> Check {
    w.seed_base(&base());
    let extended = format!("{}-copy", vm(1));
    w.seed_foreign(&extended);
    w.seed_foreign("foreign-vm");
    let id = step("clone", w.provider().clone_vm(&base(), &vm(1))).await?;
    step("clone of a sibling", w.provider().clone_vm(&base(), &vm(2))).await?;
    step("delete", w.provider().delete(&proof(1, &id))).await?;
    eq("the target exists", &w.exists(&vm(1).to_string()), &None)?;
    let sibling = vm(2).to_string();
    for name in [
        sibling.as_str(),
        extended.as_str(),
        "foreign-vm",
        base().as_str(),
    ] {
        eq(
            &format!("{name} (exists, running)"),
            &w.exists(name),
            &Some(false),
        )?;
    }
    Ok(())
}

fn ambiguous_deadline<T: Debug + PartialEq>(what: &str, got: &Result<T, VmError>) -> Check {
    match got {
        Err(VmError::Ambiguous(Ambiguity::Deadline)) => Ok(()),
        other => Err(format!(
            "{what} that hung: got {other:?}, want Err(Ambiguous(Deadline))"
        )),
    }
}

async fn hang_is_ambiguous<W: ProviderWorld>(w: &mut W) -> Check {
    w.seed_base(&base());

    w.hang_next(Op::Clone);
    let cloned = w.provider().clone_vm(&base(), &vm(1)).await;
    ambiguous_deadline("clone", &cloned)?;
    reinspect(w, &vm(1).to_string()).await?;

    step("clone", w.provider().clone_vm(&base(), &vm(2))).await?;
    w.hang_next(Op::Start);
    let ran = w.provider().start(&vm(2), &options()).await;
    ambiguous_deadline("start", &ran)?;
    reinspect(w, &vm(2).to_string()).await?;
    eq(
        "signals to run processes after a start passed its deadline",
        &w.signals_to_runs(),
        &Vec::new(),
    )?;

    let id = started(w, 3).await?;
    w.hang_next(Op::Stop);
    let stopped = w.provider().stop(&proof(3, &id), Millis::new(0)).await;
    ambiguous_deadline("stop", &stopped)?;
    reinspect(w, &vm(3).to_string()).await?;

    let id = step("clone", w.provider().clone_vm(&base(), &vm(4))).await?;
    w.hang_next(Op::Delete);
    let deleted = w.provider().delete(&proof(4, &id)).await;
    ambiguous_deadline("delete", &deleted)?;
    reinspect(w, &vm(4).to_string()).await?;

    w.hang_next(Op::Inventory);
    let inventory = w.provider().inventory().await;
    eq(
        "a hung listing",
        &inventory,
        &Inventory::Incomplete {
            reason: InventoryGap::Deadline,
        },
    )
}

async fn listing_failure_incomplete<W: ProviderWorld>(w: &mut W) -> Check {
    w.seed_base(&base());
    step("clone", w.provider().clone_vm(&base(), &vm(1))).await?;
    for fault in [ListingFault::Failed, ListingFault::Garbled] {
        w.fail_next_listing(fault);
        let inventory = w.provider().inventory().await;
        ensure(
            &format!("a {fault:?} listing: got {inventory:?}, want Incomplete"),
            matches!(inventory, Inventory::Incomplete { .. }),
        )?;
    }
    reinspect(w, &vm(1).to_string()).await
}

async fn inventory_exact<W: ProviderWorld>(w: &mut W) -> Check {
    w.seed_base(&base());
    w.seed_foreign("foreign-vm");
    step("clone", w.provider().clone_vm(&base(), &vm(2))).await?;
    started(w, 1).await?;
    let vms = listing(w).await?;
    let names: BTreeSet<String> = vms.iter().map(|vm| vm.name.clone()).collect();
    let want: BTreeSet<String> = [
        base().to_string(),
        "foreign-vm".to_owned(),
        vm(1).to_string(),
        vm(2).to_string(),
    ]
    .into_iter()
    .collect();
    eq("listed names", &names, &want)?;
    for seen in &vms {
        eq(
            &format!("{} (exists, running)", seen.name),
            &w.exists(&seen.name),
            &Some(seen.running),
        )?;
    }
    Ok(())
}

async fn restart_readopts<W: ProviderWorld>(w: &mut W) -> Check {
    w.seed_base(&base());
    let id = started(w, 1).await?;
    w.restart();
    let vms = listing(w).await?;
    let name = vm(1).to_string();
    let seen =
        listed(&vms, &name).ok_or_else(|| format!("{name} is not listed after a restart"))?;
    eq("running after a restart", &seen.running, &true)?;
    eq(
        "instance id after a restart",
        &seen.instance_id,
        &Some(id.clone()),
    )?;
    step(
        "stop after a restart",
        w.provider().stop(&proof(1, &id), GRACE),
    )
    .await?;
    eq("the VM (exists, running)", &w.exists(&name), &Some(false))
}

async fn no_signal_to_run<W: ProviderWorld>(w: &mut W) -> Check {
    w.seed_base(&base());
    let first = started(w, 1).await?;
    step("stop", w.provider().stop(&proof(1, &first), GRACE)).await?;
    let second = started(w, 2).await?;
    w.restart();
    step(
        "stop after a restart",
        w.provider().stop(&proof(2, &second), GRACE),
    )
    .await?;
    eq(
        "signals to run processes",
        &w.signals_to_runs(),
        &Vec::new(),
    )
}

async fn child_environment_cleared<W: ProviderWorld>(w: &mut W) -> Check {
    let inherited = inherited_variable()
        .ok_or("the test's environment has no variable to probe the children with")?;
    w.seed_base(&base());
    let id = started(w, 1).await?;
    listing(w).await?;
    step("stop", w.provider().stop(&proof(1, &id), GRACE)).await?;
    for names in w.child_environments() {
        ensure(
            &format!("a child received {inherited} from the test's environment: {names:?}"),
            !names.contains(&inherited),
        )?;
        let outside: Vec<&String> = names
            .iter()
            .filter(|name| !CHILD_ALLOWLIST.contains(&name.as_str()))
            .collect();
        ensure(
            &format!("a child received names outside the allowlist: {outside:?}"),
            outside.is_empty(),
        )?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// The in-memory world

/// The [`FakeVmProvider`]'s world, optionally behind a wrapper (the suite's own tests wrap it in
/// deliberately broken providers). A hang is the fake's ambiguous fault with nothing applied: the
/// fake's stand-in for a call its own deadline ended. It runs no child, so it reports no signals
/// and no environments.
pub struct FakeWorld<P = FakeVmProvider> {
    fake: FakeVmProvider,
    provider: P,
    wrap: fn(FakeVmProvider) -> P,
    minted: Cell<u8>,
}

impl FakeWorld {
    /// The plain fake.
    #[must_use]
    pub fn new() -> Self {
        Self::wrapped(|fake| fake)
    }
}

impl Default for FakeWorld {
    fn default() -> Self {
        Self::new()
    }
}

impl<P> FakeWorld<P> {
    /// The fake behind `wrap`.
    #[must_use]
    pub fn wrapped(wrap: fn(FakeVmProvider) -> P) -> Self {
        let fake = FakeVmProvider::new(Some(GuestOs::Macos), Some(50 * 1024 * 1024 * 1024));
        Self {
            provider: wrap(fake.clone()),
            fake,
            wrap,
            minted: Cell::new(0),
        }
    }

    /// The fake underneath.
    #[must_use]
    pub const fn fake(&self) -> &FakeVmProvider {
        &self.fake
    }

    fn mint(&self) -> ProviderInstanceId {
        let n = self.minted.get().wrapping_add(1);
        self.minted.set(n);
        ProviderInstanceId::new(&format!("0e:00:00:00:00:{n:02x}")).expect("a valid instance id")
    }
}

impl<P: VmProvider> ProviderWorld for FakeWorld<P> {
    type Provider = P;

    fn provider(&self) -> &P {
        &self.provider
    }

    fn restart(&mut self) {
        self.provider = (self.wrap)(self.fake.clone());
    }

    fn seed_base(&self, base: &BaseName) {
        self.fake.allow_base(base, self.mint());
    }

    fn seed_foreign(&self, name: &str) {
        let vm = FakeVm {
            instance_id: Some(self.mint()),
            running: false,
            guest: Some(GuestOs::Macos),
            memory_bytes: None,
            disk_bytes: None,
        };
        self.fake.seed(name, vm);
    }

    fn exists(&self, name: &str) -> Option<bool> {
        self.fake.vms().get(name).map(|vm| vm.running)
    }

    fn hang_next(&self, op: Op) {
        let fault = match op {
            Op::Inventory => Fault::Incomplete(InventoryGap::Deadline),
            Op::Clone | Op::Start | Op::Stop | Op::Delete => Fault::Ambiguous {
                cause: Ambiguity::Deadline,
                applied: false,
            },
        };
        self.fake.fail_next(op, fault);
    }

    fn fail_next_listing(&self, fault: ListingFault) {
        let gap = match fault {
            ListingFault::Failed => InventoryGap::ListFailed,
            ListingFault::Garbled => InventoryGap::Unparseable,
        };
        self.fake.fail_next(Op::Inventory, Fault::Incomplete(gap));
    }

    fn signals_to_runs(&self) -> Vec<String> {
        Vec::new()
    }

    fn child_environments(&self) -> Vec<BTreeSet<String>> {
        Vec::new()
    }
}
