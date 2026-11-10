//! The directive executor (plan §3.1, §6, §8.4; ADRs 0024, 0026, 0042; `formal/link.qnt`'s
//! `dpDeliver`).
//!
//! Each directive of the open session moves `Received → Executing → Done | Refused | Unknown`
//! ([`step`]); `Executing` re-observes the target first for a proof-carrying `StopVm` or
//! `DeleteVm`. [`DataPlane::receive`] takes a frame and checks, in this order, the frame's
//! version ([`mayfly_protocol::admit_frame`]), its epoch ([`mayfly_protocol::check_epoch`]: a
//! frame of a session not entered is discarded, one of an older session answered
//! `OutcomeUnknown{stale_epoch}` and never executed), the outcome table (a repeated id answers
//! the recorded outcome), then [`admission`]: safe mode (I29), the deadline, bounded strings,
//! the target (another host's VM, an image base), the proof's own consistency, and the
//! resource ledger (I33). Nothing the control plane allocated overrides a check.

use mayfly_domain::{DirectiveId, Epoch, HostId, Millis, ProviderInstanceId};
use mayfly_protocol::{
    Directive, DirectiveKind, DirectiveOutcome, DirectiveRef, DirectiveTag, EpochCheck, Mode,
    ObservationKind, OwnershipProof, ProtocolVersion, RefusalReason, SUPPORTED, UnknownReason,
    VersionRefusal, admit_frame, check_epoch,
};

use crate::ledger::Ledger;
use crate::link::{self, Link, LinkOutcome, LinkStimulus};

/// The longest runner version a `MaterialiseImage` may carry: it reaches argv and file names.
pub const MAX_RUNNER_VERSION: usize = 64;

/// What `Executing` waits for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// The target's instance id, re-observed right before a destructive effect (ADR 0026).
    ReObserve,
    /// The effect's result.
    Effect,
}

/// A directive's phase in its session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    /// Taken off the link, not yet admitted.
    Received,
    /// Admitted; waiting for the stage.
    Executing(Stage),
    /// Applied; the observation answers it.
    Done(ObservationKind),
    /// Definitely not executed in this epoch: nothing happened.
    Refused(RefusalReason),
    /// Whether it took effect is not known.
    Unknown(UnknownReason),
}

/// One stimulus of a directive's phase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stimulus {
    /// Admission passed; execution starts at the stage.
    Admit(Stage),
    /// Admission refused.
    Deny(RefusalReason),
    /// The target was re-observed: whether its instance id is the proof's.
    ReObserved {
        /// Present with the proof's instance id.
        matches: bool,
    },
    /// The target could not be re-observed: nothing was done.
    ReObserveFailed,
    /// The effect was applied; the observation says what was observed.
    Applied(ObservationKind),
    /// The effect failed definitely (`VmError::Definite`): nothing happened.
    Failed,
    /// The effect may or may not have happened (`VmError::Ambiguous`).
    Ambiguous(UnknownReason),
}

/// Why a stimulus does not apply to a phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Misfit {
    /// Admission of a directive already admitted or settled.
    NotReceived,
    /// A re-observation while none was asked for.
    NotReObserving,
    /// An effect result while no effect runs.
    NotRunning,
    /// The directive's outcome is recorded; nothing changes it in this session.
    Settled,
    /// An `Applied` whose observation is a refusal, an unknown outcome, a fact that answers
    /// nothing or an answer to another kind.
    NotAnApplication,
    /// An effect cannot end `Unknown{stale_epoch}`: only fencing answers that.
    NotAnEffectOutcome,
}

