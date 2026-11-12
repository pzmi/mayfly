//! A recovery plan applied (P20): the plan's own units read the bound streams as they are now,
//! an action's attempt command is conditioned on the bound revisions, and a plan applied after
//! the world moved, or under another run, is refused (I31, ADR 0041; `formal/recovery.qnt`'s
//! `foreignWriteBeforeTheActionRefusesTest`).

use std::collections::BTreeMap;

use mayfly_control::journal::Journal;
use mayfly_control::recovery::repair::{Applied, Repair, RepairError, World};
use mayfly_control::store::{Outcome, SaveRequest, decode_event};
use mayfly_control_domain::attempt::{
    AttemptCommand, AttemptEvent, Fact, Observation, Phase, QuarantineReason,
};
use mayfly_control_domain::operations::hostrun::RunPhase;
use mayfly_control_domain::operations::plan::{
    ActionKind, Bindings, GithubBinding, PlanAction, PlanPhase, RecoveryPlan, RecoveryPlanCommand,
    RecoveryPlanEvent, RecoveryPlanState, StaleBinding,
};
use mayfly_control_domain::{Actor, Command};
use mayfly_domain::{CommandId, Epoch, Generation, Millis, ObservationId, PlanId, StateDigest};
use mayfly_protocol::Mode;
use mayfly_testkit::block_on;
use mayfly_testkit::contracts::journal::InjectedFault;

use crate::recovery_kit::{
    BEFORE, Kit, NOW, Structure, attempt, instance, kit, kit_over, path, stream,
};
use crate::scripted::BeforeAppend;

const PLAN: u128 = 0x9_1A4;

fn plan_id() -> PlanId {
    PlanId::from_u128(PLAN)
}

fn world() -> World {
    World {
        inventory: StateDigest::from_u128(0x1),
        github: GithubBinding::Unreachable,
    }
}

fn head(kit: &Kit, n: u128) -> u64 {
    block_on(kit.memory.head(&stream(&format!("attempt-{}", attempt(n)))))
        .unwrap_or_else(|e| panic!("{e}"))
        .map(|h| h.revision.get())
        .unwrap_or_else(|| panic!("attempt {n} has no stream"))
}

fn plan(kit: &Kit, epoch: Epoch, id: u128, actor: Actor, body: RecoveryPlanCommand) {
    let command = Command::new(CommandId::from_u128(id), NOW, epoch, actor, body);
    match block_on(
        kit.store
            .execute::<RecoveryPlan>(SaveRequest::new(plan_id(), command)),
    )
    .unwrap_or_else(|e| panic!("{e}"))
    {
        Outcome::Accepted { .. } => {}
        Outcome::Refused { rejection, .. } => panic!("plan command refused: {rejection:?}"),
    }
}

/// Two prepared attempts, a restart onto a journal restored from an older backup (safe mode,
/// epoch 8), and a plan adopting both, drawn and approved in that run.
fn approved() -> (Kit, Epoch) {
    approved_with(&[ActionKind::Adopt, ActionKind::Adopt])
}

/// As [`approved`], with action `i` of kind `kinds[i]` on attempt `i + 1`.
fn approved_with(kinds: &[ActionKind]) -> (Kit, Epoch) {
    approved_on(path::prepared(2), kinds)
}

/// As [`approved_with`], attempt 2 driven along `second`.
fn approved_on(second: Vec<AttemptCommand>, kinds: &[ActionKind]) -> (Kit, Epoch) {
    let kit = kit();
    kit.previous_run();
    kit.drive(1, path::prepared(1));
    kit.drive(2, second);
    let (opened, _) = kit.open(7);
    assert_eq!(opened.mode, Mode::Safe);
    let bindings = Bindings {
        heads: BTreeMap::from([
            (format!("attempt-{}", attempt(1)), head(&kit, 1)),
            (format!("attempt-{}", attempt(2)), head(&kit, 2)),
        ]),
        inventory: world().inventory,
        github: world().github,
        epoch: opened.epoch,
    };
    let actions = kinds
        .iter()
        .zip(1_u128..)
        .map(|(kind, n)| PlanAction {
            attempt_id: attempt(n),
            instance_id: Some(instance()),
            kind: *kind,
        })
        .collect();
    plan(
        &kit,
        opened.epoch,
        0xD1,
        Actor::Startup,
        RecoveryPlanCommand::ProposePlan {
            bindings,
            actions,
            expires_at: Millis::new(NOW.get() + 3_600_000),
        },
    );
    plan(
        &kit,
        opened.epoch,
        0xD2,
        Actor::Operator { uid: 501 },
        RecoveryPlanCommand::ApprovePlan,
    );
    (kit, opened.epoch)
}

