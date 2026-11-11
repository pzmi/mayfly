//! The recovery decision table (plan §10, `doc/domain-model.md` §11.3, ADRs 0025, 0026, 0037):
//! what the startup use case does with each VM name after the data plane's inventory, given what
//! the journal records, what the provider shows and what GitHub knows of the runner.
//!
//! [`classify`] is pure and total: plain data in, one [`Decision`] out, every row written out.
//! It decides; the startup use case (CP-04) turns a decision into ordinary commands on the
//! attempt (`Adopt`, `RecordObservation`, `RequestRetirement`, `Quarantine`, …). The model is
//! `formal/recovery.qnt` (`classify`, `proposal`); every input combination and its decision is
//! pinned in `tests/fixtures/matrix/recovery.txt` by
//! `tests/recovery.rs::should_define_every_row_when_each_view_meets_each_other`. The rules no row
//! may break, each with the test that pins it over every combination:
//!
//! - **An incomplete inventory releases nothing** (I25): it is never read as any VM's absence;
//!   the decision is [`Decision::Wait`] (and, in safe mode, [`Proposal::WaitForInventory`])
//!   (`should_release_nothing_when_the_inventory_is_incomplete`).
//! - **An `mf-*` VM the journal does not know is reported, never deleted** (I04)
//!   (`should_never_delete_when_an_mf_vm_is_unknown_to_the_journal`); an image base or a foreign
//!   VM is never touched (I05) (`should_never_touch_when_the_vm_is_a_base_or_foreign`).
//! - **An instance-id mismatch is never deleted** (ADR 0026): a live record is quarantined
//!   `provider_identity_mismatch`, a terminal one reports the stranger
//!   (`should_never_delete_when_the_instance_id_differs`).
//! - **A quarantined VM is kept** (I28, ADR 0025): no row retires, stops or deletes it
//!   (`should_keep_when_the_attempt_is_quarantined`). Its VM's absence on a complete listing is
//!   read by the attempt itself when the inventory is recorded (`VmVanished` for a VM it
//!   recorded, DEC-07), not by recovery.
//! - **A recorded VM's absence is a desync, never a silent loss** (DEC-07): `MarkLost` is taken
//!   only on a complete listing, and the attempt it reaches records `VmVanished`.
//! - **No row asserts an outcome** (I10, ADR 0025): a decision that reads a VM's presence is
//!   taken only on a listing that shows it, one that reads its absence only on a complete listing
//!   without it (`should_assert_no_outcome_when_any_row_is_decided`).
//! - **A running VM whose runner may serve a job is never retired** (F13): a Busy attempt's, or
//!   one that can hold a registration while GitHub shows its runner busy; `unreachable` is
//!   never `absent` (`should_never_retire_when_a_busy_vm_runs`).
//! - **Safe mode decides nothing destructive** (I29): an untrusted journal yields a recovery
//!   plan proposal only, and no proposal retires, retries or deletes
//!   (`should_propose_nothing_destructive_when_the_journal_is_untrusted`).
//!
//! The runner-registration inputs are a plain [`GithubView`] per attempt here; the
//! `RunnerRegistration` aggregate that will feed it arrives with KER-09.

use mayfly_domain::ProviderInstanceId;

use crate::attempt::{DirectiveTag, Phase, QuarantineReason, RetireReason, Stage};

/// Why the journal is not trusted at start-up: the host enters safe mode (`doc/domain-model.md`
/// §5.3, I17, I29).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SafeModeCause {
    /// The journal's highest minted epoch is below the external floor: it was rolled back
    /// (ADR 0026).
    EpochBelowFloor,
    /// The journal holds an event kind this build does not know (a newer build wrote it).
    UnknownEventKind,
    /// The store's structural verification failed.
    VerificationFailed,
    /// `HostRun` records safe mode and no completed plan or verified restart has left it.
    Recorded,
}

/// Whether the startup verification trusts the journal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trust {
    /// The journal verifies and is not below the floor.
    Trusted,
    /// Safe mode, for this cause.
    SafeMode(SafeModeCause),
}

