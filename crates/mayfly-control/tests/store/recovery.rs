//! Startup recovery (P1, plan §10) over the store: the handshake, each row of the recovery
//! table as the journal records it, safe mode (F35), a zombie writer of an older run (F43), and
//! the projections prepared before any writer.

use std::collections::BTreeMap;

use mayfly_control::inbox::Inbox;
use mayfly_control::projection::ProjectionStore;
use mayfly_control::queries;
use mayfly_control::recovery::{Recovered, Recovery, StartupError};
use mayfly_control::saga::{AttemptSaga, SagaConfig};
use mayfly_control::sessions::HostSession;
use mayfly_control::store::{AggregateStore, NoNotifier, Outcome, SaveRequest};
use mayfly_control_domain::attempt::{
    AttemptCommand, AttemptEvent, AttemptRejection, DirectiveKind, DirectiveTag, Fact,
    Observation as AttemptObservation, Outcome as PendingOutcome, Phase, QuarantineReason,
    RetireReason, SagaStep, Stage, next_step,
};
use mayfly_control_domain::host::{AdmissionCause, Host, HostCommand, HostRejection};
use mayfly_control_domain::operations::hostrun::{
    HostRun, HostRunCommand, HostRunRejection, RunPhase, SafeModeReason, Verification,
};
use mayfly_control_domain::operations::recovery::{
    Adoption, Decision, GithubView, Proposal, SafeModeCause,
};
use mayfly_control_domain::{Actor, Command, ControlAggregates};
use mayfly_domain::{CommandId, Epoch, Generation, Millis, ObservationId, Sample as _};
use mayfly_protocol::{CURRENT, Mode, VmObservation};
use mayfly_testkit::block_on;
use mayfly_testkit::clock::{FakeClock, SeededRng};
use mayfly_testkit::fleet::GIB;

use crate::counter::{CounterBody, NewerCounter, NewerCounters, command as counter_command};
use crate::recovery_kit::{
    BEFORE, Kit, NOW, Responder, Structure, attempt, build, hello, host, incomplete, instance,
    inventory, kit, listed, named, other_instance, path, projections,
};

const RUN: Epoch = Epoch::new(2);

/// Attempt 1 driven along `path`, then a restart that reads `vms` and `github` for it.
fn row(
    path: Vec<AttemptCommand>,
    vms: Vec<VmObservation>,
    github: Option<GithubView>,
) -> (Kit, Recovered) {
    let kit = kit();
    kit.previous_run();
    kit.drive(1, path);
    let (opened, _) = kit.open(1);
    assert_eq!(opened.epoch, RUN);
    let views: BTreeMap<_, _> = github.map(|g| (attempt(1), g)).into_iter().collect();
    let saga = AttemptSaga::new(&kit.store, kit.sessions.clone(), SagaConfig::default());
    let recovery = block_on(
        kit.startup(false)
            .recover(&inventory(0x2, RUN, vms), &views, &saga),
    )
    .unwrap_or_else(|e| panic!("{e}"));
    let Recovery::Completed(recovered) = recovery else {
        panic!("not completed: {recovery:?}");
    };
    (kit, recovered)
}

fn decision(recovered: &Recovered) -> Decision {
    recovered
        .decisions
        .iter()
        .find(|(id, _)| *id == attempt(1))
        .map(|(_, d)| d.clone())
        .unwrap_or_else(|| panic!("attempt 1 not classified"))
}

/// Whether a delete was issued after the restart (the one before it was pending already).
fn deletes_again(kit: &Kit) -> bool {
    kit.events(1)
        .iter()
        .filter(|e| {
            matches!(e, AttemptEvent::DirectiveIssued(d) if d.kind.tag() == DirectiveTag::DeleteVm)
        })
        .count()
        > 1
}

fn deletes(kit: &Kit) -> bool {
    kit.events(1).iter().any(
        |e| matches!(e, AttemptEvent::DirectiveIssued(d) if d.kind.tag() == DirectiveTag::DeleteVm),
    )
}

#[test]
fn should_welcome_with_the_minted_epoch_and_the_clock_when_a_run_starts() {
    // given
    let kit = kit();

    // when
    let (opened, link) = kit.open(4);

    // then
    let welcomes = link.welcomes();
    assert_eq!(welcomes.len(), 1);
    assert_eq!(welcomes[0].epoch, Epoch::new(5), "above the floor (I17)");
    assert_eq!(
        welcomes[0].cp_clock,
        Some(NOW),
        "the Clock port's reading (ADR 0020)"
    );
    assert_eq!(welcomes[0].accepted_version, CURRENT);
    assert_eq!(opened.epoch, Epoch::new(5));
    assert_eq!(
        kit.sessions.get(&host()),
        Some(HostSession {
            epoch: Epoch::new(5),
            mode: welcomes[0].mode,
        })
    );
    assert!(
        kit.host_state()
            .gate
            .closed
            .contains(&AdmissionCause::Recovering),
        "admission closed until recovery completes (P3)"
    );
}

#[test]
fn should_refuse_the_hello_when_the_data_plane_shares_no_major() {
    // given
    let kit = kit();
    let mut offered = hello(0);
    offered.supported = vec![mayfly_protocol::ProtocolVersion::new(99, 0)];
    let mut link = Responder::new(offered);

    // when
    let opened = block_on(kit.startup(false).open(
        &mut link,
        &Structure(true),
        (&kit.memory, &projections(), &build()),
    ));

    // then
    assert_eq!(opened, Err(StartupError::Version));
    assert!(link.welcomes().is_empty());
    assert_eq!(link.refused.lock().map(|r| r.len()).unwrap_or(0), 1);
}

#[test]
fn should_prepare_the_projections_before_any_writer_when_a_run_starts() {
    // given an attempt written by a store keeping no projection (an older build)
    let kit = kit();
    let bare = AggregateStore::new::<ControlAggregates>(
        kit.memory.clone(),
        FakeClock::at(NOW),
        SeededRng::new(9),
        NoNotifier,
    )
    .unwrap_or_else(|e| panic!("{e}"));
    let command = Command::new(
        CommandId::from_u128(0xB0),
        NOW,
        BEFORE,
        Actor::Automation { policy: "start" },
        mayfly_testkit::attempts::start(1),
    );
    let _ = block_on(
        bare.execute::<mayfly_control_domain::attempt::Attempt>(SaveRequest::new(
            attempt(1),
            command,
        )),
    )
    .unwrap_or_else(|e| panic!("{e}"));
    let prepared = |k: &Kit| {
        block_on(ProjectionStore::meta(&k.memory))
            .unwrap_or_else(|e| panic!("{e}"))
            .into_iter()
            .any(|m| m.spec.as_str() == "attempts" && m.written_by == build())
    };
    assert!(!prepared(&kit));

    // when
    let _ = kit.open(0);

    // then
    assert!(
        prepared(&kit),
        "the attempts projection was rebuilt for this build"
    );
}