/// A directive's phase after `stimulus`; every pair is defined (plan §6 rule 8). Pinned by
/// `tests/data_plane/matrix.rs::should_define_every_pair_when_each_directive_phase_meets_each_stimulus`
/// (fixture `matrix/directive.txt`).
///
/// # Errors
/// The [`Misfit`] when the stimulus does not apply; the phase is then unchanged.
pub fn step(phase: &Phase, stimulus: Stimulus) -> Result<Phase, Misfit> {
    match phase {
        Phase::Received => match stimulus {
            Stimulus::Admit(stage) => Ok(Phase::Executing(stage)),
            Stimulus::Deny(reason) => Ok(Phase::Refused(reason)),
            Stimulus::ReObserved { .. } | Stimulus::ReObserveFailed => Err(Misfit::NotReObserving),
            Stimulus::Applied(_) | Stimulus::Failed | Stimulus::Ambiguous(_) => {
                Err(Misfit::NotRunning)
            }
        },
        Phase::Executing(Stage::ReObserve) => match stimulus {
            Stimulus::ReObserved { matches: true } => Ok(Phase::Executing(Stage::Effect)),
            Stimulus::ReObserved { matches: false } => {
                Ok(Phase::Refused(RefusalReason::ProofMismatch))
            }
            Stimulus::ReObserveFailed => Ok(Phase::Refused(RefusalReason::EffectFailed)),
            Stimulus::Admit(_) | Stimulus::Deny(_) => Err(Misfit::NotReceived),
            Stimulus::Applied(_) | Stimulus::Failed | Stimulus::Ambiguous(_) => {
                Err(Misfit::NotRunning)
            }
        },
        Phase::Executing(Stage::Effect) => match stimulus {
            Stimulus::Applied(kind) => match kind.outcome() {
                Some(DirectiveOutcome::Applied) => Ok(Phase::Done(kind)),
                Some(DirectiveOutcome::Refused(_) | DirectiveOutcome::Unknown) | None => {
                    Err(Misfit::NotAnApplication)
                }
            },
            Stimulus::Failed => Ok(Phase::Refused(RefusalReason::EffectFailed)),
            Stimulus::Ambiguous(reason) => match reason {
                UnknownReason::Deadline | UnknownReason::ProcessLost | UnknownReason::HostSlept => {
                    Ok(Phase::Unknown(reason))
                }
                UnknownReason::StaleEpoch => Err(Misfit::NotAnEffectOutcome),
            },
            Stimulus::Admit(_) | Stimulus::Deny(_) => Err(Misfit::NotReceived),
            Stimulus::ReObserved { .. } | Stimulus::ReObserveFailed => Err(Misfit::NotReObserving),
        },
        Phase::Done(_) | Phase::Refused(_) | Phase::Unknown(_) => Err(Misfit::Settled),
    }
}

/// One directive of the session's table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The directive as first received; a repeat is answered from it, whatever it carries.
    pub directive: Directive,
    /// Its phase.
    pub phase: Phase,
}

/// An answer to send: the caller adds the observation id and its clock reading.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    /// The open session's epoch (ADR 0042: a stale frame's answer too).
    pub epoch: Epoch,
    /// The directive it answers.
    pub answers: DirectiveRef,
    /// What it says.
    pub kind: ObservationKind,
}

/// Why a frame was discarded unanswered (the relay re-sends under the current session).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Discard {
    /// No session is open, or the frame is of one not entered yet.
    NotEntered,
    /// Its version is not the session's.
    Version(VersionRefusal),
}

/// What to do with a received frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Receipt {
    /// Drop it, unanswered.
    Discard(Discard),
    /// Answer now: a stale frame, a repeat of a settled directive, or a refusal.
    Reply(Box<Reply>),
    /// A repeat of a directive still executing: its answer follows when it settles.
    Pending,
    /// Re-observe the target's instance id, then report [`DataPlane::reobserved`].
    ReObserve,
    /// Run the effect, then report [`DataPlane::finish`].
    Execute,
}

/// What a re-observation or an effect result led to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Progress {
    /// The proof matched: run the effect.
    Execute,
    /// Answer.
    Reply(Box<Reply>),
    /// The report does not fit the directive's phase (a caller fault): nothing changed.
    Misfit(Misfit),
    /// The session it belonged to ended; its outcome will not be recorded or answered.
    Orphaned,
}