/// A VM name no attempt record names, by its shape (`doc/domain-model.md` §5.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum NameClass {
    /// `mf-*` without ownership: managed-looking.
    ManagedLooking,
    /// `mfb-*` or a name in the data plane's base set: an image base.
    ImageBase,
    /// Anything else.
    Foreign,
}

/// What the journal records of the attempt whose VM name this is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttemptRecord {
    /// The attempt's phase.
    pub phase: Phase,
    /// The instance id `VmCloned` recorded; `None` before.
    pub instance_id: Option<ProviderInstanceId>,
    /// The kind of the pending directive. After a restart its outcome is not known, whether it
    /// was in flight or already `Unknown`: only the inventory resolves it.
    pub pending: Option<DirectiveTag>,
}

/// Whose name it is, by the journal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Subject {
    /// An attempt records this exact `OwnedVmName`.
    Attempt(AttemptRecord),
    /// No attempt records it.
    Unrecorded(NameClass),
}

/// The journal's side of one row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalView {
    /// Whether the journal is trusted.
    pub trust: Trust,
    /// What it records of this name.
    pub subject: Subject,
}

/// Whether the VM runs, and under what (ADR 0037).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmRun {
    /// Running; `shim_alive` is whether its shim holds the VM's lock.
    Running {
        /// The shim's lock is held.
        shim_alive: bool,
    },
    /// Stopped; `exit_json` is whether its shim recorded the exit (else the host rebooted under
    /// it). The table reads both alike.
    Stopped {
        /// The shim wrote `exit.json`.
        exit_json: bool,
    },
}

/// One VM in a complete inventory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedVm {
    /// The provider instance id read back now (ADR 0026).
    pub instance_id: ProviderInstanceId,
    /// Whether it runs.
    pub run: VmRun,
}

/// The data plane's side of one row: the inventory, shim locks and `exit.json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderView {
    /// The listing failed or could not be parsed: it says nothing about any VM (I25).
    Incomplete,
    /// A complete listing without this name.
    Absent,
    /// A complete listing with this name.
    Present(ObservedVm),
}

/// GitHub's side of one row: the attempt's runner registration and what GitHub shows of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum GithubView {
    /// The journal records no registration for the attempt.
    NotRegistered,
    /// A registration was issued and its JIT delivery is not recorded.
    IssuedNotDelivered,
    /// GitHub shows the runner online and idle.
    Online,
    /// GitHub shows the runner running a job.
    Busy,
    /// GitHub shows the runner offline or abandoned.
    Offline,
    /// GitHub answered, and lists no such runner.
    Absent,
    /// GitHub could not answer: nothing is known, and this is never `Absent`.
    Unreachable,
}

/// How an attempt is adopted under the new run epoch (`AttemptAdopted`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Adoption {
    /// Continue where the journal stands.
    Continue,
    /// Continue, and verify the guest again before any activation.
    Reverify,
    /// Keep the runner and monitor it; never delete it on this decision.
    Monitor,
    /// The pending clone took effect: record this instance id (`VmCloned`) and continue.
    Clone(ProviderInstanceId),
}

/// The decision for one VM name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// The inventory is incomplete: decide nothing, release nothing, keep admission closed.
    Wait,
    /// Take responsibility for the attempt under the new epoch.
    Adopt(Adoption),
    /// The pending clone did not happen: retry it by the same name and generation (budgeted).
    RetryClone,
    /// Retire the attempt; cleanup (deregistration first, then stop and delete with proof)
    /// follows the saga.
    Retire(RetireReason),
    /// A JIT may have reached something else: presume it leaked, deregister the runner by its
    /// journaled id, retire.
    PresumeLeaked,
    /// The pending delete took effect: record the VM's absence and release the slot.
    ConfirmDeleted,
    /// The VM is absent and we did not delete it: the use case records the complete listing's
    /// absence on the attempt, which records the VM vanished (`VmVanished`, a desync) and closes
    /// (DEC-07).
    MarkLost,
    /// A leaked VM of a terminal attempt: delete it with the ownership proof.
    DeleteLeaked,
    /// Cleanup would stop a VM whose runner GitHub shows active: hold it for inspection.
    SuspendCleanup,
    /// Ownership or an outcome cannot be proven: quarantine.
    Quarantine(QuarantineReason),
    /// Quarantined: the case continues.
    Keep,
    /// Report the VM for operator review; never touch it.
    Report,
    /// Nothing to do.
    Ignore,
    /// The journal is not trusted: nothing is executed; this is the action the recovery plan
    /// proposes for the name.
    SafeMode {
        /// Why.
        cause: SafeModeCause,
        /// The plan's action.
        proposal: Proposal,
    },
}