/// Another writer on attempt 2's stream (the inbox recording a stale observation): its record
/// unchanged, its head moved.
fn foreign_write(kit: &Kit) {
    let other = kit_over(&kit.memory);
    let _ = other.command(
        2,
        BEFORE,
        AttemptCommand::RecordObservation(Observation {
            observation_id: ObservationId::from_u128(0xF0),
            epoch: BEFORE,
            generation: Generation::FIRST.next().unwrap_or(Generation::FIRST),
            fact: Fact::VmAbsent,
        }),
    );
}

fn plan_state(kit: &Kit) -> RecoveryPlanState {
    block_on(kit.store.load::<RecoveryPlan>(&plan_id().to_string()))
        .unwrap_or_else(|e| panic!("{e}"))
}

fn plan_events(kit: &Kit) -> Vec<RecoveryPlanEvent> {
    block_on(
        kit.memory
            .read_stream(&stream(&format!("recovery-{}", plan_id())), None, None),
    )
    .unwrap_or_else(|e| panic!("{e}"))
    .iter()
    .map(|e| decode_event::<RecoveryPlanEvent>(e).unwrap_or_else(|e| panic!("{e:?}")))
    .collect()
}

fn adopted(kit: &Kit, n: u128) -> bool {
    kit.events(n)
        .iter()
        .any(|e| matches!(e, AttemptEvent::AttemptAdopted(_)))
}

#[test]
fn should_apply_every_action_and_leave_safe_mode_when_the_world_holds() {
    // given
    let (kit, epoch) = approved();

    // when
    let applied = block_on(
        Repair::new(&kit.store, crate::recovery_kit::host(), epoch).apply(
            plan_id(),
            world(),
            &Structure(true),
        ),
    )
    .unwrap_or_else(|e| panic!("{e}"));

    // then
    assert!(matches!(applied, Applied::Completed { .. }), "{applied:?}");
    assert_eq!(plan_state(&kit).phase, PlanPhase::Completed);
    assert!(adopted(&kit, 1) && adopted(&kit, 2));
    let run = kit.hostrun();
    assert_eq!(run.safe_mode, None);
    assert!(
        run.restart_required,
        "the run restarts before recovery completes"
    );
    assert_eq!(run.phase, RunPhase::Recovering);
}

#[test]
fn should_record_the_plan_refused_when_a_bound_stream_moved_before_the_action() {
    // given another writer moved a bound stream after the plan was drawn
    let (kit, epoch) = approved();
    foreign_write(&kit);

    // when
    let applied = block_on(
        Repair::new(&kit.store, crate::recovery_kit::host(), epoch).apply(
            plan_id(),
            world(),
            &Structure(true),
        ),
    )
    .unwrap_or_else(|e| panic!("{e}"));

    // then the plan unit, read at the current heads, committed and recorded the refusal
    assert_eq!(applied, Applied::Refused(StaleBinding::JournalHeads));
    assert_eq!(
        plan_state(&kit).phase,
        PlanPhase::Refused(StaleBinding::JournalHeads)
    );
    assert!(!adopted(&kit, 1), "no action ran on a moved world (F45)");
}

