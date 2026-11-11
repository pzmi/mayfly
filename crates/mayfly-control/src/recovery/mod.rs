//! Startup recovery (policy P1, plan §10, `doc/domain-model.md` §7) and the application of a
//! recovery plan (P20, [`repair`]). The model is `formal/recovery.qnt` (`open`, `verify`,
//! `mint`, `welcome`, `inventoryWith`, `classifyWith`, `completeRecovery`); this module
//! implements it as it stands.
//!
//! [`Startup::open`] runs, in order, everything before the inventory:
//!
//! 1. **Verify** ([`verify`]): the stored kinds against this build's registry, then the store's
//!    structure through the [`StructureCheck`] port; either failing records safe mode at the
//!    mint.
//! 2. **Prepare the projections** ([`crate::projection::prepare`]) before any writer, so every
//!    unit after it writes this build's rows (ES-04). A journal holding a kind this build does
//!    not decode is not rebuilt (it cannot be folded): the run is in safe mode, which admits
//!    nothing, and the projections are prepared by the restart that trusts the journal.
//! 3. **`StartRun`**, then the data plane's `Hello`, whose `epoch_floor` is the external floor.
//! 4. **`MintEpoch`** above the journal and the floor (I17), with the verification and the
//!    crash-loop finding, an input the start-up watchdog computes outside the journal (OPS-05,
//!    ADR 0031; this module never derives it). A safe mode a former run recorded is left as a
//!    verified restart when this run's verification trusts the journal and no plan is required.
//! 5. **Admission closed** (`CloseAdmission{recovering}`, P3) on a registered `Host` before the
//!    session opens, so nothing is reserved until recovery completes.
//! 6. **`Welcome`** with the minted epoch, the mode (`safe` while `HostRun` records safe mode)
//!    and `cp_clock`, the `Clock` port's reading when it is written (ADR 0020 as amended
//!    2026-11-10), and the session recorded in [`HostSessions`].
//!
//! [`Startup::recover`] then reads one inventory of the session:
//!
//! - **Incomplete**: nothing is decided, released or recorded (I25); admission stays closed and
//!   the caller passes the next inventory ([`Recovery::Waiting`]).
//! - **Safe mode**: each name's proposal is reported for a plan (`mayfly recover --dry-run`)
//!   and nothing is written to any attempt or host: no adoption, no fact, no command, admission
//!   stays closed (I29; pinned by
//!   `tests/store/recovery.rs::should_admit_and_delete_nothing_when_the_journal_was_restored_from_an_older_backup`).
//! - **Trusted**: every attempt the host leases ([`host_attempts`]) is classified by the
//!   recovery table ([`classify`]) against the state the journal holds, then: every attempt that can be adopted
//!   is adopted under the run's epoch; the inventory is recorded through the [`Inbox`], so a
//!   pending `Unknown` effect is resolved by the observation only (I10); the rows that act beyond
//!   the inventory become ordinary commands (`RequestRetirement`, `Quarantine`); the saga
//!   reconsiders each adopted attempt, re-arming the deadline of a directive still in flight;
//!   then, once every attempt was seen, `CompleteRecovery` and `OpenAdmission{recovering}` (the
//!   model's `complete`). While a row waits (a listed VM whose instance id is unread),
//!   recovery does not complete and admission stays closed ([`Recovery::Waiting`]; pinned by
//!   `tests/store/recovery.rs::should_keep_admission_closed_when_a_listed_vm_says_nothing_yet`);
//!   the next pass of the same [`Startup`] keeps what this one did.
//!
//! Two rows have no command to issue yet and are reported for the operator instead:
//! `DeleteLeaked` (a terminal attempt's VM is present; no attempt command deletes for a closed
//! case) and `Report` (a name no attempt records). `SuspendCleanup` (a pending delete of a VM
//! whose runner GitHub shows active) is held. The model adopts the attempt with its effect held
//! (`apply`'s `hold`, `activeRunnerSuspendsCleanupTest`), but the `Attempt` aggregate cannot hold
//! a directive in flight after an adoption (adopting lets the relay's sink send the pending
//! delete again, and `Quarantine` is refused while a directive is in flight). Until CP-04C lands
//! that hold model first, the attempt is left **unadopted**: the sink holds its pending delete
//! (`attempt_not_adopted`), its lease keeps its slot, so nothing is admitted in its place,
//! recovery completes and admission reopens with it listed in [`Recovered::held`], and
//! [`Startup::resume_held`] classifies it again on every later inventory, adopting it and
//! resuming its cleanup once GitHub shows the runner neither online nor busy (pinned by
//! `tests/store/recovery.rs::should_suspend_the_cleanup_when_a_deleting_vm_runs_a_busy_runner` and
//! `should_resume_a_held_cleanup_when_github_reports_the_runner_offline`; the divergence is
//! recorded in `doc/formal-models.md`).
//! Adoption references the inventory by [`inventory_ref`], shared with a plan's application:
//! the inventory itself is not an event.
//!
//! What the protocol's listing does not carry, the table reads conservatively: a running VM's
//! shim is taken as alive, and a stopped VM as stopped without `exit.json` (the table reads both
//! alike, ADR 0037).