/// A recovery plan's proposed action for one name in safe mode (`doc/domain-model.md` §6.9).
/// The proposal is drawn automatically, so it is never destructive: the type has no retire,
/// retry or delete (ADR 0025).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Proposal {
    /// The inventory is incomplete: no plan can be bound to it yet.
    WaitForInventory,
    /// The VM is the one the journal recorded: adopt it.
    Adopt,
    /// A complete listing shows the VM absent: mark the attempt lost (its VM vanished, DEC-07).
    MarkLost,
    /// Ownership cannot be proven: quarantine.
    Quarantine(QuarantineReason),
    /// Already quarantined: keep.
    Keep,
    /// Report for operator review.
    Report,
    /// Nothing to do.
    Ignore,
}

impl Decision {
    /// Whether the decision deletes a VM (only a leaked VM of a terminal attempt, with proof).
    #[must_use]
    pub const fn deletes(&self) -> bool {
        match self {
            Self::DeleteLeaked => true,
            Self::Wait
            | Self::Adopt(_)
            | Self::RetryClone
            | Self::Retire(_)
            | Self::PresumeLeaked
            | Self::ConfirmDeleted
            | Self::MarkLost
            | Self::SuspendCleanup
            | Self::Quarantine(_)
            | Self::Keep
            | Self::Report
            | Self::Ignore
            | Self::SafeMode { .. } => false,
        }
    }

    /// Whether the decision ends the attempt's use of its VM or starts an effect: retire,
    /// presume leaked, retry a clone or delete.
    #[must_use]
    pub const fn is_destructive(&self) -> bool {
        match self {
            Self::RetryClone | Self::Retire(_) | Self::PresumeLeaked | Self::DeleteLeaked => true,
            Self::Wait
            | Self::Adopt(_)
            | Self::ConfirmDeleted
            | Self::MarkLost
            | Self::SuspendCleanup
            | Self::Quarantine(_)
            | Self::Keep
            | Self::Report
            | Self::Ignore
            | Self::SafeMode { .. } => false,
        }
    }

    /// Whether the decision frees the attempt's slot (it reads the VM's absence).
    #[must_use]
    pub const fn releases(&self) -> bool {
        match self {
            Self::ConfirmDeleted | Self::MarkLost => true,
            Self::SafeMode { proposal, .. } => match proposal {
                Proposal::MarkLost => true,
                Proposal::WaitForInventory
                | Proposal::Adopt
                | Proposal::Quarantine(_)
                | Proposal::Keep
                | Proposal::Report
                | Proposal::Ignore => false,
            },
            Self::Wait
            | Self::Adopt(_)
            | Self::RetryClone
            | Self::Retire(_)
            | Self::PresumeLeaked
            | Self::DeleteLeaked
            | Self::SuspendCleanup
            | Self::Quarantine(_)
            | Self::Keep
            | Self::Report
            | Self::Ignore => false,
        }
    }
}