/// The data plane's executor state: its host, its floor, its session and its ledger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataPlane {
    /// The host it runs on.
    pub host_id: HostId,
    /// The epoch floor read from `TART_HOME` at start, raised by each session entered.
    pub floor: Epoch,
    /// The session.
    pub link: Link,
    /// The resource ledger.
    pub ledger: Ledger,
}

impl DataPlane {
    /// A data plane after a start: no session, the persisted floor, a ledger to rebuild.
    #[must_use]
    pub const fn start(host_id: HostId, floor: Epoch, ledger: Ledger) -> Self {
        Self {
            host_id,
            floor,
            link: Link::Idle,
            ledger,
        }
    }

    /// Applies a session stimulus ([`link::step`]). A hello ends the open session: its
    /// reservations are held, since no outcome of it will be recorded. A floor report opens
    /// the session and moves the floor.
    pub fn link(&mut self, stimulus: LinkStimulus) -> LinkOutcome {
        let (next, outcome) = link::step(&self.link, self.floor, stimulus);
        match outcome {
            LinkOutcome::SendHello => self.ledger.hold_all(),
            LinkOutcome::Entered(epoch) => self.floor = epoch,
            LinkOutcome::RaiseFloor(_) | LinkOutcome::Ignored(_) => {}
        }
        self.link = next;
        outcome
    }

    /// The open session's epoch.
    #[must_use]
    pub const fn session_epoch(&self) -> Option<Epoch> {
        match &self.link {
            Link::Open(session) => Some(session.epoch),
            Link::Idle | Link::Awaiting | Link::Entering(_) => None,
        }
    }

    /// Takes one directive frame written in `version` at `now` (a reading of the clock its
    /// deadline is in). Pinned by the tables of
    /// `tests/data_plane/matrix.rs::should_fence_every_frame_when_it_meets_each_session_state_and_table_entry`
    /// and `should_admit_or_refuse_every_kind_when_it_meets_each_condition`.
    pub fn receive(
        &mut self,
        version: ProtocolVersion,
        directive: &Directive,
        now: Millis,
    ) -> Receipt {
        let session = match &mut self.link {
            Link::Open(session) => session,
            Link::Idle | Link::Awaiting | Link::Entering(_) => {
                return Receipt::Discard(Discard::NotEntered);
            }
        };
        if let Err(refusal) = admit_frame(Some(session.version), version, SUPPORTED) {
            return Receipt::Discard(Discard::Version(refusal));
        }
        let answers = DirectiveRef {
            directive_id: directive.directive_id,
            attempt: directive.attempt,
        };
        match check_epoch(Some(session.epoch), directive.epoch) {
            EpochCheck::NotEntered => return Receipt::Discard(Discard::NotEntered),
            EpochCheck::Stale => {
                return Receipt::Reply(Box::new(Reply {
                    epoch: session.epoch,
                    answers,
                    kind: ObservationKind::OutcomeUnknown {
                        reason: UnknownReason::StaleEpoch,
                    },
                }));
            }
            EpochCheck::Current => {}
        }
        if let Some(entry) = session.table.get(&directive.directive_id) {
            let first = DirectiveRef {
                directive_id: entry.directive.directive_id,
                attempt: entry.directive.attempt,
            };
            return match answer(&entry.directive, &entry.phase) {
                Some(kind) => Receipt::Reply(Box::new(Reply {
                    epoch: session.epoch,
                    answers: first,
                    kind,
                })),
                None => Receipt::Pending,
            };
        }
        let key = (session.epoch, directive.directive_id);
        let admitted = admission(session.mode, &self.host_id, &self.ledger, directive, now)
            .and_then(|stage| {
                if directive.kind.needs_resource_fit() {
                    self.ledger
                        .reserve(key, &directive.kind)
                        .map_err(|dimension| RefusalReason::InsufficientResources { dimension })?;
                }
                Ok(stage)
            });
        let stimulus = match admitted {
            Ok(stage) => Stimulus::Admit(stage),
            Err(reason) => Stimulus::Deny(reason),
        };
        let phase =
            step(&Phase::Received, stimulus).unwrap_or(Phase::Refused(RefusalReason::EffectFailed));
        let receipt = match answer(directive, &phase) {
            Some(kind) => Receipt::Reply(Box::new(Reply {
                epoch: session.epoch,
                answers,
                kind,
            })),
            None if proof_of(&directive.kind).is_some() => Receipt::ReObserve,
            None => Receipt::Execute,
        };
        session.table.insert(
            directive.directive_id,
            Entry {
                directive: directive.clone(),
                phase,
            },
        );
        receipt
    }