pub mod repair;

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::{Mutex, PoisonError};

use mayfly_control_domain::attempt::{
    Attempt, AttemptCommand, AttemptRejection, AttemptState, Phase, RetireReason,
};
use mayfly_control_domain::host::{AdmissionCause, Host, HostCommand, HostRejection};
use mayfly_control_domain::operations::hostrun::{
    HostRun, HostRunCommand, HostRunRejection, HostRunState, RecoveryCounts, SafeModeExit,
    SafeModeReason, Verification,
};
use mayfly_control_domain::operations::recovery::{
    AttemptRecord, Decision, GithubView, JournalView, NameClass, ObservedVm, Proposal,
    ProviderView, SafeModeCause, Subject, Trust, VmRun, classify,
};
use mayfly_control_domain::{Actor, Aggregate, Command, ControlAggregates, Rejection as _};
use mayfly_domain::{
    AttemptId, CommandId, Epoch, EventId, HostId, ObservationId, Resources, StateDigest,
    VmNameClass, classify_vm_name,
};
use mayfly_protocol::{
    HandshakeRefusal, HandshakeRefused, HandshakeResponder, LinkError, Mode, Observation,
    ObservationKind, ProtocolVersion, SUPPORTED, VmObservation, Welcome, negotiate,
};

use crate::ids::derive;
use crate::inbox::{Inbox, host_attempts};
use crate::journal::{Journal, StorageError};
use crate::projection::{
    BuildId, ProjectionRebuild, ProjectionStore, Projections, RebuildError, prepare,
};
use crate::saga::AttemptSaga;
use crate::sessions::{HostSession, HostSessions};
use crate::store::{
    AggregateStore, Clock, KindCheck, Outcome, Rng, SaveError, SaveRequest, StoreNotifier,
    check_stored_kinds,
};

const ID_DOMAIN: &[u8] = b"mayfly-startup-recovery-v1\n";

/// The store's structural verification (an integrity check of the database), a port the
/// adapter that owns the store implements.
pub trait StructureCheck: Send + Sync {
    /// Whether the store's structure verifies.
    ///
    /// # Errors
    /// [`StorageError`] when the check cannot run.
    fn verify(&self) -> impl Future<Output = Result<bool, StorageError>> + Send;
}

/// What the start-up verification found: an unknown stored kind first, then the structure.
///
/// # Errors
/// [`StorageError`] when the journal or the check fails.
pub async fn verify<J: Journal, S: StructureCheck>(
    journal: &J,
    structure: &S,
) -> Result<Verification, StorageError> {
    Ok(
        match check_stored_kinds::<ControlAggregates, J>(journal).await? {
            KindCheck::SafeMode { .. } => Verification::UnknownEventKind,
            KindCheck::Known => {
                if structure.verify().await? {
                    Verification::Verified
                } else {
                    Verification::StructureFailed
                }
            }
        },
    )
}

/// The complete inventory an adoption was decided on, as the deciding use case knows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InventorySeen {
    /// Start-up read it as this observation of the session.
    Observation(ObservationId),
    /// A recovery plan is bound to its digest (`Bindings::inventory`).
    Digest(StateDigest),
}

/// The adoption's reference to the inventory it was decided on (`Adopt::inventory_ref`), the one
/// derivation start-up and a plan's application share: from the host and the inventory, since
/// the inventory is not an event of its own. Start-up names the inventory by its observation, a
/// plan by the digest it is bound to (it holds no observation id), so the two kinds are tagged
/// apart.
#[must_use]
pub fn inventory_ref(host: &HostId, inventory: InventorySeen) -> EventId {
    let (kind, id): (&[u8], u128) = match inventory {
        InventorySeen::Observation(id) => (b"observation", id.as_u128()),
        InventorySeen::Digest(digest) => (b"digest", digest.as_u128()),
    };
    EventId::from_u128(derive(
        ID_DOMAIN,
        &[
            b"inventory",
            host.as_str().as_bytes(),
            kind,
            &id.to_be_bytes(),
        ],
    ))
}