#[test]
fn should_record_a_crash_loop_as_safe_mode_when_the_watchdog_found_one() {
    // given
    let kit = kit();
    let mut link = Responder::new(hello(0));

    // when
    let opened = block_on(kit.startup(true).open(
        &mut link,
        &Structure(true),
        (&kit.memory, &projections(), &build()),
    ))
    .unwrap_or_else(|e| panic!("{e}"));

    // then
    assert_eq!(opened.mode, Mode::Safe);
    assert_eq!(
        kit.hostrun().safe_mode.map(|s| s.reason),
        Some(SafeModeReason::CrashLoop)
    );
}

// ---- rows of the recovery table (plan §10) ---------------------------------------------------

#[test]
fn should_adopt_and_record_the_clone_when_a_cloning_attempt_finds_its_owned_clone() {
    // given the attempt's path and what the restart lists
    // when it recovers
    let (kit, recovered) = row(
        path::unknown_clone(1),
        vec![listed(1, instance(), false)],
        None,
    );

    // then
    assert_eq!(
        decision(&recovered),
        Decision::Adopt(Adoption::Clone(instance()))
    );
    let state = kit.state(1);
    assert_eq!(state.epoch, RUN);
    assert_eq!(
        state.instance_id,
        Some(instance()),
        "resolved by the observation (I10)"
    );
    assert_eq!(state.pending, None);
    assert_eq!(recovered.adopted, vec![attempt(1)]);
}

#[test]
fn should_retry_by_the_same_name_when_a_cloning_attempt_finds_no_clone() {
    // given the attempt's path and what the restart lists
    // when it recovers
    let (kit, recovered) = row(path::unknown_clone(1), Vec::new(), None);

    // then
    assert_eq!(decision(&recovered), Decision::RetryClone);
    let state = kit.state(1);
    assert_eq!(
        state.pending, None,
        "the Unknown clone resolved as not done"
    );
    assert_eq!(state.instance_id, None);
    assert!(matches!(
        state.phase,
        Phase::Backoff { .. } | Phase::Reserved
    ));
    assert_eq!(
        state.identity.map(|i| i.vm_name),
        Some(crate::recovery_kit::vm_name(1)),
        "the same name and generation"
    );
}

#[test]
fn should_decide_and_release_nothing_when_the_inventory_is_incomplete() {
    // given
    let kit = kit();
    kit.previous_run();
    kit.drive(1, path::prepared(1));
    let before = kit.events(1).len();
    let _ = kit.open(1);
    let saga = AttemptSaga::new(&kit.store, kit.sessions.clone(), SagaConfig::default());

    // when
    let recovery = block_on(kit.startup(false).recover(
        &incomplete(0x2, RUN),
        &BTreeMap::new(),
        &saga,
    ))
    .unwrap_or_else(|e| panic!("{e}"));

    // then
    assert_eq!(
        recovery,
        Recovery::Waiting {
            waiting: vec![attempt(1)],
        }
    );
    assert_eq!(kit.events(1).len(), before, "nothing recorded (I25)");
    assert_eq!(kit.hostrun().phase, RunPhase::Recovering);
    assert!(
        kit.host_state()
            .gate
            .closed
            .contains(&AdmissionCause::Recovering)
    );
}

#[test]
fn should_quarantine_and_never_delete_when_the_instance_id_differs() {
    // given the attempt's path and what the restart lists
    // when it recovers
    let (kit, recovered) = row(
        path::prepared(1),
        vec![listed(1, other_instance(), true)],
        None,
    );

    // then
    assert_eq!(
        decision(&recovered),
        Decision::Quarantine(QuarantineReason::ProviderIdentityMismatch)
    );
    assert!(matches!(
        kit.state(1).phase,
        Phase::Quarantined {
            reason: QuarantineReason::ProviderIdentityMismatch,
            ..
        }
    ));
    assert!(!deletes(&kit));
}

#[test]
fn should_adopt_for_reverification_when_a_prepared_vm_runs_with_no_registration() {
    // given the attempt's path and what the restart lists
    // when it recovers
    let (kit, recovered) = row(
        path::prepared(1),
        vec![listed(1, instance(), true)],
        Some(GithubView::NotRegistered),
    );

    // then
    assert_eq!(decision(&recovered), Decision::Adopt(Adoption::Reverify));
    assert_eq!(kit.state(1).epoch, RUN);
    assert_eq!(kit.state(1).phase, Phase::Prepared);
}

#[test]
fn should_adopt_and_monitor_when_a_listening_runner_is_online() {
    // given the attempt's path and what the restart lists
    // when it recovers
    let (kit, recovered) = row(
        path::listening(1),
        vec![listed(1, instance(), true)],
        Some(GithubView::Online),
    );

    // then
    assert_eq!(decision(&recovered), Decision::Adopt(Adoption::Monitor));
    assert_eq!(kit.state(1).phase, Phase::Listening);
    assert!(!deletes(&kit));
}

#[test]
fn should_adopt_and_never_delete_when_github_is_unreachable() {
    // given the attempt's path and what the restart lists
    // when it recovers
    let (kit, recovered) = row(
        path::busy(1),
        vec![listed(1, instance(), true)],
        Some(GithubView::Unreachable),
    );

    // then
    assert_eq!(decision(&recovered), Decision::Adopt(Adoption::Monitor));
    assert_eq!(kit.state(1).phase, Phase::Busy);
    assert!(!deletes(&kit));
}

#[test]
fn should_retire_when_a_listening_runner_has_no_registration_left() {
    // given the attempt's path and what the restart lists
    // when it recovers
    let (kit, recovered) = row(
        path::listening(1),
        vec![listed(1, instance(), true)],
        Some(GithubView::Absent),
    );

    // then
    assert!(matches!(decision(&recovered), Decision::Retire(_)));
    let state = kit.state(1);
    assert!(
        state.retirement_requested.is_some() || matches!(state.phase, Phase::Retiring),
        "{state:?}"
    );
    assert_eq!(recovered.counts.retired, 1);
}

