//! The directive executor (plan §3.1, §6, §8.4; ADRs 0024, 0026, 0042; `formal/link.qnt`'s
//! `dpDeliver`).
//!
//! Each directive of the open session moves `Received → Executing → Done | Refused | Unknown`
//! ([`step`]); `Executing` re-observes the target first for a proof-carrying `StopVm` or
//! `DeleteVm`, which are idempotent by outcome (ADR 0053): a delete whose target a complete
//! listing shows absent, and a stop whose proven target is stopped, are `Done` without an
//! effect; a re-observation that fails is `Unknown{listing_incomplete}`, never `Done` (I25). [`DataPlane::receive`] takes a frame and checks, in this order, the frame's
//! version ([`mayfly_protocol::admit_frame`]), its epoch ([`mayfly_protocol::check_epoch`]: a
//! frame of a session not entered is discarded, one of an older session answered
//! `OutcomeUnknown{stale_epoch}` and never executed), the outcome table (a repeated id answers
//! the recorded outcome), then [`admission`]: safe mode (I29), the deadline, bounded strings,
//! the target (another host's VM), the proof's own consistency, and the resource ledger (I33).
//! Nothing the control plane allocated overrides a check. An image base is never a VM
//! directive's target by type: a VM directive names an [`OwnedVmName`] (`mf-`), which a base
//! name (`mfb-`) never parses as (I05).

use mayfly_domain::{DirectiveId, Epoch, HostId, Millis, OwnedVmName, ProviderInstanceId};
use mayfly_protocol::{
    Directive, DirectiveKind, DirectiveOutcome, DirectiveRef, DirectiveTag, EpochCheck, Mode,
    ObservationKind, OwnershipProof, ProtocolVersion, RefusalReason, SUPPORTED, UnknownReason,
    VersionRefusal, admit_frame, check_epoch,
};

use crate::ledger::Ledger;
use crate::link::{self, Link, LinkOutcome, LinkStimulus};

/// The longest runner version a `MaterialiseImage` may carry: it reaches argv and file names.
pub const MAX_RUNNER_VERSION: usize = 64;

/// What a re-observation found at a destructive directive's target, right before its effect
/// (ADR 0026): the listed VM's parsed name (its generation included), its instance id and
/// whether it runs (ADR 0053: a proven target already stopped answers a `StopVm` `Done`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reobservation {
    /// The listed name, parsed.
    pub vm_name: OwnedVmName,
    /// The provider's instance id of that VM.
    pub instance_id: ProviderInstanceId,
    /// Whether the listing shows it running.
    pub running: bool,
}

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
    /// The target could not be re-observed (the listing failed, did not parse or did not
    /// finish): nothing was done now, and what an earlier delivery did is not known.
    ReObserveFailed,
    /// The re-observation shows the directive's outcome already holds (ADR 0053): a `DeleteVm`
    /// whose target a complete listing shows absent (`VmDeleted`), a `StopVm` whose proven
    /// target is stopped (`VmStopped`). Answered as applied, without running the effect.
    Satisfied(ObservationKind),
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
    /// An `Applied` or `Satisfied` whose observation is a refusal, an unknown outcome, a fact
    /// that answers nothing or an answer to another kind.
    NotAnApplication,
    /// An effect cannot end `Unknown{stale_epoch}` or `Unknown{listing_incomplete}`: only
    /// fencing and a failed re-observation answer those.
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
            Stimulus::ReObserved { .. } | Stimulus::ReObserveFailed | Stimulus::Satisfied(_) => {
                Err(Misfit::NotReObserving)
            }
            Stimulus::Applied(_) | Stimulus::Failed | Stimulus::Ambiguous(_) => {
                Err(Misfit::NotRunning)
            }
        },
        Phase::Executing(Stage::ReObserve) => match stimulus {
            Stimulus::ReObserved { matches: true } => Ok(Phase::Executing(Stage::Effect)),
            Stimulus::ReObserved { matches: false } => {
                Ok(Phase::Refused(RefusalReason::ProofMismatch))
            }
            Stimulus::ReObserveFailed => Ok(Phase::Unknown(UnknownReason::ListingIncomplete)),
            Stimulus::Satisfied(kind) => applied(kind),
            Stimulus::Admit(_) | Stimulus::Deny(_) => Err(Misfit::NotReceived),
            Stimulus::Applied(_) | Stimulus::Failed | Stimulus::Ambiguous(_) => {
                Err(Misfit::NotRunning)
            }
        },
        Phase::Executing(Stage::Effect) => match stimulus {
            Stimulus::Applied(kind) => applied(kind),
            Stimulus::Failed => Ok(Phase::Refused(RefusalReason::EffectFailed)),
            Stimulus::Ambiguous(reason) => match reason {
                UnknownReason::Deadline | UnknownReason::ProcessLost | UnknownReason::HostSlept => {
                    Ok(Phase::Unknown(reason))
                }
                UnknownReason::StaleEpoch | UnknownReason::ListingIncomplete => {
                    Err(Misfit::NotAnEffectOutcome)
                }
            },
            Stimulus::Admit(_) | Stimulus::Deny(_) => Err(Misfit::NotReceived),
            Stimulus::ReObserved { .. } | Stimulus::ReObserveFailed | Stimulus::Satisfied(_) => {
                Err(Misfit::NotReObserving)
            }
        },
        Phase::Done(_) | Phase::Refused(_) | Phase::Unknown(_) => Err(Misfit::Settled),
    }
}