/// Why start-up stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartupError {
    /// The store failed or refused a unit.
    Store(SaveError),
    /// The journal could not be read.
    Storage(StorageError),
    /// The projections could not be prepared.
    Projections(RebuildError),
    /// The link failed.
    Link(LinkError),
    /// The data plane speaks no major this build speaks: the hello was refused.
    Version,
    /// The data plane said hello for another host.
    WrongHost(HostId),
    /// `HostRun` refused a start-up command, by its code.
    Refused(&'static str),
    /// The observation passed to [`Startup::recover`] is not an inventory.
    NotAnInventory,
}

impl std::fmt::Display for StartupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Store(e) => write!(f, "the store failed: {e}"),
            Self::Storage(e) => write!(f, "the journal failed: {e}"),
            Self::Projections(e) => write!(f, "the projections could not be prepared: {e}"),
            Self::Link(e) => write!(f, "the link failed: {e}"),
            Self::Version => f.write_str("the data plane speaks no protocol major of this build"),
            Self::WrongHost(h) => write!(f, "the data plane said hello for host {h}"),
            Self::Refused(code) => write!(f, "the run refused a start-up command: {code}"),
            Self::NotAnInventory => f.write_str("the observation is not an inventory"),
        }
    }
}

impl std::error::Error for StartupError {}

impl From<SaveError> for StartupError {
    fn from(e: SaveError) -> Self {
        Self::Store(e)
    }
}

/// What start-up needs to know of this daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartupConfig {
    /// The host.
    pub host: HostId,
    /// The host's boot session id.
    pub boot_id: String,
    /// This build's version.
    pub version: String,
    /// The operator reserves a host is registered with.
    pub reserves: Resources,
    /// The daemon is crash-looping (P25): OPS-05's watchdog computes it from its start record
    /// outside the journal; start-up only passes it to `MintEpoch`.
    pub crash_loop: bool,
}

/// The session start-up opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Opened {
    /// The run's epoch, the session's.
    pub epoch: Epoch,
    /// The session's mode.
    pub mode: Mode,
    /// The negotiated version.
    pub accepted_version: ProtocolVersion,
    /// The verification the mint recorded.
    pub verification: Verification,
}

/// What [`Startup::recover`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Recovery {
    /// Recovery cannot complete on this inventory, and admission stays closed (the model's
    /// `complete` needs every attempt seen): the inventory was incomplete (every attempt waits,
    /// nothing decided, I25) or a listed VM's instance id was unread. Pass the next inventory to
    /// the same [`Startup`]: what this pass did counts towards the pass that completes.
    Waiting {
        /// Attempts the inventory says nothing of.
        waiting: Vec<AttemptId>,
    },
    /// Safe mode: each name's proposal, for a plan; nothing written.
    SafeMode {
        /// Why the journal is not trusted.
        cause: SafeModeCause,
        /// The proposal for each recorded attempt.
        attempts: Vec<(AttemptId, Proposal)>,
        /// The proposal for each listed name no attempt records.
        names: Vec<(String, Proposal)>,
    },
    /// Recovery completed and admission opened.
    Completed(Recovered),
}

/// What a trusted recovery did, for the report and `RecoveryCompleted`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Recovered {
    /// The decision for each recorded attempt.
    pub decisions: Vec<(AttemptId, Decision)>,
    /// The attempts adopted under the run's epoch.
    pub adopted: Vec<AttemptId>,
    /// Names reported for the operator: unrecorded `mf-*` names and leaked VMs of terminal
    /// attempts.
    pub reported: Vec<String>,
    /// Attempts whose cleanup is held for inspection (`SuspendCleanup`), left unadopted (see
    /// the module docs); [`Startup::resume_held`] resumes them.
    pub held: Vec<AttemptId>,
    /// The counts recorded with `RecoveryCompleted`, over every pass of the run.
    pub counts: RecoveryCounts,
}

/// The startup use case of one host (see the module docs).
pub struct Startup<'s, J, C, R, N> {
    store: &'s AggregateStore<J, C, R, N>,
    sessions: HostSessions,
    config: StartupConfig,
    /// What the run's passes did so far: a pass that waits keeps its adoptions and counts for
    /// the one that completes.
    progress: Mutex<Recovered>,
}