#[test]
fn should_retire_when_a_busy_vm_stopped_and_its_runner_is_offline() {
    // given the attempt's path and what the restart lists
    // when it recovers
    let (kit, recovered) = row(
        path::busy(1),
        vec![listed(1, instance(), false)],
        Some(GithubView::Offline),
    );

    // then
    assert!(matches!(decision(&recovered), Decision::Retire(_)));
    let state = kit.state(1);
    assert!(
        matches!(state.phase, Phase::Retiring) || state.retirement_requested.is_some(),
        "{state:?}"
    );
}

#[test]
fn should_presume_the_jit_leaked_when_a_registration_was_issued_and_not_delivered() {
    // given the attempt's path and what the restart lists
    // when it recovers
    let (kit, recovered) = row(
        path::jit_issued(1),
        vec![listed(1, instance(), true)],
        Some(GithubView::IssuedNotDelivered),
    );

    // then
    assert_eq!(decision(&recovered), Decision::PresumeLeaked);
    let events = kit.events(1);
    assert!(events.iter().any(|e| matches!(
        e,
        AttemptEvent::RetirementRequested(r) if r.reason == RetireReason::JitPresumedLeaked
    ) || matches!(
        e,
        AttemptEvent::AttemptRetired(r) if r.reason == RetireReason::JitPresumedLeaked
    )));
}

#[test]
fn should_adopt_and_never_delete_when_an_activating_runner_is_busy() {
    // given the attempt's path and what the restart lists
    // when it recovers
    let (kit, recovered) = row(
        path::jit_issued(1),
        vec![listed(1, instance(), true)],
        Some(GithubView::Busy),
    );

    // then
    assert_eq!(decision(&recovered), Decision::Adopt(Adoption::Monitor));
    assert!(!deletes(&kit));
    assert_eq!(kit.state(1).epoch, RUN);
}

#[test]
fn should_quarantine_a_prepared_attempt_when_github_knows_its_runner() {
    // given the attempt's path and what the restart lists
    // when it recovers
    let (kit, recovered) = row(
        path::prepared(1),
        vec![listed(1, instance(), true)],
        Some(GithubView::Online),
    );

    // then
    assert_eq!(
        decision(&recovered),
        Decision::Quarantine(QuarantineReason::JitPresumedLeaked)
    );
    assert!(matches!(
        kit.state(1).phase,
        Phase::Quarantined {
            reason: QuarantineReason::JitPresumedLeaked,
            ..
        }
    ));
    assert_eq!(recovered.counts.quarantined, 1);
}

#[test]
fn should_report_the_leaked_vm_when_a_deleted_attempt_is_still_listed() {
    // given the attempt's path and what the restart lists
    // when it recovers
    let (kit, recovered) = row(path::deleted(1), vec![listed(1, instance(), false)], None);

    // then
    assert_eq!(decision(&recovered), Decision::DeleteLeaked);
    assert_eq!(
        recovered.reported,
        vec![crate::recovery_kit::vm_name(1).to_string()]
    );
    assert_eq!(kit.state(1).phase, Phase::Deleted);
}

#[test]
fn should_confirm_the_deletion_when_a_deleting_attempt_finds_its_vm_absent() {
    // given the attempt's path and what the restart lists
    // when it recovers
    let (kit, recovered) = row(path::deleting(1), Vec::new(), None);

    // then
    assert_eq!(decision(&recovered), Decision::ConfirmDeleted);
    assert_eq!(kit.state(1).phase, Phase::Deleted);
    assert_eq!(kit.state(1).vanished, None, "our own delete is no desync");
}

#[test]
fn should_mark_the_attempt_lost_as_a_desync_when_its_recorded_vm_is_absent() {
    // given the attempt's path and what the restart lists
    // when it recovers
    let (kit, recovered) = row(path::verifying(1), Vec::new(), None);

    // then
    assert_eq!(decision(&recovered), Decision::MarkLost);
    let state = kit.state(1);
    assert_eq!(state.phase, Phase::Lost);
    assert!(state.vanished.is_some(), "VmVanished (DEC-07)");
    assert_eq!(recovered.counts.lost, 1);
}

/// Attempt 1 driven along `path`, then a restart that reads `vms` and `github` for it, whatever
/// the recovery answers.
fn restart(
    path: Vec<AttemptCommand>,
    vms: Vec<VmObservation>,
    github: Option<GithubView>,
) -> (Kit, Recovery) {
    let kit = kit();
    kit.previous_run();
    kit.drive(1, path);
    let _ = kit.open(1);
    let views: BTreeMap<_, _> = github.map(|g| (attempt(1), g)).into_iter().collect();
    let saga = AttemptSaga::new(&kit.store, kit.sessions.clone(), SagaConfig::default());
    let recovery = block_on(
        kit.startup(false)
            .recover(&inventory(0x2, RUN, vms), &views, &saga),
    )
    .unwrap_or_else(|e| panic!("{e}"));
    (kit, recovery)
}

#[test]
fn should_suspend_the_cleanup_when_a_deleting_vm_runs_a_busy_runner() {
    // given the attempt's path and what the restart lists
    // when it recovers
    let (kit, recovered) = row(
        path::deleting(1),
        vec![listed(1, instance(), true)],
        Some(GithubView::Busy),
    );

    // then the attempt is held, left unadopted with its pending delete held by the relay's
    // sink, and recovery completes with admission open
    assert_eq!(decision(&recovered), Decision::SuspendCleanup);
    assert_eq!(recovered.held, vec![attempt(1)]);
    let state = kit.state(1);
    assert_eq!(state.phase, Phase::Deleting);
    assert_eq!(
        state.epoch, BEFORE,
        "not adopted (attempt_not_adopted holds the delete)"
    );
    assert!(!deletes_again(&kit));
    assert_eq!(kit.hostrun().phase, RunPhase::Operating);
    assert!(
        !kit.host_state()
            .gate
            .closed
            .contains(&AdmissionCause::Recovering)
    );
    assert!(
        kit.host_state().leases.contains_key(&attempt(1)),
        "its lease keeps its slot"
    );
}