#[test]
fn should_refuse_the_attempt_command_and_then_the_plan_when_another_writer_moves_a_bound_stream() {
    // given another writer appends to attempt 2 between action 0's intent and its command
    let (kit, epoch) = approved();
    let other = kit.memory.clone();
    kit.journal.before_next_append(BeforeAppend::appending(
        stream(&format!("attempt-{}", attempt(1))),
        move || {
            let kit = kit_over(&other);
            foreign_write(&kit);
        },
    ));

    // when
    let applied = block_on(
        Repair::new(&kit.store, crate::recovery_kit::host(), epoch).apply(
            plan_id(),
            world(),
            &Structure(true),
        ),
    )
    .unwrap_or_else(|e| panic!("{e}"));

    // then the command, conditioned on the bound heads, was refused at commit; the next
    // BeginAction, conditioned on the current heads, was accepted and recorded the refusal
    assert_eq!(applied, Applied::Refused(StaleBinding::JournalHeads));
    assert!(!adopted(&kit, 1), "the attempt command never ran");
    assert!(!adopted(&kit, 2));
    let events = plan_events(&kit);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, RecoveryPlanEvent::RecoveryActionStarted(s) if s.index == 0))
    );
    assert!(matches!(
        events.last(),
        Some(RecoveryPlanEvent::RecoveryPlanRefused(r)) if r.stale == StaleBinding::JournalHeads
    ));
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, RecoveryPlanEvent::RecoveryActionRecorded(_))),
        "no outcome and no further action"
    );
}

#[test]
fn should_refuse_the_plan_when_it_is_applied_under_a_new_run_epoch() {
    // given
    let (kit, epoch) = approved();
    let next = epoch.next().unwrap_or(epoch);

    // when
    let applied = block_on(
        Repair::new(&kit.store, crate::recovery_kit::host(), next).apply(
            plan_id(),
            world(),
            &Structure(true),
        ),
    )
    .unwrap_or_else(|e| panic!("{e}"));

    // then
    assert_eq!(applied, Applied::Refused(StaleBinding::Epoch));
    assert!(!adopted(&kit, 1));
}

#[test]
fn should_record_the_plans_own_write_when_an_action_applies() {
    // given
    let (kit, epoch) = approved();
    let bound = head(&kit, 1);

    // when
    let _ = block_on(
        Repair::new(&kit.store, crate::recovery_kit::host(), epoch).apply(
            plan_id(),
            world(),
            &Structure(true),
        ),
    )
    .unwrap_or_else(|e| panic!("{e}"));

    // then the outcome carries the attempt unit's receipt: before the bound head, after its own
    let write = plan_events(&kit).into_iter().find_map(|e| match e {
        RecoveryPlanEvent::RecoveryActionRecorded(r) if r.index == 0 => r.write,
        _ => None,
    });
    let write = write.unwrap_or_else(|| panic!("no outcome of action 0"));
    assert_eq!(write.before, bound);
    assert_eq!(write.after, head(&kit, 1));
}

#[test]
fn should_complete_the_plan_when_an_application_resumes_after_its_outcome_failed() {
    // given action 0's attempt command commits, then the store fails its outcome
    let (kit, epoch) = approved();
    let journal = kit.journal.clone();
    let memory = kit.memory.clone();
    kit.journal.before_next_append(BeforeAppend::appending(
        stream(&format!("attempt-{}", attempt(1))),
        move || {
            journal.before_next_append(BeforeAppend::appending(
                stream(&format!("recovery-{}", plan_id())),
                move || {
                    assert!(memory.inject(InjectedFault::StorageBeforeCommit));
                },
            ));
        },
    ));
    let repair = Repair::new(&kit.store, crate::recovery_kit::host(), epoch);
    let failed = block_on(repair.apply(plan_id(), world(), &Structure(true)));
    assert!(failed.is_err(), "the outcome's unit failed: {failed:?}");
    assert!(plan_state(&kit).intent_open);

    // when the application resumes
    let applied = block_on(repair.apply(plan_id(), world(), &Structure(true)))
        .unwrap_or_else(|e| panic!("{e}"));

    // then the attempt command was answered by its receipt and the plan completed
    assert!(matches!(applied, Applied::Completed { .. }), "{applied:?}");
    let adoptions = kit
        .events(1)
        .iter()
        .filter(|e| matches!(e, AttemptEvent::AttemptAdopted(_)))
        .count();
    assert_eq!(adoptions, 1, "adopted once");
}