impl<J, C, R, N> std::fmt::Debug for Startup<'_, J, C, R, N> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Startup")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl<'s, J: Journal, C: Clock, R: Rng, N: StoreNotifier> Startup<'s, J, C, R, N> {
    /// Start-up of `config.host` over `store`, recording the session in `sessions`.
    pub fn new(
        store: &'s AggregateStore<J, C, R, N>,
        sessions: HostSessions,
        config: StartupConfig,
    ) -> Self {
        Self {
            store,
            sessions,
            config,
            progress: Mutex::new(Recovered::default()),
        }
    }

    /// Steps 1 to 6 of the module docs: verified, projections prepared, run started, epoch
    /// minted above the hello's floor, admission closed, session welcomed and recorded. Pinned
    /// by `tests/store/recovery.rs::should_welcome_with_the_minted_epoch_and_the_clock_when_a_run_starts`.
    ///
    /// # Errors
    /// [`StartupError`]; a version refusal is sent to the data plane first.
    pub async fn open<L, S, P>(
        &self,
        link: &mut L,
        structure: &S,
        projections: (&P, &Projections, &BuildId),
    ) -> Result<Opened, StartupError>
    where
        L: HandshakeResponder,
        S: StructureCheck,
        P: ProjectionStore + ProjectionRebuild,
    {
        let journal = self.store.journal();
        let verification = verify(journal, structure)
            .await
            .map_err(StartupError::Storage)?;
        if verification != Verification::UnknownEventKind {
            let (rows, specs, build) = projections;
            prepare(journal, rows, specs, build)
                .await
                .map_err(StartupError::Projections)?;
        }
        let run = self.hostrun().await?;
        let run_no = run.run_no.saturating_add(1);
        self.host_run(
            run.epoch,
            b"start",
            run_no,
            HostRunCommand::StartRun {
                boot_id: self.config.boot_id.clone(),
                version: self.config.version.clone(),
            },
        )
        .await?;
        let hello = link.next_hello().await.map_err(StartupError::Link)?;
        if hello.host_id != self.config.host {
            return Err(StartupError::WrongHost(hello.host_id));
        }
        let Ok(accepted_version) = negotiate(&hello.supported, SUPPORTED) else {
            link.refuse(HandshakeRefused {
                reason: HandshakeRefusal::UnsupportedVersion {
                    supported: SUPPORTED.to_vec(),
                },
            })
            .await
            .map_err(StartupError::Link)?;
            return Err(StartupError::Version);
        };
        self.host_run(
            run.epoch,
            b"mint",
            run_no,
            HostRunCommand::MintEpoch {
                floor: hello.epoch_floor,
                verification,
                crash_loop: self.config.crash_loop,
            },
        )
        .await?;
        let mut run = self.hostrun().await?;
        let epoch = run.epoch;
        if run
            .safe_mode
            .is_some_and(|s| s.entered_run < run.run_no && !s.needs_plan)
            && verification == Verification::Verified
        {
            match self
                .host_run(
                    epoch,
                    b"verified_restart",
                    run_no,
                    HostRunCommand::LeaveSafeMode {
                        exit: SafeModeExit::VerifiedRestart,
                    },
                )
                .await
            {
                Ok(()) | Err(StartupError::Refused(_)) => {}
                Err(error) => return Err(error),
            }
            run = self.hostrun().await?;
        }
        self.close_admission(epoch).await?;
        let mode = if run.safe_mode.is_some() {
            Mode::Safe
        } else {
            Mode::Normal
        };
        link.welcome(Welcome {
            epoch,
            mode,
            accepted_version,
            cp_clock: Some(self.store.clock().now()),
        })
        .await
        .map_err(StartupError::Link)?;
        self.sessions
            .open(self.config.host.clone(), HostSession { epoch, mode });
        Ok(Opened {
            epoch,
            mode,
            accepted_version,
            verification,
        })
    }

    /// Recovery from one inventory of the session (see the module docs); `github` is what the
    /// registrations and GitHub show per attempt (an attempt not in it reads `unreachable`,
    /// never `absent`). Pinned by the row tests of `tests/store/recovery.rs`.
    ///
    /// # Errors
    /// [`StartupError`]: a store fault, or [`StartupError::NotAnInventory`].
    pub async fn recover(
        &self,
        inventory: &Observation,
        github: &BTreeMap<AttemptId, GithubView>,
        saga: &AttemptSaga<'_, J, C, R, N>,
    ) -> Result<Recovery, StartupError> {
        let vms = match &inventory.kind {
            ObservationKind::Inventory { vms } => vms,
            ObservationKind::InventoryIncomplete { .. } => {
                return Ok(Recovery::Waiting {
                    waiting: host_attempts(self.store, &self.config.host)
                        .await?
                        .into_iter()
                        .map(|(id, _)| id)
                        .collect(),
                });
            }
            _ => return Err(StartupError::NotAnInventory),
        };
        let run = self.hostrun().await?;
        let trust = match run.safe_mode {
            Some(safe) => Trust::SafeMode(safe_cause(safe.reason)),
            None => Trust::Trusted,
        };
        let attempts = host_attempts(self.store, &self.config.host).await?;
        let decisions: Vec<(AttemptId, AttemptState, Decision)> = attempts
            .into_iter()
            .map(|(id, state)| {
                let decision = classify(
                    &JournalView {
                        trust,
                        subject: Subject::Attempt(record_of(&state)),
                    },
                    &provider_view(&state, vms),
                    github.get(&id).copied().unwrap_or(GithubView::Unreachable),
                );
                (id, state, decision)
            })
            .collect();
        let names = unrecorded(vms, &decisions);
        if let Trust::SafeMode(cause) = trust {
            return Ok(Recovery::SafeMode {
                cause,
                attempts: decisions
                    .iter()
                    .filter_map(|(id, _, d)| proposal(d).map(|p| (*id, p)))
                    .collect(),
                names: names
                    .iter()
                    .filter_map(|(vm, class)| {
                        proposal(&unrecorded_decision(trust, *class, vm))
                            .map(|p| (vm.name.clone(), p))
                    })
                    .collect(),
            });
        }
        let epoch = run.epoch;
        let mut recovered = self.lock().clone();
        self.pass(inventory, &decisions, epoch, saga, &mut recovered)
            .await?;
        for (vm, class) in &names {
            if matches!(unrecorded_decision(trust, *class, vm), Decision::Report)
                && !recovered.reported.contains(&vm.name)
            {
                recovered.reported.push(vm.name.clone());
            }
        }
        let rows = |kind: fn(&Decision) -> bool| -> Vec<AttemptId> {
            decisions
                .iter()
                .filter(|(_, _, d)| kind(d))
                .map(|(id, _, _)| *id)
                .collect()
        };
        let waiting = rows(|d| matches!(d, Decision::Wait));
        recovered.held = rows(|d| matches!(d, Decision::SuspendCleanup));
        recovered.counts.adopted = count(recovered.adopted.len());
        recovered.counts.reported = count(recovered.reported.len());
        recovered.decisions = decisions.into_iter().map(|(id, _, d)| (id, d)).collect();
        *self.lock() = recovered.clone();
        if !waiting.is_empty() {
            return Ok(Recovery::Waiting { waiting });
        }
        self.host_run(
            epoch,
            b"complete",
            run.run_no,
            HostRunCommand::CompleteRecovery {
                counts: recovered.counts,
            },
        )
        .await?;
        self.admission(
            epoch,
            b"open",
            HostCommand::OpenAdmission {
                cause: AdmissionCause::Recovering,
            },
        )
        .await?;
        Ok(Recovery::Completed(recovered))
    }

    /// A later inventory of the run, for the attempts recovery left held: each attempt of the
    /// host not adopted under the run is classified again, and one that is no longer held (its
    /// runner GitHub shows neither online nor busy) is adopted and its cleanup resumes, as
    /// recovery would have done. Returns the attempts still held; nothing in safe mode or on an
    /// incomplete inventory. RT-02 calls it on every inventory while any is held. Pinned by
    /// `tests/store/recovery.rs::should_resume_a_held_cleanup_when_github_reports_the_runner_offline`.
    ///
    /// # Errors
    /// [`StartupError`]: a store fault, or [`StartupError::NotAnInventory`].
    pub async fn resume_held(
        &self,
        inventory: &Observation,
        github: &BTreeMap<AttemptId, GithubView>,
        saga: &AttemptSaga<'_, J, C, R, N>,
    ) -> Result<Vec<AttemptId>, StartupError> {
        let run = self.hostrun().await?;
        let unadopted: Vec<(AttemptId, AttemptState)> =
            host_attempts(self.store, &self.config.host)
                .await?
                .into_iter()
                .filter(|(_, state)| adoptable(state, run.epoch))
                .collect();
        let vms = match &inventory.kind {
            ObservationKind::Inventory { vms } => vms,
            ObservationKind::InventoryIncomplete { .. } => {
                return Ok(unadopted.into_iter().map(|(id, _)| id).collect());
            }
            _ => return Err(StartupError::NotAnInventory),
        };
        if run.safe_mode.is_some() {
            return Ok(Vec::new());
        }
        let decisions: Vec<(AttemptId, AttemptState, Decision)> = unadopted
            .into_iter()
            .map(|(id, state)| {
                let decision = classify(
                    &JournalView {
                        trust: Trust::Trusted,
                        subject: Subject::Attempt(record_of(&state)),
                    },
                    &provider_view(&state, vms),
                    github.get(&id).copied().unwrap_or(GithubView::Unreachable),
                );
                (id, state, decision)
            })
            .collect();
        let mut resumed = Recovered::default();
        self.pass(inventory, &decisions, run.epoch, saga, &mut resumed)
            .await?;
        Ok(decisions
            .into_iter()
            .filter(|(_, _, d)| matches!(d, Decision::SuspendCleanup))
            .map(|(id, _, _)| id)
            .collect())
    }

    /// One trusted pass over `decisions`: adopt, record the inventory, act, reconsider.
    async fn pass(
        &self,
        inventory: &Observation,
        decisions: &[(AttemptId, AttemptState, Decision)],
        epoch: Epoch,
        saga: &AttemptSaga<'_, J, C, R, N>,
        recovered: &mut Recovered,
    ) -> Result<(), StartupError> {
        let reference = inventory_ref(
            &self.config.host,
            InventorySeen::Observation(inventory.observation_id),
        );
        let mut adopted = Vec::new();
        for (id, state, decision) in decisions {
            if adoptable(state, epoch) && adopts(decision) {
                let outcome = self
                    .attempt(
                        *id,
                        epoch,
                        b"adopt",
                        AttemptCommand::Adopt {
                            inventory_ref: reference,
                        },
                    )
                    .await?;
                if matches!(outcome, Outcome::Accepted { .. }) {
                    adopted.push(*id);
                    if !recovered.adopted.contains(id) {
                        recovered.adopted.push(*id);
                    }
                }
            }
        }
        Inbox::new(self.store, self.config.host.clone(), epoch)
            .record(inventory)
            .await?;
        for (id, state, decision) in decisions {
            self.act(*id, state, decision, epoch, recovered).await?;
        }
        for id in &adopted {
            saga.reconsider(&id.to_string()).await?;
        }
        Ok(())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Recovered> {
        self.progress.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The command a row asks for beyond adoption and the inventory, and the counts.
    async fn act(
        &self,
        id: AttemptId,
        state: &AttemptState,
        decision: &Decision,
        epoch: Epoch,
        recovered: &mut Recovered,
    ) -> Result<(), StartupError> {
        let command = match decision {
            Decision::Retire(reason) => Some(AttemptCommand::RequestRetirement { reason: *reason }),
            Decision::PresumeLeaked => Some(AttemptCommand::RequestRetirement {
                reason: RetireReason::JitPresumedLeaked,
            }),
            Decision::Quarantine(reason) => Some(AttemptCommand::Quarantine { reason: *reason }),
            Decision::SuspendCleanup | Decision::DeleteLeaked => {
                if let Some(identity) = &state.identity {
                    let name = identity.vm_name.to_string();
                    if !recovered.reported.contains(&name) {
                        recovered.reported.push(name);
                    }
                }
                None
            }
            Decision::ConfirmDeleted | Decision::MarkLost => {
                let now = self.store.load::<Attempt>(&id.to_string()).await?;
                let ended = |p: Phase| matches!(p, Phase::Deleted | Phase::Lost);
                if ended(now.phase) && !ended(state.phase) {
                    recovered.counts.lost = recovered.counts.lost.saturating_add(1);
                }
                None
            }
            Decision::Wait
            | Decision::Adopt(_)
            | Decision::RetryClone
            | Decision::Keep
            | Decision::Report
            | Decision::Ignore
            | Decision::SafeMode { .. } => None,
        };
        let Some(command) = command else {
            return Ok(());
        };
        let quarantine = matches!(command, AttemptCommand::Quarantine { .. });
        let outcome = self.attempt(id, epoch, b"act", command).await?;
        if matches!(
            outcome,
            Outcome::Accepted {
                replayed: false,
                ..
            }
        ) {
            if quarantine {
                recovered.counts.quarantined = recovered.counts.quarantined.saturating_add(1);
            } else {
                recovered.counts.retired = recovered.counts.retired.saturating_add(1);
            }
        }
        Ok(())
    }

    async fn hostrun(&self) -> Result<HostRunState, StartupError> {
        Ok(self
            .store
            .load::<HostRun>(self.config.host.as_str())
            .await?)
    }

    /// A `HostRun` command of run `run_no`, its id derived from the host, the step and the run.
    async fn host_run(
        &self,
        epoch: Epoch,
        step: &[u8],
        run_no: u64,
        body: HostRunCommand,
    ) -> Result<(), StartupError> {
        let id = self.id(&[step, &run_no.to_be_bytes()]);
        match self
            .save::<HostRun>(self.config.host.as_str(), id, epoch, body)
            .await?
        {
            Outcome::Accepted { .. } => Ok(()),
            Outcome::Refused { rejection, .. } => Err(StartupError::Refused(rejection.code())),
        }
    }

    /// Registers the host if it is not, then closes its admission for recovery (P3).
    async fn close_admission(&self, epoch: Epoch) -> Result<(), StartupError> {
        let host = self.store.load::<Host>(self.config.host.as_str()).await?;
        if !host.registered {
            self.admission(
                epoch,
                b"register",
                HostCommand::RegisterHost {
                    reserves: self.config.reserves,
                },
            )
            .await?;
        }
        self.admission(
            epoch,
            b"close",
            HostCommand::CloseAdmission {
                cause: AdmissionCause::Recovering,
            },
        )
        .await
    }

    async fn admission(
        &self,
        epoch: Epoch,
        step: &[u8],
        body: HostCommand,
    ) -> Result<(), StartupError> {
        let id = self.id(&[step, &epoch.get().to_be_bytes()]);
        match self
            .save::<Host>(self.config.host.as_str(), id, epoch, body)
            .await?
        {
            Outcome::Accepted { .. }
            | Outcome::Refused {
                rejection: HostRejection::AlreadyRegistered,
                ..
            } => Ok(()),
            Outcome::Refused { rejection, .. } => Err(StartupError::Refused(rejection.code())),
        }
    }

    async fn attempt(
        &self,
        id: AttemptId,
        epoch: Epoch,
        step: &[u8],
        body: AttemptCommand,
    ) -> Result<Outcome<AttemptRejection>, StartupError> {
        let command_id = self.id(&[
            step,
            &id.as_u128().to_be_bytes(),
            &epoch.get().to_be_bytes(),
        ]);
        Ok(self
            .save::<Attempt>(&id.to_string(), command_id, epoch, body)
            .await?)
    }

    async fn save<A: Aggregate>(
        &self,
        instance: &str,
        command_id: CommandId,
        epoch: Epoch,
        body: A::Body,
    ) -> Result<Outcome<A::Rejection>, SaveError>
    where
        A::Body: Clone,
    {
        let command = Command::new(
            command_id,
            self.store.clock().now(),
            epoch,
            Actor::Startup,
            body,
        );
        self.store
            .execute::<A>(SaveRequest::new(instance, command))
            .await
    }

    fn id(&self, parts: &[&[u8]]) -> CommandId {
        let mut all: Vec<&[u8]> = vec![self.config.host.as_str().as_bytes()];
        all.extend_from_slice(parts);
        CommandId::from_u128(derive(ID_DOMAIN, &all))
    }
}

/// Records safe mode entered during the run (an unknown kind met by a save, an operator's
/// `mayfly safe-mode enter`) and holds the session's effects at once (`set_mode`, I29).
///
/// # Errors
/// [`SaveError`]; a refusal is returned as its outcome.
pub async fn enter_safe_mode<J: Journal, C: Clock, R: Rng, N: StoreNotifier>(
    store: &AggregateStore<J, C, R, N>,
    sessions: &HostSessions,
    host: &HostId,
    epoch: Epoch,
    reason: SafeModeReason,
) -> Result<Outcome<HostRunRejection>, SaveError> {
    sessions.set_mode(host, Mode::Safe);
    let reason_code: &[u8] = match reason {
        SafeModeReason::EpochBelowFloor => b"epoch_below_floor",
        SafeModeReason::UnknownEventKind => b"unknown_event_kind",
        SafeModeReason::VerificationFailed => b"verification_failed",
        SafeModeReason::CrashLoop => b"crash_loop",
    };
    let command = Command::new(
        CommandId::from_u128(derive(
            ID_DOMAIN,
            &[
                host.as_str().as_bytes(),
                b"enter_safe_mode",
                reason_code,
                &epoch.get().to_be_bytes(),
            ],
        )),
        store.clock().now(),
        epoch,
        Actor::Startup,
        HostRunCommand::EnterSafeMode { reason },
    );
    store
        .execute::<HostRun>(SaveRequest::new(host.as_str(), command))
        .await
}

/// The data plane's session ended: nothing is sent or issued for `host` until the next welcome.
pub fn session_ended(sessions: &HostSessions, host: &HostId) {
    sessions.close(host);
}

const fn safe_cause(reason: SafeModeReason) -> SafeModeCause {
    match reason {
        SafeModeReason::EpochBelowFloor => SafeModeCause::EpochBelowFloor,
        SafeModeReason::UnknownEventKind => SafeModeCause::UnknownEventKind,
        SafeModeReason::VerificationFailed => SafeModeCause::VerificationFailed,
        SafeModeReason::CrashLoop => SafeModeCause::Recorded,
    }
}

fn record_of(state: &AttemptState) -> AttemptRecord {
    AttemptRecord {
        phase: state.phase,
        instance_id: state.instance_id.clone(),
        pending: state.pending.as_ref().map(|p| p.tag),
    }
}

/// The provider's side for `state`'s VM in a complete listing (see the module docs for the
/// shim and `exit.json`). A listed VM whose instance id is unread says nothing about it.
fn provider_view(state: &AttemptState, vms: &[VmObservation]) -> ProviderView {
    let Some(identity) = &state.identity else {
        return ProviderView::Absent;
    };
    let name = identity.vm_name.to_string();
    vms.iter()
        .find(|vm| vm.name == name)
        .map_or(ProviderView::Absent, listed)
}

/// A listed VM as the table reads it; one whose instance id is unread says nothing.
fn listed(vm: &VmObservation) -> ProviderView {
    match &vm.instance_id {
        None => ProviderView::Incomplete,
        Some(instance_id) => ProviderView::Present(ObservedVm {
            instance_id: instance_id.clone(),
            run: if vm.running {
                VmRun::Running { shim_alive: true }
            } else {
                VmRun::Stopped { exit_json: false }
            },
        }),
    }
}

/// Listed names no recorded attempt names, with their shape.
fn unrecorded<'v>(
    vms: &'v [VmObservation],
    decisions: &[(AttemptId, AttemptState, Decision)],
) -> Vec<(&'v VmObservation, NameClass)> {
    let recorded: Vec<String> = decisions
        .iter()
        .filter_map(|(_, s, _)| s.identity.as_ref().map(|i| i.vm_name.to_string()))
        .collect();
    vms.iter()
        .filter(|vm| !recorded.contains(&vm.name))
        .map(|vm| {
            let class = if vm.name.starts_with("mfb-") {
                NameClass::ImageBase
            } else {
                match classify_vm_name(&vm.name) {
                    VmNameClass::WellFormed(_) | VmNameClass::MalformedManaged => {
                        NameClass::ManagedLooking
                    }
                    VmNameClass::Foreign => NameClass::Foreign,
                }
            };
            (vm, class)
        })
        .collect()
}