    /// The target of the directive `id` admitted under `epoch` was re-observed with
    /// `observed` as its instance id (`None`: not there). Only the proof's exact instance id
    /// lets the effect run (ADR 0026); anything else is `ProofMismatch`. Pinned by
    /// `tests/data_plane/executor.rs::should_refuse_proof_mismatch_when_the_instance_id_changed`.
    pub fn reobserved(
        &mut self,
        epoch: Epoch,
        id: DirectiveId,
        observed: Option<&ProviderInstanceId>,
    ) -> Progress {
        self.advance(epoch, id, |entry| {
            Ok(Stimulus::ReObserved {
                matches: proof_of(&entry.directive.kind)
                    .is_some_and(|proof| observed == Some(&proof.instance_id)),
            })
        })
    }

    /// The target of `id` could not be re-observed: nothing was done.
    pub fn reobserve_failed(&mut self, epoch: Epoch, id: DirectiveId) -> Progress {
        self.advance(epoch, id, |_| Ok(Stimulus::ReObserveFailed))
    }

    /// The effect of `id` under `epoch` ended with `result` ([`Stimulus::Applied`],
    /// [`Stimulus::Failed`] or [`Stimulus::Ambiguous`]). The ledger follows: an applied effect
    /// is recorded, a refused one released, an unknown one held. An applied observation must
    /// answer the directive's kind.
    pub fn finish(&mut self, epoch: Epoch, id: DirectiveId, result: Stimulus) -> Progress {
        self.advance(epoch, id, |entry| match &result {
            Stimulus::Applied(kind) if !answered_by(entry.directive.kind.tag(), kind) => {
                Err(Misfit::NotAnApplication)
            }
            Stimulus::Applied(_)
            | Stimulus::Admit(_)
            | Stimulus::Deny(_)
            | Stimulus::ReObserved { .. }
            | Stimulus::ReObserveFailed
            | Stimulus::Failed
            | Stimulus::Ambiguous(_) => Ok(result),
        })
    }

    fn advance(
        &mut self,
        epoch: Epoch,
        id: DirectiveId,
        stimulus: impl FnOnce(&Entry) -> Result<Stimulus, Misfit>,
    ) -> Progress {
        let session = match &mut self.link {
            Link::Open(session) if session.epoch == epoch => session,
            Link::Open(_) | Link::Idle | Link::Awaiting | Link::Entering(_) => {
                return Progress::Orphaned;
            }
        };
        let Some(entry) = session.table.get_mut(&id) else {
            return Progress::Orphaned;
        };
        let next = match stimulus(entry).and_then(|stimulus| step(&entry.phase, stimulus)) {
            Ok(next) => next,
            Err(misfit) => return Progress::Misfit(misfit),
        };
        let key = (epoch, id);
        match &next {
            Phase::Done(_) => self.ledger.commit(key, &entry.directive.kind),
            Phase::Refused(_) => self.ledger.release(key),
            Phase::Unknown(_) => self.ledger.hold(key),
            Phase::Received | Phase::Executing(_) => {}
        }
        entry.phase = next;
        let answers = DirectiveRef {
            directive_id: entry.directive.directive_id,
            attempt: entry.directive.attempt,
        };
        match answer(&entry.directive, &entry.phase) {
            Some(kind) => Progress::Reply(Box::new(Reply {
                epoch,
                answers,
                kind,
            })),
            None => Progress::Execute,
        }
    }
}