#[test]
fn should_resume_a_held_cleanup_when_github_reports_the_runner_offline() {
    // given a cleanup held at recovery, with admission open meanwhile
    let kit = kit();
    kit.previous_run();
    let mut deleting = path::deleting(1);
    deleting.push(path::sent(1, 8));
    kit.drive(1, deleting);
    let _ = kit.open(1);
    let startup = kit.startup(false);
    let saga = AttemptSaga::new(&kit.store, kit.sessions.clone(), SagaConfig::default());
    let busy = BTreeMap::from([(attempt(1), GithubView::Busy)]);
    let listing = |id| inventory(id, RUN, vec![listed(1, instance(), true)]);
    let _ =
        block_on(startup.recover(&listing(0x2), &busy, &saga)).unwrap_or_else(|e| panic!("{e}"));
    assert!(
        !kit.host_state()
            .gate
            .closed
            .contains(&AdmissionCause::Recovering)
    );
    let still = block_on(startup.resume_held(&listing(0x3), &busy, &saga))
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(still, vec![attempt(1)], "held while the runner is busy");
    assert_eq!(saga.next_due(), None, "no deadline runs for a held delete");

    // when GitHub reports the runner offline
    let offline = BTreeMap::from([(attempt(1), GithubView::Offline)]);
    let still = block_on(startup.resume_held(&listing(0x4), &offline, &saga))
        .unwrap_or_else(|e| panic!("{e}"));

    // then it is adopted and its cleanup resumes: the delete sent before the restart is
    // Unknown from the adoption, the listing that shows the VM reads it not applied,
    // and the delete is due again with its proof (the data plane re-checks it)
    assert!(still.is_empty());
    let state = kit.state(1);
    assert_eq!(state.epoch, RUN);
    assert_eq!(state.phase, Phase::Deleting);
    assert_eq!(state.pending, None);
    assert!(matches!(
        next_step(&state),
        SagaStep::Issue(DirectiveKind::DeleteVm { .. })
    ));
    assert!(kit.events(1).iter().any(|e| matches!(
        e,
        AttemptEvent::DirectiveObservedNotApplied(d) if d.directive_id == path::directive(1, 8)
    )));
}

#[test]
fn should_count_every_pass_when_recovery_completes_after_waiting() {
    // given attempt 1 retires on the first pass while attempt 2's listing says nothing yet
    let kit = kit();
    kit.previous_run();
    kit.drive(1, path::listening(1));
    kit.drive(2, path::prepared(2));
    let _ = kit.open(1);
    let startup = kit.startup(false);
    let saga = AttemptSaga::new(&kit.store, kit.sessions.clone(), SagaConfig::default());
    let views = BTreeMap::from([
        (attempt(1), GithubView::Absent),
        (attempt(2), GithubView::NotRegistered),
    ]);
    let unread = named(&crate::recovery_kit::vm_name(2).to_string(), None, true);
    let first = block_on(startup.recover(
        &inventory(0x2, RUN, vec![listed(1, instance(), true), unread]),
        &views,
        &saga,
    ))
    .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(
        first,
        Recovery::Waiting {
            waiting: vec![attempt(2)]
        }
    );

    // when the next inventory lists both
    let second = block_on(startup.recover(
        &inventory(
            0x3,
            RUN,
            vec![listed(1, instance(), true), listed(2, instance(), true)],
        ),
        &views,
        &saga,
    ))
    .unwrap_or_else(|e| panic!("{e}"));

    // then the counts hold both passes' work
    let Recovery::Completed(recovered) = second else {
        panic!("not completed: {second:?}");
    };
    assert_eq!(recovered.counts.retired, 1, "the first pass's retirement");
    assert_eq!(recovered.counts.adopted, 2);
}

#[test]
fn should_keep_admission_closed_when_a_listed_vm_says_nothing_yet() {
    // given the attempt's VM listed without a readable instance id
    // when the restart recovers
    let (kit, recovery) = restart(
        path::prepared(1),
        vec![named(
            &crate::recovery_kit::vm_name(1).to_string(),
            None,
            true,
        )],
        Some(GithubView::NotRegistered),
    );

    // then
    assert_eq!(
        recovery,
        Recovery::Waiting {
            waiting: vec![attempt(1)],
        }
    );
    assert_eq!(kit.state(1).epoch, BEFORE, "nothing decided for it (I25)");
    assert_eq!(kit.hostrun().phase, RunPhase::Recovering);
    assert!(
        kit.host_state()
            .gate
            .closed
            .contains(&AdmissionCause::Recovering)
    );
}

#[test]
fn should_start_in_safe_mode_when_an_unknown_kind_leaves_the_projections_unprepared() {
    // given a newer build's kind in the journal, and this build's projections never prepared
    let kit = kit();
    let newer = AggregateStore::new::<NewerCounters>(
        kit.memory.clone(),
        FakeClock::at(NOW),
        SeededRng::new(5),
        NoNotifier,
    )
    .unwrap_or_else(|e| panic!("{e}"));
    let _ = block_on(newer.save::<NewerCounter>(SaveRequest::new(
        "1",
        counter_command(0x3157, CounterBody::Note),
    )))
    .unwrap_or_else(|e| panic!("{e}"));

    // when
    let (opened, link) = kit.open(0);

    // then
    assert_eq!(opened.verification, Verification::UnknownEventKind);
    assert_eq!(opened.mode, Mode::Safe);
    assert_eq!(link.welcomes().len(), 1);
    assert_eq!(
        kit.hostrun().safe_mode.map(|s| s.reason),
        Some(SafeModeReason::UnknownEventKind)
    );
}

#[test]
fn should_keep_the_case_when_the_attempt_is_quarantined() {
    // given the attempt's path and what the restart lists
    // when it recovers
    let (kit, recovered) = row(
        path::quarantined(1),
        vec![listed(1, instance(), true)],
        None,
    );

    // then
    assert_eq!(decision(&recovered), Decision::Keep);
    assert!(matches!(kit.state(1).phase, Phase::Quarantined { .. }));
    assert_eq!(
        kit.state(1).epoch,
        RUN,
        "adopted under the run, its case kept"
    );
    assert!(!deletes(&kit));
}

#[test]
fn should_report_and_never_touch_when_an_mf_vm_has_no_record() {
    // given
    let stray = crate::recovery_kit::vm_name(9).to_string();

    // when
    let (kit, recovered) = row(
        path::prepared(1),
        vec![
            listed(1, instance(), true),
            named(&stray, Some(other_instance()), true),
        ],
        Some(GithubView::NotRegistered),
    );

    // then
    assert_eq!(recovered.reported, vec![stray]);
    assert!(kit.events(9).is_empty());
}