fn unrecorded_decision(trust: Trust, class: NameClass, vm: &VmObservation) -> Decision {
    classify(
        &JournalView {
            trust,
            subject: Subject::Unrecorded(class),
        },
        &listed(vm),
        GithubView::Unreachable,
    )
}

const fn proposal(decision: &Decision) -> Option<Proposal> {
    match decision {
        Decision::SafeMode { proposal, .. } => Some(*proposal),
        _ => None,
    }
}

/// Whether the row adopts: not for a VM the listing says nothing of (`Wait`), nor for a cleanup
/// held for inspection, whose pending directive the relay's sink holds while the attempt is not
/// adopted (`attempt_not_adopted`), nor for a name reported or ignored.
const fn adopts(decision: &Decision) -> bool {
    match decision {
        Decision::Wait
        | Decision::SuspendCleanup
        | Decision::Report
        | Decision::Ignore
        | Decision::SafeMode { .. } => false,
        Decision::Adopt(_)
        | Decision::RetryClone
        | Decision::Retire(_)
        | Decision::PresumeLeaked
        | Decision::ConfirmDeleted
        | Decision::MarkLost
        | Decision::DeleteLeaked
        | Decision::Quarantine(_)
        | Decision::Keep => true,
    }
}

/// Whether `state` takes an adoption under `epoch`: started, not terminal, not yet adopted.
fn adoptable(state: &AttemptState, epoch: Epoch) -> bool {
    !matches!(
        state.phase,
        Phase::NotStarted | Phase::Deleted | Phase::Lost | Phase::Closed
    ) && state.epoch < epoch
}

fn count(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}