fn apply(kit: &Kit, epoch: Epoch) -> Applied {
    block_on(
        Repair::new(&kit.store, crate::recovery_kit::host(), epoch).apply(
            plan_id(),
            world(),
            &Structure(true),
        ),
    )
    .unwrap_or_else(|e| panic!("{e}"))
}

#[test]
fn should_mark_lost_and_quarantine_through_ordinary_commands_when_the_plan_says_so() {
    // given
    let (kit, epoch) = approved_with(&[
        ActionKind::MarkLost,
        ActionKind::Quarantine(QuarantineReason::OwnershipDoubt),
    ]);

    // when
    let applied = apply(&kit, epoch);

    // then
    assert!(matches!(applied, Applied::Completed { .. }), "{applied:?}");
    assert_eq!(kit.state(1).phase, Phase::Lost, "its VM vanished (DEC-07)");
    assert!(matches!(
        kit.state(2).phase,
        Phase::Quarantined {
            reason: QuarantineReason::OwnershipDoubt,
            ..
        }
    ));
}

#[test]
fn should_stop_at_an_action_without_a_command_when_the_plan_holds_one() {
    // given
    let (kit, epoch) = approved_with(&[ActionKind::Report]);

    // when
    let applied = apply(&kit, epoch);

    // then
    assert_eq!(applied, Applied::NoCommand { index: 0 });
}

#[test]
fn should_stop_the_plan_resumable_when_an_actions_command_is_refused() {
    // given attempt 1 is adopted under the run already, so its adoption is refused
    let (kit, epoch) = approved();
    let _ = kit.command(
        1,
        epoch,
        AttemptCommand::Adopt {
            inventory_ref: mayfly_domain::EventId::from_u128(1),
        },
    );

    // when
    let applied = apply(&kit, epoch);

    // then the moved head refuses the plan first: the world moved under it
    assert_eq!(applied, Applied::Refused(StaleBinding::JournalHeads));
}

#[test]
fn should_record_the_failure_when_the_attempt_refuses_on_an_unmoved_world() {
    // given a keep of an attempt the run adopted before the plan was drawn
    let kit = kit();
    kit.previous_run();
    kit.drive(1, path::prepared(1));
    let (opened, _) = kit.open(7);
    let _ = kit.command(
        1,
        opened.epoch,
        AttemptCommand::Adopt {
            inventory_ref: mayfly_domain::EventId::from_u128(1),
        },
    );
    let bindings = Bindings {
        heads: BTreeMap::from([(format!("attempt-{}", attempt(1)), head(&kit, 1))]),
        inventory: world().inventory,
        github: world().github,
        epoch: opened.epoch,
    };
    plan(
        &kit,
        opened.epoch,
        0xD3,
        Actor::Startup,
        RecoveryPlanCommand::ProposePlan {
            bindings,
            actions: vec![PlanAction {
                attempt_id: attempt(1),
                instance_id: Some(instance()),
                kind: ActionKind::Keep,
            }],
            expires_at: Millis::new(NOW.get() + 3_600_000),
        },
    );
    plan(
        &kit,
        opened.epoch,
        0xD4,
        Actor::Operator { uid: 501 },
        RecoveryPlanCommand::ApprovePlan,
    );

    // when
    let applied = apply(&kit, opened.epoch);

    // then
    assert_eq!(
        applied,
        Applied::Failed {
            index: 0,
            refused: Some("already_current"),
        }
    );
    assert_eq!(plan_state(&kit).phase, PlanPhase::Failed);
}

#[test]
fn should_apply_nothing_when_no_plan_is_approved() {
    // given
    let kit = kit();
    let (opened, _) = kit.open(7);

    // when
    let applied = apply(&kit, opened.epoch);

    // then
    assert_eq!(applied, Applied::NotApplicable);
}