/// Attempt 1 driven along `path` with its last directive `k` sent (`DirectiveSent`) and
/// unanswered when the run died, then a restart that reads `vms` (CP-04D).
fn sent_before_the_crash(
    mut path: Vec<AttemptCommand>,
    k: u128,
    vms: Vec<VmObservation>,
) -> (Kit, Recovered) {
    path.push(path::sent(1, k));
    row(path, vms, None)
}

/// How many pending effects `mayfly status --json` counts as unknown
/// (`attempts.unknown_effects`, from the `pending_effects` query).
fn unknown_effects(kit: &Kit) -> usize {
    block_on(queries::pending_effects(&kit.memory))
        .unwrap_or_else(|e| panic!("{e}"))
        .1
}

#[test]
fn should_move_past_cloning_at_recovery_when_a_clone_sent_before_the_crash_is_listed() {
    // given: a clone sent, not answered and not yet Unknown when the run died (its deadline is
    // a minute away), and the restart's inventory lists the clone
    // when it recovers
    let (kit, recovered) =
        sent_before_the_crash(path::cloning(1), 1, vec![listed(1, instance(), false)]);

    // then: adopted, the outcome made Unknown by the adoption and resolved by the inventory
    // with the listed instance id: past cloning at recovery, the boot next, one clone
    assert_eq!(
        decision(&recovered),
        Decision::Adopt(Adoption::Clone(instance()))
    );
    assert_eq!(recovered.adopted, vec![attempt(1)]);
    let state = kit.state(1);
    assert_eq!(state.epoch, RUN);
    assert_eq!(state.phase, Phase::Booting);
    assert_eq!(state.instance_id, Some(instance()));
    assert_eq!(state.pending, None);
    assert_eq!(next_step(&state), SagaStep::Issue(DirectiveKind::StartVm));
    let events = kit.events(1);
    let unknown_at = events
        .iter()
        .position(|e| matches!(e, AttemptEvent::DirectiveOutcomeUnknown(..)));
    let adopted_at = events
        .iter()
        .position(|e| matches!(e, AttemptEvent::AttemptAdopted(..)));
    let cloned_at = events
        .iter()
        .position(|e| matches!(e, AttemptEvent::VmCloned(..)));
    assert!(
        adopted_at < unknown_at && unknown_at < cloned_at && adopted_at.is_some(),
        "adopted, then Unknown, then resolved: {events:?}"
    );
    let clones = events
        .iter()
        .filter(|e| {
            matches!(e, AttemptEvent::DirectiveIssued(d) if d.kind.tag() == DirectiveTag::CloneVm)
        })
        .count();
    assert_eq!(clones, 1, "never cloned again");
    assert_eq!(unknown_effects(&kit), 0, "nothing left unknown");
}

#[test]
fn should_retry_by_the_same_name_at_recovery_when_a_clone_sent_before_the_crash_is_absent() {
    // given: a clone sent and unanswered when the run died, and the restart's complete inventory
    // does not list it
    // when it recovers
    let (kit, recovered) = sent_before_the_crash(path::cloning(1), 1, Vec::new());

    // then: the adoption's Unknown resolved as not done at recovery, and the clone due again
    // at once (the saga's next step) by the same name and generation (I13), within the budget
    assert_eq!(decision(&recovered), Decision::RetryClone);
    let state = kit.state(1);
    assert!(matches!(
        state.phase,
        Phase::Backoff {
            stage: Stage::Clone,
            ..
        }
    ));
    assert_eq!(state.instance_id, None);
    assert_eq!(state.pending, None);
    assert_eq!(next_step(&state), SagaStep::Issue(DirectiveKind::CloneVm));
    let identity = state
        .identity
        .clone()
        .unwrap_or_else(|| panic!("no identity"));
    assert_eq!(identity.vm_name, crate::recovery_kit::vm_name(1));
    assert!(kit.events(1).iter().any(|e| matches!(
        e,
        AttemptEvent::DirectiveObservedNotApplied(d) if d.directive_id == path::directive(1, 1)
    )));
    assert_eq!(unknown_effects(&kit), 0);
}

#[test]
fn should_record_the_late_clone_by_observation_when_the_retry_finds_its_name_taken() {
    // given: the clone sent before the crash was absent at recovery, so its retry (the last try
    // of the budget) is issued and sent; meanwhile the orphaned `tart clone` of the killed run
    // finishes, so the provider finds the name taken and the data plane answers Unknown
    let (kit, _) = sent_before_the_crash(path::cloning(1), 1, Vec::new());
    for body in [path::issue(1, 2), path::sent(1, 2)] {
        assert!(matches!(
            kit.command(1, RUN, body),
            Outcome::Accepted { .. }
        ));
    }
    let answered = kit.command(
        1,
        RUN,
        AttemptCommand::RecordObservation(AttemptObservation {
            observation_id: ObservationId::from_u128(0x0A15),
            epoch: RUN,
            generation: Generation::FIRST,
            fact: Fact::OutcomeUnknown {
                directive_id: path::directive(1, 2),
            },
        }),
    );
    let at_the_answer = kit.state(1);

    // when the next inventory lists the clone the orphan made
    block_on(Inbox::new(&kit.store, host(), RUN).record(&inventory(
        0x5,
        RUN,
        vec![listed(1, other_instance(), false)],
    )))
    .unwrap_or_else(|e| panic!("{e}"));

    // then: the budget spent, yet nothing retired it; the observation records the VM under its
    // name, and the attempt moves on with it (no VM left behind as an unrecorded name)
    assert!(matches!(answered, Outcome::Accepted { .. }));
    assert_eq!(at_the_answer.tries, 2);
    assert!(
        at_the_answer
            .pending
            .is_some_and(|p| matches!(p.outcome, PendingOutcome::Unknown { .. }))
    );
    let state = kit.state(1);
    assert_eq!(state.phase, Phase::Booting);
    assert_eq!(state.instance_id, Some(other_instance()));
    assert!(
        !kit.events(1)
            .iter()
            .any(|e| matches!(e, AttemptEvent::AttemptRetired(..))),
        "never retired"
    );
}