/// The checks a received directive passes before it executes, in order: safe mode holds the
/// effect kinds (I29); a directive past its deadline does not start; strings are bounded; a VM
/// directive names a VM of this host, a destructive one never an image base (ADR 0026); a
/// proof is consistent with its own name and the directive's attempt; and the ledger has room
/// (I33). Pure: the ledger is read, not changed. Pinned by
/// `tests/data_plane/matrix.rs::should_admit_or_refuse_every_kind_when_it_meets_each_condition`.
///
/// # Errors
/// The first [`RefusalReason`] that applies.
pub fn admission(
    mode: Mode,
    host: &HostId,
    ledger: &Ledger,
    directive: &Directive,
    now: Millis,
) -> Result<Stage, RefusalReason> {
    let kind = &directive.kind;
    match mode {
        Mode::Safe if kind.held_in_safe_mode() => return Err(RefusalReason::SafeMode),
        Mode::Safe | Mode::Normal => {}
    }
    if directive.deadline.is_expired(now) {
        return Err(RefusalReason::DeadlineExpired);
    }
    if !well_formed(kind) {
        return Err(RefusalReason::EffectFailed);
    }
    if let Some(vm) = kind.vm_name()
        && (vm.host() != host || ledger.is_base(&vm.to_string()))
    {
        return Err(RefusalReason::ProtectedTarget);
    }
    if let Some(proof) = proof_of(kind) {
        if proof.host_id != *host {
            return Err(RefusalReason::ProtectedTarget);
        }
        if !consistent(proof, directive) {
            return Err(RefusalReason::ProofMismatch);
        }
        return Ok(Stage::ReObserve);
    }
    if kind.needs_resource_fit() {
        ledger
            .fits(ledger.demand(kind))
            .map_err(|dimension| RefusalReason::InsufficientResources { dimension })?;
    }
    Ok(Stage::Effect)
}

/// Whether the strings a directive carries are bounded and inert: the only free text the
/// control plane sends is a runner version, which reaches argv and file names.
fn well_formed(kind: &DirectiveKind) -> bool {
    match kind {
        DirectiveKind::MaterialiseImage { runner_version, .. } => {
            (1..=MAX_RUNNER_VERSION).contains(&runner_version.len())
                && runner_version
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b".-+_".contains(&b))
        }
        DirectiveKind::PullImage { .. }
        | DirectiveKind::EvictImage { .. }
        | DirectiveKind::CloneVm { .. }
        | DirectiveKind::StartVm { .. }
        | DirectiveKind::VerifyGuest { .. }
        | DirectiveKind::LaunchRunner { .. }
        | DirectiveKind::ExportDiagnostics { .. }
        | DirectiveKind::StopVm { .. }
        | DirectiveKind::DeleteVm { .. }
        | DirectiveKind::ReportInventory
        | DirectiveKind::ReportCapacity
        | DirectiveKind::ReportHostFacts => true,
    }
}

/// Whether a proof agrees with its own VM name and with the directive's attempt: the name's
/// slot, generation and attempt short are the proof's, and so is the directive's attempt.
fn consistent(proof: &OwnershipProof, directive: &Directive) -> bool {
    let name = &proof.vm_name;
    name.slot() == proof.slot
        && name.is_for(&proof.attempt_id, proof.generation)
        && directive.attempt.is_none_or(|attempt| {
            attempt.attempt_id == proof.attempt_id && attempt.generation == proof.generation
        })
}