#[test]
fn should_say_what_stopped_the_application_when_each_error_is_displayed() {
    // given
    let errors = [
        RepairError::Store(mayfly_control::store::SaveError::Busy),
        RepairError::UnboundStream("attempt-1".to_owned()),
        RepairError::Refused("closed"),
    ];

    // when
    let shown: Vec<String> = errors.iter().map(ToString::to_string).collect();

    // then
    assert!(shown[1].contains("attempt-1"));
    assert!(shown[2].contains("closed"));
    let kit = kit();
    let repair = Repair::new(&kit.store, crate::recovery_kit::host(), Epoch::FIRST);
    assert!(format!("{repair:?}").contains("Repair"));
}

#[test]
fn should_leave_the_attempt_unadopted_when_its_quarantine_would_be_refused() {
    // given a quarantine of an attempt whose clone is in flight
    let (kit, epoch) = approved_on(
        path::cloning(2),
        &[
            ActionKind::Adopt,
            ActionKind::Quarantine(QuarantineReason::OwnershipDoubt),
        ],
    );

    // when
    let applied = apply(&kit, epoch);

    // then the plan failed there and the attempt was not adopted under it
    assert_eq!(
        applied,
        Applied::Failed {
            index: 1,
            refused: Some("directive_pending"),
        }
    );
    assert!(adopted(&kit, 1));
    assert!(!adopted(&kit, 2), "no adoption left behind a failed plan");
}

#[test]
fn should_complete_the_quarantine_when_the_application_resumes_between_its_two_units() {
    // given the quarantine's unit fails after its adoption committed
    let (kit, epoch) = approved_with(&[
        ActionKind::Adopt,
        ActionKind::Quarantine(QuarantineReason::OwnershipDoubt),
    ]);
    let journal = kit.journal.clone();
    let memory = kit.memory.clone();
    let second = stream(&format!("attempt-{}", attempt(2)));
    let again = second.clone();
    kit.journal
        .before_next_append(BeforeAppend::appending(second, move || {
            journal.before_next_append(BeforeAppend::appending(again, move || {
                assert!(memory.inject(InjectedFault::StorageBeforeCommit));
            }));
        }));
    let repair = Repair::new(&kit.store, crate::recovery_kit::host(), epoch);
    let crashed = block_on(repair.apply(plan_id(), world(), &Structure(true)));
    assert!(crashed.is_err(), "{crashed:?}");
    assert!(adopted(&kit, 2), "the first unit committed");

    // when the application resumes
    let applied = block_on(repair.apply(plan_id(), world(), &Structure(true)))
        .unwrap_or_else(|e| panic!("{e}"));

    // then
    assert!(matches!(applied, Applied::Completed { .. }), "{applied:?}");
    assert!(matches!(kit.state(2).phase, Phase::Quarantined { .. }));
    let adoptions = kit
        .events(2)
        .iter()
        .filter(|e| matches!(e, AttemptEvent::AttemptAdopted(_)))
        .count();
    assert_eq!(adoptions, 1);
}

#[test]
fn should_fail_with_the_adoptions_refusal_when_the_attempt_cannot_be_adopted() {
    // given a quarantine of an attempt whose VM was found lost (terminal)
    let mut lost = path::prepared(2);
    lost.push(path::observe(2, 9, Fact::VmAbsent));
    let (kit, epoch) = approved_on(
        lost,
        &[
            ActionKind::Adopt,
            ActionKind::Quarantine(QuarantineReason::OwnershipDoubt),
        ],
    );
    assert_eq!(kit.state(2).phase, Phase::Lost);

    // when
    let applied = apply(&kit, epoch);

    // then the adoption's own refusal stops the plan, not one decided on the unadopted state
    assert_eq!(
        applied,
        Applied::Failed {
            index: 1,
            refused: Some("terminal"),
        }
    );
    assert!(!adopted(&kit, 2));
}