#[test]
fn should_count_the_outcome_unknown_when_a_directive_sent_before_the_crash_has_no_inventory_answer()
{
    // given: a verification sent and unanswered when the run died; the restart lists the VM
    // running, which says nothing of the guest's answer
    // when it recovers
    let mut p = path::verifying(1);
    p.push(path::issue(1, 3));
    let (kit, recovered) = sent_before_the_crash(p, 3, vec![listed(1, instance(), true)]);

    // then: adopted with the outcome Unknown, which status counts until an observation
    // resolves it (P19), and no deadline of the older run is waited for
    assert_eq!(decision(&recovered), Decision::Adopt(Adoption::Reverify));
    let state = kit.state(1);
    assert_eq!(state.epoch, RUN);
    assert!(
        state
            .pending
            .is_some_and(|p| p.tag == DirectiveTag::VerifyGuest
                && matches!(p.outcome, PendingOutcome::Unknown { .. }))
    );
    assert_eq!(unknown_effects(&kit), 1);
}

#[test]
fn should_stop_again_at_recovery_when_a_stop_sent_before_the_crash_left_the_vm_running() {
    // given: a retirement whose stop was sent and unanswered when the run died (P24 left the
    // VM running under its shim), and the restart's inventory lists the VM still running
    let mut p = path::prepared(1);
    p.push(AttemptCommand::RequestRetirement {
        reason: RetireReason::Drain,
    });
    p.push(path::issue(1, 4));
    p.push(path::observe(
        1,
        4,
        Fact::DiagnosticsExported {
            directive_id: path::directive(1, 4),
            bytes: 0,
            sha256: [0; 32],
        },
    ));
    p.push(path::issue(1, 5));

    // when it recovers
    let (kit, recovered) = sent_before_the_crash(p, 5, vec![listed(1, instance(), true)]);

    // then: adopted, the stop made Unknown by the adoption and resolved as not applied by the
    // listing, so the stop is due again at once under the new run: the retirement finishes
    // without waiting for the old stop's deadline (the daemon-level
    // `should_force_p24_when_ctrl_c_is_pressed_a_third_time_during_the_wait` waits for it)
    assert_eq!(decision(&recovered), Decision::Adopt(Adoption::Continue));
    let state = kit.state(1);
    assert_eq!(state.epoch, RUN);
    assert_eq!(state.phase, Phase::Retiring);
    assert_eq!(state.pending, None);
    assert!(matches!(
        next_step(&state),
        SagaStep::Issue(DirectiveKind::StopVm { .. })
    ));
    let events = kit.events(1);
    let adopted_at = events
        .iter()
        .position(|e| matches!(e, AttemptEvent::AttemptAdopted(..)));
    let unknown_at = events.iter().position(|e| {
        matches!(e, AttemptEvent::DirectiveOutcomeUnknown(d) if d.directive_id == path::directive(1, 5))
    });
    let not_applied_at = events.iter().position(|e| {
        matches!(e, AttemptEvent::DirectiveObservedNotApplied(d) if d.directive_id == path::directive(1, 5))
    });
    assert!(
        adopted_at.is_some() && adopted_at < unknown_at && unknown_at < not_applied_at,
        "adopted, then Unknown, then not applied: {events:?}"
    );
    assert_eq!(unknown_effects(&kit), 0, "nothing left unknown");
}

#[test]
fn should_send_a_held_directive_under_the_new_epoch_when_its_attempt_is_adopted() {
    // given: a clone issued and never recorded sent when the run died (the relay held it)
    let kit = kit();
    kit.previous_run();
    kit.drive(1, path::cloning(1));
    let _ = kit.open(1);
    let saga = AttemptSaga::new(&kit.store, kit.sessions.clone(), SagaConfig::default());

    // when the clone is listed
    let recovery = block_on(kit.startup(false).recover(
        &inventory(0x2, RUN, vec![listed(1, instance(), false)]),
        &BTreeMap::new(),
        &saga,
    ))
    .unwrap_or_else(|e| panic!("{e}"));

    // then: still in flight, never Unknown, for the relay to send now that the attempt is
    // adopted (its answer, by the name, resolves it), and no deadline armed before that send
    assert!(matches!(recovery, Recovery::Completed(_)));
    let state = kit.state(1);
    assert_eq!(state.epoch, RUN);
    assert!(state.pending.is_some_and(|p| p.tag == DirectiveTag::CloneVm
        && p.outcome == PendingOutcome::InFlight
        && !p.sent));
    assert_eq!(saga.next_due(), None);
    assert_eq!(unknown_effects(&kit), 0);
}

#[test]
fn should_open_admission_and_complete_recovery_when_every_row_is_decided() {
    // given the attempt's path and what the restart lists
    // when it recovers
    let (kit, _) = row(
        path::prepared(1),
        vec![listed(1, instance(), true)],
        Some(GithubView::NotRegistered),
    );

    // then
    assert_eq!(kit.hostrun().phase, RunPhase::Operating);
    assert!(
        !kit.host_state()
            .gate
            .closed
            .contains(&AdmissionCause::Recovering)
    );
}

// ---- safe mode and faults ---------------------------------------------------------------------

/// F35: the journal is restored from an older backup: the floor the data plane kept is above
/// every epoch the journal minted.
#[test]
fn should_admit_and_delete_nothing_when_the_journal_was_restored_from_an_older_backup() {
    // given
    let kit = kit();
    kit.previous_run();
    kit.drive(1, path::prepared(1));
    kit.drive(2, path::deleting(2));
    let before = (kit.events(1).len(), kit.events(2).len());

    // when
    let (opened, link) = kit.open(7);
    let saga = AttemptSaga::new(&kit.store, kit.sessions.clone(), SagaConfig::default());
    let recovery = block_on(kit.startup(false).recover(
        &inventory(0x2, opened.epoch, vec![listed(1, instance(), true)]),
        &BTreeMap::new(),
        &saga,
    ))
    .unwrap_or_else(|e| panic!("{e}"));

    // then
    assert_eq!(opened.mode, Mode::Safe);
    assert_eq!(link.welcomes()[0].mode, Mode::Safe);
    assert_eq!(kit.sessions.get(&host()).map(|s| s.mode), Some(Mode::Safe));
    let Recovery::SafeMode {
        cause, attempts, ..
    } = recovery
    else {
        panic!("not safe mode: {recovery:?}");
    };
    assert_eq!(cause, SafeModeCause::EpochBelowFloor);
    assert_eq!(
        attempts,
        vec![
            (attempt(1), Proposal::Adopt),
            (attempt(2), Proposal::MarkLost)
        ],
        "a salvage plan's proposals"
    );
    assert_eq!(
        (kit.events(1).len(), kit.events(2).len()),
        before,
        "nothing written to any attempt"
    );
    assert_eq!(kit.hostrun().phase, RunPhase::Recovering);
    let reserved = block_on(kit.store.execute::<Host>(SaveRequest::new(
        host().as_str(),
        Command::new(
            CommandId::from_u128(0xF35),
            NOW,
            opened.epoch,
            Actor::Automation {
                policy: "reconciler",
            },
            HostCommand::ReserveSlot(mayfly_control_domain::host::ReserveSlot {
                attempt_id: attempt(3),
                pool: mayfly_domain::PoolName::sample(),
                os: mayfly_control_domain::attempt::GuestOs::Linux,
                resources: mayfly_domain::Resources::new(8 * GIB, 50 * GIB),
                terms_revision: 1,
            }),
        ),
    )))
    .unwrap_or_else(|e| panic!("{e}"));
    assert!(
        matches!(
            reserved,
            mayfly_control::store::Outcome::Refused {
                rejection: HostRejection::AdmissionClosed,
                ..
            }
        ),
        "safe mode admits nothing: {reserved:?}"
    );
}