/// The recovery decision for one VM name (plan §10). Total over its inputs; see the module
/// documentation for the rules every row keeps and the tests that pin them.
#[must_use]
pub fn classify(journal: &JournalView, provider: &ProviderView, github: GithubView) -> Decision {
    match journal.trust {
        Trust::SafeMode(cause) => Decision::SafeMode {
            cause,
            proposal: propose(&journal.subject, provider),
        },
        Trust::Trusted => {
            let vm = match provider {
                ProviderView::Incomplete => return Decision::Wait,
                ProviderView::Absent => None,
                ProviderView::Present(vm) => Some(vm),
            };
            match &journal.subject {
                Subject::Unrecorded(class) => unrecorded(*class, vm),
                Subject::Attempt(record) => attempt(record, vm, github),
            }
        }
    }
}

const fn unrecorded(class: NameClass, vm: Option<&ObservedVm>) -> Decision {
    match class {
        NameClass::ManagedLooking => match vm {
            Some(_) => Decision::Report,
            None => Decision::Ignore,
        },
        NameClass::ImageBase | NameClass::Foreign => Decision::Ignore,
    }
}

/// Phases with no live use of a VM: a present VM is a stranger or a leak.
const fn terminal(phase: Phase) -> bool {
    match phase {
        Phase::NotStarted | Phase::Deleted | Phase::Lost | Phase::Closed => true,
        Phase::Reserved
        | Phase::Cloning
        | Phase::Booting
        | Phase::Verifying
        | Phase::Prepared
        | Phase::Activating
        | Phase::Listening
        | Phase::Busy
        | Phase::Finished
        | Phase::Retiring
        | Phase::Deleting
        | Phase::Backoff { .. }
        | Phase::Quarantined { .. } => false,
    }
}

const fn quarantined(phase: Phase) -> bool {
    match phase {
        Phase::Quarantined { .. } => true,
        Phase::NotStarted
        | Phase::Reserved
        | Phase::Cloning
        | Phase::Booting
        | Phase::Verifying
        | Phase::Prepared
        | Phase::Activating
        | Phase::Listening
        | Phase::Busy
        | Phase::Finished
        | Phase::Retiring
        | Phase::Deleting
        | Phase::Deleted
        | Phase::Lost
        | Phase::Backoff { .. }
        | Phase::Closed => false,
    }
}

const fn running(vm: &ObservedVm) -> bool {
    match vm.run {
        VmRun::Running { .. } => true,
        VmRun::Stopped { .. } => false,
    }
}