/// The proof a destructive directive carries.
#[must_use]
pub const fn proof_of(kind: &DirectiveKind) -> Option<&OwnershipProof> {
    match kind {
        DirectiveKind::StopVm { proof, .. } | DirectiveKind::DeleteVm { proof } => Some(proof),
        DirectiveKind::PullImage { .. }
        | DirectiveKind::MaterialiseImage { .. }
        | DirectiveKind::EvictImage { .. }
        | DirectiveKind::CloneVm { .. }
        | DirectiveKind::StartVm { .. }
        | DirectiveKind::VerifyGuest { .. }
        | DirectiveKind::LaunchRunner { .. }
        | DirectiveKind::ExportDiagnostics { .. }
        | DirectiveKind::ReportInventory
        | DirectiveKind::ReportCapacity
        | DirectiveKind::ReportHostFacts => None,
    }
}

/// The directive kind an observation answers when it is applied, or `None` for one that never
/// answers an application. Pinned by
/// `tests/data_plane/executor.rs::should_accept_only_its_own_answer_when_an_effect_is_applied`.
#[must_use]
pub const fn answered_kind(kind: &ObservationKind) -> Option<DirectiveTag> {
    match kind {
        ObservationKind::VmCloned { .. } => Some(DirectiveTag::CloneVm),
        ObservationKind::VmBooted => Some(DirectiveTag::StartVm),
        ObservationKind::GuestVerified { .. } | ObservationKind::GuestRejected { .. } => {
            Some(DirectiveTag::VerifyGuest)
        }
        ObservationKind::RunnerLaunched => Some(DirectiveTag::LaunchRunner),
        ObservationKind::DiagnosticsExported { .. } | ObservationKind::DiagnosticsLost { .. } => {
            Some(DirectiveTag::ExportDiagnostics)
        }
        ObservationKind::VmStopped => Some(DirectiveTag::StopVm),
        ObservationKind::VmDeleted => Some(DirectiveTag::DeleteVm),
        ObservationKind::Inventory { .. } | ObservationKind::InventoryIncomplete { .. } => {
            Some(DirectiveTag::ReportInventory)
        }
        ObservationKind::CapacityReported(_) => Some(DirectiveTag::ReportCapacity),
        ObservationKind::HostFacts(_) => Some(DirectiveTag::ReportHostFacts),
        ObservationKind::ImagePulled { .. } => Some(DirectiveTag::PullImage),
        ObservationKind::ImageMaterialised { .. } => Some(DirectiveTag::MaterialiseImage),
        ObservationKind::ImageEvicted { .. } => Some(DirectiveTag::EvictImage),
        ObservationKind::RunnerLaunchUnknown
        | ObservationKind::RunnerExited { .. }
        | ObservationKind::VmObservedStopped { .. }
        | ObservationKind::HypervisorLimitHit
        | ObservationKind::DirectiveRefused { .. }
        | ObservationKind::OutcomeUnknown { .. } => None,
    }
}

fn answered_by(tag: DirectiveTag, kind: &ObservationKind) -> bool {
    answered_kind(kind) == Some(tag)
}

/// The answer a settled phase gives; `None` while it is not settled. An unknown launch is
/// `RunnerLaunchUnknown` (the JIT config is then presumed leaked, I22).
fn answer(directive: &Directive, phase: &Phase) -> Option<ObservationKind> {
    match phase {
        Phase::Done(kind) => Some(kind.clone()),
        Phase::Refused(reason) => Some(ObservationKind::DirectiveRefused { reason: *reason }),
        Phase::Unknown(reason) => Some(match directive.kind.tag() {
            DirectiveTag::LaunchRunner => ObservationKind::RunnerLaunchUnknown,
            DirectiveTag::PullImage
            | DirectiveTag::MaterialiseImage
            | DirectiveTag::EvictImage
            | DirectiveTag::CloneVm
            | DirectiveTag::StartVm
            | DirectiveTag::VerifyGuest
            | DirectiveTag::ExportDiagnostics
            | DirectiveTag::StopVm
            | DirectiveTag::DeleteVm
            | DirectiveTag::ReportInventory
            | DirectiveTag::ReportCapacity
            | DirectiveTag::ReportHostFacts => ObservationKind::OutcomeUnknown { reason: *reason },
        }),
        Phase::Received | Phase::Executing(_) => None,
    }
}