fn old<B>(id: u128, body: B) -> Command<B> {
    Command::new(
        CommandId::from_u128(id),
        Millis::new(1),
        BEFORE,
        Actor::Automation { policy: "zombie" },
        body,
    )
}

/// F43: a delayed command of the previous run reaches every aggregate after the restart.
#[test]
fn should_refuse_a_zombie_writer_of_an_older_run_when_its_command_arrives_after_the_restart() {
    // given
    let (kit, _) = row(
        path::prepared(1),
        vec![listed(1, instance(), true)],
        Some(GithubView::NotRegistered),
    );

    // when
    let attempt_cmd = kit.command(
        1,
        BEFORE,
        AttemptCommand::RequestRetirement {
            reason: RetireReason::Surplus,
        },
    );
    let hostrun_cmd = block_on(kit.store.execute::<HostRun>(SaveRequest::new(
        host().as_str(),
        old(0x430, HostRunCommand::BeginShutdown),
    )))
    .unwrap_or_else(|e| panic!("{e}"));
    let host_cmd = block_on(kit.store.execute::<Host>(SaveRequest::new(
        host().as_str(),
        old(
            0x431,
            HostCommand::CloseAdmission {
                cause: AdmissionCause::Woke,
            },
        ),
    )))
    .unwrap_or_else(|e| panic!("{e}"));

    // then
    assert!(matches!(
        attempt_cmd,
        mayfly_control::store::Outcome::Refused {
            rejection: AttemptRejection::StaleRun,
            ..
        }
    ));
    assert!(matches!(
        hostrun_cmd,
        mayfly_control::store::Outcome::Refused {
            rejection: HostRunRejection::StaleRun,
            ..
        }
    ));
    assert!(matches!(
        host_cmd,
        mayfly_control::store::Outcome::Refused {
            rejection: HostRejection::StaleRun,
            ..
        }
    ));
    assert_eq!(kit.hostrun().phase, RunPhase::Operating, "nothing changed");
}

#[test]
fn should_hold_effects_at_once_when_safe_mode_is_entered_during_the_run() {
    // given
    let (kit, _) = row(
        path::prepared(1),
        vec![listed(1, instance(), true)],
        Some(GithubView::NotRegistered),
    );
    assert_eq!(
        kit.sessions.get(&host()).map(|s| s.mode),
        Some(Mode::Normal)
    );

    // when
    let entered = block_on(mayfly_control::recovery::enter_safe_mode(
        &kit.store,
        &kit.sessions,
        &host(),
        RUN,
        SafeModeReason::UnknownEventKind,
    ))
    .unwrap_or_else(|e| panic!("{e}"));

    // then
    assert!(matches!(
        entered,
        mayfly_control::store::Outcome::Accepted { .. }
    ));
    assert!(kit.hostrun().safe_mode.is_some());
    assert_eq!(kit.sessions.get(&host()).map(|s| s.mode), Some(Mode::Safe));
}

#[test]
fn should_forget_the_session_when_the_data_plane_leaves() {
    // given
    let kit = kit();
    let _ = kit.open(0);
    assert!(kit.sessions.get(&host()).is_some());

    // when
    mayfly_control::recovery::session_ended(&kit.sessions, &host());

    // then
    assert_eq!(
        kit.sessions.get(&host()),
        None,
        "nothing is sent or issued for it"
    );
}

#[test]
fn should_refuse_the_session_when_the_hello_names_another_host() {
    // given
    let kit = kit();
    let mut offered = hello(0);
    offered.host_id = mayfly_domain::HostId::new("host9").unwrap_or_else(|e| panic!("{e}"));
    let mut link = Responder::new(offered);

    // when
    let opened = block_on(kit.startup(false).open(
        &mut link,
        &Structure(true),
        (&kit.memory, &projections(), &build()),
    ));

    // then
    assert!(matches!(opened, Err(StartupError::WrongHost(_))));
    assert!(link.welcomes().is_empty());
}

#[test]
fn should_enter_safe_mode_when_the_structure_does_not_verify() {
    // given
    let kit = kit();
    let mut link = Responder::new(hello(0));

    // when
    let opened = block_on(kit.startup(false).open(
        &mut link,
        &Structure(false),
        (&kit.memory, &projections(), &build()),
    ))
    .unwrap_or_else(|e| panic!("{e}"));

    // then
    assert_eq!(opened.verification, Verification::StructureFailed);
    assert_eq!(opened.mode, Mode::Safe);
    assert!(format!("{:?}", kit.startup(false)).contains("Startup"));
}

#[test]
fn should_leave_a_former_runs_safe_mode_when_a_restart_verifies() {
    // given a run that recorded a crash loop
    let kit = kit();
    let mut looping = Responder::new(hello(0));
    let first = block_on(kit.startup(true).open(
        &mut looping,
        &Structure(true),
        (&kit.memory, &projections(), &build()),
    ))
    .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(first.mode, Mode::Safe);

    // when the next run verifies
    let (opened, _) = kit.open(first.epoch.get());

    // then
    assert_eq!(opened.mode, Mode::Normal);
    assert_eq!(kit.hostrun().safe_mode, None);
}

#[test]
fn should_refuse_the_recovery_when_the_observation_is_not_an_inventory() {
    // given
    let kit = kit();
    let (opened, _) = kit.open(0);
    let saga = AttemptSaga::new(&kit.store, kit.sessions.clone(), SagaConfig::default());
    let mut capacity = inventory(0x2, opened.epoch, Vec::new());
    capacity.kind = mayfly_protocol::ObservationKind::HypervisorLimitHit;

    // when
    let recovery = block_on(
        kit.startup(false)
            .recover(&capacity, &BTreeMap::new(), &saga),
    );

    // then
    assert_eq!(recovery, Err(StartupError::NotAnInventory));
}