fn attempt(record: &AttemptRecord, vm: Option<&ObservedVm>, github: GithubView) -> Decision {
    if quarantined(record.phase) {
        return Decision::Keep;
    }
    if let (Some(vm), Some(recorded)) = (vm, &record.instance_id)
        && vm.instance_id != *recorded
    {
        return if terminal(record.phase) {
            Decision::Report
        } else {
            Decision::Quarantine(QuarantineReason::ProviderIdentityMismatch)
        };
    }
    let Some(vm) = vm else {
        return absent(record.phase);
    };
    match record.phase {
        Phase::NotStarted | Phase::Closed => Decision::Report,
        Phase::Deleted | Phase::Lost => match record.instance_id {
            Some(_) => Decision::DeleteLeaked,
            None => Decision::Report,
        },
        Phase::Reserved => Decision::Quarantine(QuarantineReason::OwnershipDoubt),
        // The clone's reply was lost, so its instance id is learned here from the listing, by
        // name. That is safe under ADR 0026: the name is this attempt's exact `OwnedVmName`
        // (slot and generation included), no instance id is recorded yet to contradict it (a
        // recorded one that differs was quarantined above), and the id taken here is recorded
        // (`VmCloned`) and is the proof every later stop or delete carries, which the data plane
        // re-observes before acting. A VM someone else made under our name is thus adopted at
        // worst as ours, never deleted without the id it was adopted with.
        Phase::Cloning
        | Phase::Backoff {
            stage: Stage::Clone,
            ..
        } => match (record.pending, &record.instance_id) {
            (Some(DirectiveTag::CloneVm), None) => {
                Decision::Adopt(Adoption::Clone(vm.instance_id.clone()))
            }
            (Some(_) | None, Some(_) | None) => {
                Decision::Quarantine(QuarantineReason::OwnershipDoubt)
            }
        },
        Phase::Booting
        | Phase::Backoff {
            stage: Stage::Boot, ..
        } => match vm.run {
            VmRun::Running { shim_alive: false } => Decision::Retire(RetireReason::VmStopped),
            VmRun::Running { shim_alive: true } | VmRun::Stopped { .. } => {
                Decision::Adopt(Adoption::Continue)
            }
        },
        Phase::Verifying
        | Phase::Backoff {
            stage: Stage::Verify,
            ..
        } => prepared_vm(vm),
        // The registration is read before the VM's liveness: a runner GitHub knows for an
        // attempt that never activated is an outcome the journal cannot prove, so the VM is
        // quarantined, stopped or not, and its evidence kept (ADR 0025); retiring a stopped one
        // would delete it on that doubt, and the runner has no journaled id to deregister, so
        // the reaper could only report it (ADR 0029). Pinned by
        // `tests/recovery.rs::should_quarantine_when_a_prepared_vm_has_a_runner_github_knows`.
        Phase::Prepared => match github {
            GithubView::IssuedNotDelivered
            | GithubView::Online
            | GithubView::Busy
            | GithubView::Offline => Decision::Quarantine(QuarantineReason::JitPresumedLeaked),
            GithubView::NotRegistered | GithubView::Absent | GithubView::Unreachable => {
                prepared_vm(vm)
            }
        },
        Phase::Activating
        | Phase::Backoff {
            stage: Stage::Activate,
            ..
        } => activating(vm, github),
        Phase::Listening => listening(vm, github),
        Phase::Busy => busy(vm, github),
        Phase::Finished | Phase::Retiring => Decision::Adopt(Adoption::Continue),
        Phase::Deleting => match github {
            GithubView::Online | GithubView::Busy => Decision::SuspendCleanup,
            GithubView::NotRegistered
            | GithubView::IssuedNotDelivered
            | GithubView::Offline
            | GithubView::Absent
            | GithubView::Unreachable => Decision::Adopt(Adoption::Continue),
        },
        Phase::Quarantined { .. } => Decision::Keep,
    }
}

/// A complete listing without the VM. Read only after [`ProviderView::Incomplete`] was ruled
/// out (I25).
const fn absent(phase: Phase) -> Decision {
    match phase {
        Phase::NotStarted | Phase::Deleted | Phase::Lost | Phase::Closed => Decision::Ignore,
        Phase::Reserved => Decision::Adopt(Adoption::Continue),
        Phase::Cloning
        | Phase::Backoff {
            stage: Stage::Clone,
            ..
        } => Decision::RetryClone,
        Phase::Deleting => Decision::ConfirmDeleted,
        Phase::Quarantined { .. } => Decision::Keep,
        Phase::Booting
        | Phase::Verifying
        | Phase::Prepared
        | Phase::Activating
        | Phase::Listening
        | Phase::Busy
        | Phase::Finished
        | Phase::Retiring
        | Phase::Backoff { .. } => Decision::MarkLost,
    }
}

/// A VM that should be booted and idle with no runner: kept only under a live shim, and then
/// verified again before it is used.
const fn prepared_vm(vm: &ObservedVm) -> Decision {
    match vm.run {
        VmRun::Running { shim_alive: true } => Decision::Adopt(Adoption::Reverify),
        VmRun::Running { shim_alive: false } | VmRun::Stopped { .. } => {
            Decision::Retire(RetireReason::VmStopped)
        }
    }
}