/// `Done` with an observation that says the effect was applied; anything else is a misfit.
fn applied(kind: ObservationKind) -> Result<Phase, Misfit> {
    match kind.outcome() {
        Some(DirectiveOutcome::Applied) => Ok(Phase::Done(kind)),
        Some(DirectiveOutcome::Refused(_) | DirectiveOutcome::Unknown) | None => {
            Err(Misfit::NotAnApplication)
        }
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
            return match answer(&entry.directive, &entry.phase, session.version) {
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
                    self.ledger.reserve(key, &directive.kind)?;
                }
                Ok(stage)
            });
        let stimulus = match admitted {
            Ok(stage) => Stimulus::Admit(stage),
            Err(reason) => Stimulus::Deny(reason),
        };
        let phase =
            step(&Phase::Received, stimulus).unwrap_or(Phase::Refused(RefusalReason::EffectFailed));
        let receipt = match answer(directive, &phase, session.version) {
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

    /// The target of the directive `id` admitted under `epoch` was re-observed by a **complete**
    /// listing as `observed` (`None`: the complete listing does not list it; an incomplete
    /// listing is [`DataPlane::reobserve_failed`], never `None`, I25). Only the proof's exact
    /// name, generation and instance id, all three, let the effect run (ADR 0026); a present
    /// target that differs in any is `ProofMismatch` and is never touched. Idempotent by
    /// outcome (ADR 0053): a `DeleteVm` whose target is absent is `Done(VmDeleted)`, whether
    /// this directive, an earlier one or someone else removed it, and a `StopVm` whose proven
    /// target is stopped is `Done(VmStopped)`; a `StopVm` whose target is absent is still
    /// `ProofMismatch` (absence is the inventory's fact, not a stop's). Pinned by
    /// `tests/data_plane/executor.rs::should_refuse_proof_mismatch_when_the_reobserved_name_generation_or_instance_id_differs`,
    /// `should_answer_done_when_a_delete_is_resent_and_a_complete_listing_shows_the_target_absent`,
    /// `should_answer_done_when_a_stop_finds_its_proven_target_stopped` and
    /// `should_refuse_proof_mismatch_when_a_stop_finds_its_target_gone`.
    pub fn reobserved(
        &mut self,
        epoch: Epoch,
        id: DirectiveId,
        observed: Option<&Reobservation>,
    ) -> Progress {
        self.advance(epoch, id, |entry| {
            let kind = &entry.directive.kind;
            let matches = proof_of(kind)
                .zip(observed)
                .is_some_and(|(proof, seen)| proven(proof, seen));
            let stopped = observed.is_some_and(|seen| !seen.running);
            Ok(match kind {
                DirectiveKind::DeleteVm { .. } if observed.is_none() => {
                    Stimulus::Satisfied(ObservationKind::VmDeleted)
                }
                DirectiveKind::StopVm { .. } if matches && stopped => {
                    Stimulus::Satisfied(ObservationKind::VmStopped)
                }
                DirectiveKind::DeleteVm { .. }
                | DirectiveKind::StopVm { .. }
                | DirectiveKind::PullImage { .. }
                | DirectiveKind::MaterialiseImage { .. }
                | DirectiveKind::EvictImage { .. }
                | DirectiveKind::CloneVm { .. }
                | DirectiveKind::StartVm { .. }
                | DirectiveKind::VerifyGuest { .. }
                | DirectiveKind::LaunchRunner { .. }
                | DirectiveKind::ExportDiagnostics { .. }
                | DirectiveKind::ReportInventory
                | DirectiveKind::ReportCapacity
                | DirectiveKind::ReportHostFacts => Stimulus::ReObserved { matches },
            })
        })
    }

    /// The target of `id` could not be re-observed (the listing was incomplete): nothing was
    /// done now, and whether an earlier delivery took effect is not known, so the outcome is
    /// `Unknown{listing_incomplete}`, never `Done` (I25, ADR 0053). Pinned by
    /// `tests/data_plane/executor.rs::should_answer_unknown_never_done_when_a_delete_reobservation_listing_is_incomplete`.
    pub fn reobserve_failed(&mut self, epoch: Epoch, id: DirectiveId) -> Progress {
        self.advance(epoch, id, |_| Ok(Stimulus::ReObserveFailed))
    }

    /// The effect of `id` under `epoch` ended with `result` ([`Stimulus::Applied`],
    /// [`Stimulus::Failed`] or [`Stimulus::Ambiguous`]). The ledger follows: an applied effect
    /// is recorded, a refused one released, an unknown one held. An applied observation must
    /// answer the directive's kind.
    pub fn finish(&mut self, epoch: Epoch, id: DirectiveId, result: Stimulus) -> Progress {
        self.advance(epoch, id, |entry| match &result {
            Stimulus::Applied(kind) | Stimulus::Satisfied(kind)
                if !answered_by(entry.directive.kind.tag(), kind) =>
            {
                Err(Misfit::NotAnApplication)
            }
            Stimulus::Applied(_)
            | Stimulus::Satisfied(_)
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
        let version = session.version;
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
        match answer(&entry.directive, &entry.phase, version) {
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
/// effect kinds (I29); a directive past its deadline does not start; strings are bounded (a
/// malformed runner version is refused `effect_failed`); a VM directive names a VM of this
/// host, else `protected_target` (ADR 0026; an image base it cannot name, see the module doc);
/// a proof is consistent with its own name and the directive's attempt; and the ledger has
/// room and knows it has (I33, ADR 0052). Pure: the ledger is read, not changed. Pinned by
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
        && vm.host() != host
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
        ledger.admits(kind)?;
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
/// A directive without an attempt (`directive.attempt` is `None`: the protocol documents that
/// for image and host directives only, but its type does not forbid it on a `StopVm` or
/// `DeleteVm`) skips the attempt check: the proof is then checked against its own name only,
/// and the re-observation ([`proven`]) still requires the exact name, generation and instance
/// id. Pinned by
/// `tests/data_plane/executor.rs::should_check_the_proof_against_its_name_only_when_the_directive_has_no_attempt`.
fn consistent(proof: &OwnershipProof, directive: &Directive) -> bool {
    let name = &proof.vm_name;
    name.slot() == proof.slot
        && name.is_for(&proof.attempt_id, proof.generation)
        && directive.attempt.is_none_or(|attempt| {
            attempt.attempt_id == proof.attempt_id && attempt.generation == proof.generation
        })
}

/// Whether a re-observed target is the proof's: the same name, the same generation and the
/// same instance id (ADR 0026). The generation is part of the name; it is compared on its own
/// too so that the rule reads as the ADR states it.
fn proven(proof: &OwnershipProof, seen: &Reobservation) -> bool {
    seen.vm_name == proof.vm_name
        && seen.vm_name.generation() == proof.generation
        && seen.instance_id == proof.instance_id
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

/// The answer a settled phase gives, as a frame of the session's `version` may carry it
/// ([`ObservationKind::written_in`]); `None` while it is not settled. An unknown launch is
/// `RunnerLaunchUnknown` (the JIT config is then presumed leaked, I22).
fn answer(
    directive: &Directive,
    phase: &Phase,
    version: ProtocolVersion,
) -> Option<ObservationKind> {
    let kind = match phase {
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
    };
    kind.map(|kind| kind.written_in(version))
}