#[test]
fn should_propose_for_unrecorded_names_when_the_journal_is_untrusted() {
    // given a stray mf-* VM, an image base and a foreign VM listed in safe mode
    let kit = kit();
    let (opened, _) = kit.open(3);
    let saga = AttemptSaga::new(&kit.store, kit.sessions.clone(), SagaConfig::default());
    let stray = crate::recovery_kit::vm_name(9).to_string();
    let listing = inventory(
        0x2,
        opened.epoch,
        vec![
            named(&stray, Some(other_instance()), true),
            named("mfb-base", Some(other_instance()), false),
            named("someone-else", Some(other_instance()), true),
        ],
    );

    // when
    let recovery = block_on(
        kit.startup(false)
            .recover(&listing, &BTreeMap::new(), &saga),
    )
    .unwrap_or_else(|e| panic!("{e}"));

    // then
    let Recovery::SafeMode { names, .. } = recovery else {
        panic!("not safe mode: {recovery:?}");
    };
    assert_eq!(
        names,
        vec![
            (stray, Proposal::Report),
            ("mfb-base".to_owned(), Proposal::Ignore),
            ("someone-else".to_owned(), Proposal::Ignore),
        ]
    );
}

#[test]
fn should_record_each_reason_when_safe_mode_is_entered_during_the_run() {
    // given
    let (kit, _) = row(
        path::prepared(1),
        vec![listed(1, instance(), true)],
        Some(GithubView::NotRegistered),
    );

    // when
    let outcomes: Vec<bool> = [
        SafeModeReason::EpochBelowFloor,
        SafeModeReason::UnknownEventKind,
        SafeModeReason::VerificationFailed,
        SafeModeReason::CrashLoop,
    ]
    .into_iter()
    .map(|reason| {
        matches!(
            block_on(mayfly_control::recovery::enter_safe_mode(
                &kit.store,
                &kit.sessions,
                &host(),
                RUN,
                reason,
            )),
            Ok(mayfly_control::store::Outcome::Accepted { .. })
        )
    })
    .collect();

    // then
    assert_eq!(outcomes, vec![true; 4]);
}

#[test]
fn should_say_what_stopped_start_up_when_each_error_is_displayed() {
    // given
    let errors = [
        StartupError::Store(mayfly_control::store::SaveError::Busy),
        StartupError::Storage(mayfly_control::journal::StorageError::new("disk")),
        StartupError::Link(mayfly_protocol::LinkError::Closed),
        StartupError::Version,
        StartupError::WrongHost(host()),
        StartupError::Refused("no_run"),
        StartupError::NotAnInventory,
    ];

    // when
    let shown: Vec<String> = errors.iter().map(ToString::to_string).collect();

    // then
    assert!(shown.iter().all(|s| !s.is_empty()));
    assert!(shown[4].contains("host0"));
    assert!(shown[5].contains("no_run"));
}

#[test]
fn should_keep_the_cleanup_held_when_github_cannot_be_read() {
    // given a cleanup held at recovery
    let kit = kit();
    kit.previous_run();
    kit.drive(1, path::deleting(1));
    let _ = kit.open(1);
    let startup = kit.startup(false);
    let saga = AttemptSaga::new(&kit.store, kit.sessions.clone(), SagaConfig::default());
    let listing = |id| inventory(id, RUN, vec![listed(1, instance(), true)]);
    let busy = BTreeMap::from([(attempt(1), GithubView::Busy)]);
    let _ =
        block_on(startup.recover(&listing(0x2), &busy, &saga)).unwrap_or_else(|e| panic!("{e}"));

    // when GitHub cannot be read, or gives no view of the runner
    let unreachable = BTreeMap::from([(attempt(1), GithubView::Unreachable)]);
    let unread = block_on(startup.resume_held(&listing(0x3), &unreachable, &saga))
        .unwrap_or_else(|e| panic!("{e}"));
    let missing = block_on(startup.resume_held(&listing(0x4), &BTreeMap::new(), &saga))
        .unwrap_or_else(|e| panic!("{e}"));

    // then a failed read never releases the held delete
    assert_eq!(unread, vec![attempt(1)]);
    assert_eq!(missing, vec![attempt(1)]);
    assert_eq!(kit.state(1).epoch, BEFORE, "still unadopted");
    assert!(!deletes_again(&kit));
}

#[test]
fn should_keep_the_counts_of_a_pass_when_it_fails_midway() {
    // given a pass that adopts both attempts and then fails on attempt 1's retirement
    let kit = kit();
    kit.previous_run();
    kit.drive(1, path::listening(1));
    kit.drive(2, path::prepared(2));
    let _ = kit.open(1);
    let journal = kit.journal.clone();
    let memory = kit.memory.clone();
    let first = crate::recovery_kit::stream(&format!("attempt-{}", attempt(1)));
    let again = first.clone();
    kit.journal
        .before_next_append(crate::scripted::BeforeAppend::appending(first, move || {
            journal.before_next_append(crate::scripted::BeforeAppend::appending(
                again,
                move || {
                    assert!(memory.inject(
                        mayfly_testkit::contracts::journal::InjectedFault::StorageBeforeCommit
                    ));
                },
            ));
        }));
    let startup = kit.startup(false);
    let saga = AttemptSaga::new(&kit.store, kit.sessions.clone(), SagaConfig::default());
    let views = BTreeMap::from([
        (attempt(1), GithubView::Absent),
        (attempt(2), GithubView::NotRegistered),
    ]);
    let listing = |id| {
        inventory(
            id,
            RUN,
            vec![listed(1, instance(), true), listed(2, instance(), true)],
        )
    };
    let failed = block_on(startup.recover(&listing(0x2), &views, &saga));
    assert!(failed.is_err(), "{failed:?}");

    // when the next pass completes
    let second =
        block_on(startup.recover(&listing(0x3), &views, &saga)).unwrap_or_else(|e| panic!("{e}"));

    // then the first pass's adoptions count
    let Recovery::Completed(recovered) = second else {
        panic!("not completed: {second:?}");
    };
    assert_eq!(recovered.counts.adopted, 2);
    assert_eq!(recovered.counts.retired, 1);
}