/// A launch whose outcome is unknown: the JIT is presumed leaked (I22, P13, KER-04F) and the
/// runner deregistered by its journaled id before any stop, except under a job.
///
/// - `Busy`: GitHub shows the runner running a job, so the runner did start and a job is live.
///   Retiring it would aim cleanup at that job (F13); it is adopted and monitored, its JIT
///   still presumed leaked, so it binds no further job and is retired once the job ends.
/// - `Online`: the runner started and is idle. It is still presumed leaked: KER-04F decided
///   that a re-observed launch never gets a job and is retired after deregistration, and an
///   idle runner's deregistration kills nothing (a job that lands during it is refused 422
///   and the retirement withdrawn: F33, I21).
/// - `Offline`: the runner started and is gone, or the JIT was used elsewhere and left. No job
///   can be live on an offline runner, and the doubt is the delivery's, which P13 answers by
///   presuming the JIT leaked and retiring, not by quarantine (kept for identity doubt after
///   delivery and redaction violations): presumed leaked.
/// - `Absent`, `IssuedNotDelivered`: the plan row: presumed leaked.
/// - `Unreachable`: presumed leaked; the deregistration that precedes any stop waits for
///   GitHub's answer, so a job hidden behind the outage is refused 422, never killed (I21).
///
/// Pinned by `tests/recovery.rs::should_decide_each_plan_row_when_its_inputs_are_given` and
/// `should_never_retire_when_a_busy_vm_runs`.
const fn activating(vm: &ObservedVm, github: GithubView) -> Decision {
    match github {
        GithubView::NotRegistered => prepared_vm(vm),
        GithubView::Busy => Decision::Adopt(Adoption::Monitor),
        GithubView::IssuedNotDelivered
        | GithubView::Online
        | GithubView::Offline
        | GithubView::Absent
        | GithubView::Unreachable => Decision::PresumeLeaked,
    }
}

const fn listening(vm: &ObservedVm, github: GithubView) -> Decision {
    if !running(vm) {
        return Decision::Retire(RetireReason::VmStopped);
    }
    match github {
        GithubView::Absent => Decision::Retire(RetireReason::JitPresumedLeaked),
        GithubView::NotRegistered | GithubView::IssuedNotDelivered => Decision::PresumeLeaked,
        GithubView::Online | GithubView::Busy | GithubView::Offline | GithubView::Unreachable => {
            Decision::Adopt(Adoption::Monitor)
        }
    }
}

/// A running Busy VM is never retired, whatever GitHub says (F13); a stopped one only once
/// GitHub shows its runner offline or gone, never on `unreachable`.
const fn busy(vm: &ObservedVm, github: GithubView) -> Decision {
    if running(vm) {
        return Decision::Adopt(Adoption::Monitor);
    }
    match github {
        GithubView::Offline | GithubView::Absent => Decision::Retire(RetireReason::GuestDied),
        GithubView::NotRegistered
        | GithubView::IssuedNotDelivered
        | GithubView::Online
        | GithubView::Busy
        | GithubView::Unreachable => Decision::Adopt(Adoption::Monitor),
    }
}

/// Safe mode's proposal: adopt only what the journal provably owns, quarantine what it cannot
/// prove, mark lost only on a complete listing's absence, never anything destructive.
fn propose(subject: &Subject, provider: &ProviderView) -> Proposal {
    let vm = match provider {
        ProviderView::Incomplete => return Proposal::WaitForInventory,
        ProviderView::Absent => None,
        ProviderView::Present(vm) => Some(vm),
    };
    let record = match subject {
        Subject::Unrecorded(class) => {
            return match (class, vm) {
                (NameClass::ManagedLooking, Some(_)) => Proposal::Report,
                (NameClass::ManagedLooking, None)
                | (NameClass::ImageBase | NameClass::Foreign, Some(_) | None) => Proposal::Ignore,
            };
        }
        Subject::Attempt(record) => record,
    };
    if quarantined(record.phase) {
        return Proposal::Keep;
    }
    match (vm, terminal(record.phase), &record.instance_id) {
        (None, true, _) => Proposal::Ignore,
        (None, false, _) => Proposal::MarkLost,
        (Some(_), true, _) => Proposal::Report,
        (Some(vm), false, Some(recorded)) if vm.instance_id == *recorded => Proposal::Adopt,
        (Some(_), false, Some(_)) => {
            Proposal::Quarantine(QuarantineReason::ProviderIdentityMismatch)
        }
        (Some(_), false, None) => Proposal::Quarantine(QuarantineReason::OwnershipDoubt),
    }
}
