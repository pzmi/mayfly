//! Component tests of the data plane's executor (ver §2): the real executor and domain over
//! the data-plane kit's fakes, a fake clock and an in-memory link, on one task.

mod effects;
mod session;
mod support;

use mayfly_data::{Ambiguity, SessionEnd, Stats};
use mayfly_domain::Millis;
use mayfly_protocol::{
    DirectiveKind, InventoryGap, LinkError, ObservationKind, RefusalReason, ResourceDimension,
    UnknownCause, UnknownReason,
};
use mayfly_testkit_data::{FakeVm, Fault, Op};

use support::{
    DEADLINE, Fakes, GIB, LISTING, OBSERVE, answers, answers_to, clone, described, directive, due,
    instance, probe, proof, run, session, vm,
};

fn closed() -> SessionEnd {
    SessionEnd::SourceClosed(LinkError::Closed)
}

#[test]
fn should_execute_once_when_a_directive_is_delivered_twice() {
    run(async {
        // given
        let fakes = Fakes::new(probe(4));
        let mut exec = fakes.open(2).await;
        let held = fakes.provider.hold_next(Op::Clone);

        // when
        let (end, seen) = session(&mut exec, |feed, mut tap| async move {
            feed.send(directive(1, clone(1))).await;
            held.started().await;
            feed.send(directive(1, clone(1))).await;
            feed.send(directive(2, DirectiveKind::ReportCapacity)).await;
            tap.wait_for(answers(2)).await;
            held.release();
            tap.wait_for(answers(1)).await;
            feed.send(directive(1, clone(1))).await;
            tap.wait_for(answers(1)).await;
            tap
        })
        .await;

        // then
        assert_eq!(end, closed());
        assert_eq!(fakes.provider.calls(Op::Clone), 1);
        let answered = answers_to(&seen, 1);
        assert_eq!(answered.len(), 2, "{answered:?}");
        assert!(matches!(answered[0], ObservationKind::VmCloned { .. }));
        assert_eq!(answered[0], answered[1]);
    });
}

#[test]
fn should_answer_unknown_never_success_when_the_provider_error_is_ambiguous() {
    run(async {
        // given
        let fakes = Fakes::new(probe(4));
        let mut exec = fakes.open(2).await;
        fakes.provider.fail_next(
            Op::Clone,
            Fault::Ambiguous {
                cause: Ambiguity::ProcessLost,
                applied: true,
            },
        );
        let clock = fakes.clock.clone();
        let provider = fakes.provider.clone();

        // when
        let (end, seen) = session(&mut exec, |feed, mut tap| async move {
            feed.send(directive(1, clone(1))).await;
            tap.wait_for(answers(1)).await;
            let hung = provider.hold_next(Op::Clone);
            feed.send(directive(2, clone(2))).await;
            hung.started().await;
            clock.advance(DEADLINE);
            tap.wait_for(answers(2)).await;
            hung.release();
            let listed = vm(2).to_string();
            tap.wait_for(|o| match &o.kind {
                ObservationKind::Inventory { vms, .. } => vms.iter().any(|v| v.name == listed),
                _ => false,
            })
            .await;
            tap
        })
        .await;

        // then
        assert_eq!(end, closed());
        assert_eq!(
            answers_to(&seen, 1),
            [ObservationKind::OutcomeUnknown {
                reason: UnknownReason::ProcessLost
            }]
        );
        assert_eq!(
            answers_to(&seen, 2),
            [ObservationKind::OutcomeUnknown {
                reason: UnknownReason::Deadline
            }]
        );
        assert!(fakes.provider.vms().contains_key(&vm(1).to_string()));
        let ledger = exec.ledger();
        assert!(
            ledger.vms.contains_key(&vm(1))
                && ledger.vms.contains_key(&vm(2))
                && ledger.reservations.is_empty(),
            "an unknown clone is counted from the next complete listing, not from its answer"
        );
    });
}

#[test]
fn should_rebuild_the_ledger_from_observation_when_the_executor_restarts() {
    run(async {
        // given
        let fakes = Fakes::new(probe(4));
        fakes
            .provider
            .seed(&vm(1).to_string(), described("0a:00:00:00:01:01", true));
        fakes
            .provider
            .seed(&vm(2).to_string(), described("0a:00:00:00:01:02", false));
        fakes
            .provider
            .seed("someone-elses-vm", described("0a:01", true));
        let mut first = fakes.open(2).await;
        let (_, seen) = session(&mut first, |feed, mut tap| async move {
            feed.send(directive(1, clone(3))).await;
            tap.wait_for(answers(1)).await;
            tap
        })
        .await;
        drop(first);

        // when
        let restarted = fakes.start(2).await;

        // then
        let Some(ObservationKind::CapacityReported(opened)) = seen.first().map(|o| o.kind.clone())
        else {
            panic!("a session opens with a capacity report: {seen:?}");
        };
        assert_eq!(opened.used.vm_slots, 2);
        assert_eq!(opened.used.resources.memory_bytes, 8 * GIB);
        assert!(opened.unknown.is_empty(), "{:?}", opened.unknown);
        let ledger = restarted.ledger();
        assert_eq!(
            ledger.vms.keys().cloned().collect::<Vec<_>>(),
            [vm(1), vm(2), vm(3)]
        );
        assert!(ledger.reservations.is_empty());
        assert!(ledger.unknown().is_empty());
        assert_eq!(restarted.plane().floor, support::EPOCH);
        assert_eq!(restarted.plane().session_epoch(), None);
    });
}

#[test]
fn should_refuse_what_does_not_fit_when_the_control_plane_over_allocates() {
    run(async {
        // given
        let fakes = Fakes::new(probe(2));
        fakes
            .provider
            .seed(&vm(1).to_string(), described("0a:00:00:00:01:01", true));
        fakes
            .provider
            .seed(&vm(2).to_string(), described("0a:00:00:00:01:02", false));
        let mut exec = fakes.open(2).await;

        // when
        let (_, seen) = session(&mut exec, |feed, mut tap| async move {
            feed.send(directive(1, clone(3))).await;
            tap.wait_for(answers(1)).await;
            tap
        })
        .await;

        // then
        assert_eq!(
            answers_to(&seen, 1),
            [ObservationKind::DirectiveRefused {
                reason: RefusalReason::InsufficientResources {
                    dimension: ResourceDimension::VmSlots
                }
            }]
        );
        assert_eq!(fakes.provider.calls(Op::Clone), 0);
    });
}

#[test]
fn should_discard_the_listing_when_an_effect_overlaps_it() {
    run(async {
        // given: the delete re-observes, then deletes while an observer pass lists
        let fakes = Fakes::new(probe(4));
        let one = "0a:00:00:00:01:01";
        fakes
            .provider
            .seed(&vm(1).to_string(), described(one, false));
        let mut exec = fakes.open(2).await;
        let reobservation = fakes.provider.hold_next(Op::Inventory);
        let pass = fakes.provider.hold_next(Op::Inventory);
        let delete = DirectiveKind::DeleteVm {
            proof: proof(1, &instance(one)),
        };

        // when
        let (_, seen) = session(&mut exec, |feed, mut tap| async move {
            feed.send(directive(1, delete)).await;
            reobservation.started().await;
            feed.send(directive(2, DirectiveKind::ReportInventory))
                .await;
            pass.started().await;
            reobservation.release();
            tap.wait_for(answers(1)).await;
            pass.release();
            tap.wait_for(answers(2)).await;
            tap
        })
        .await;

        // then
        let name = vm(1).to_string();
        let lists_deleted = |kind: &ObservationKind| match kind {
            ObservationKind::Inventory { vms, .. } => vms.iter().any(|v| v.name == name),
            _ => false,
        };
        assert_eq!(answers_to(&seen, 1), [ObservationKind::VmDeleted]);
        assert!(
            !seen.iter().any(|o| lists_deleted(&o.kind)),
            "the listing read before the delete is never reported: {seen:?}"
        );
        assert!(matches!(
            answers_to(&seen, 2).as_slice(),
            [ObservationKind::Inventory { .. }]
        ));
        assert_eq!(
            exec.stats(),
            Stats {
                discarded_listings: 1,
                ..Stats::default()
            }
        );
        assert_eq!(fakes.provider.calls(Op::Inventory), 4);
        assert!(!exec.ledger().vms.contains_key(&vm(1)));
    });
}

#[test]
fn should_list_before_further_effects_when_a_pass_is_due() {
    run(async {
        // given
        let fakes = Fakes::new(probe(4));
        let mut exec = fakes.open(1).await;
        let held = fakes.provider.hold_next(Op::Clone);

        // when
        let (_, seen) = session(&mut exec, |feed, mut tap| async move {
            feed.send(directive(1, clone(1))).await;
            held.started().await;
            feed.send(directive(2, DirectiveKind::ReportInventory))
                .await;
            feed.send(directive(3, clone(3))).await;
            feed.send(directive(4, DirectiveKind::ReportCapacity)).await;
            tap.wait_for(answers(4)).await;
            held.release();
            tap.wait_for(answers(3)).await;
            tap
        })
        .await;

        // then
        let order: Vec<u128> = seen
            .iter()
            .filter_map(|o| o.answers.map(|a| a.directive_id.as_u128()))
            .collect();
        assert_eq!(
            order,
            [4, 1, 2, 3],
            "the due listing runs before the next clone"
        );
        let first = vm(1).to_string();
        assert!(matches!(
            answers_to(&seen, 2).as_slice(),
            [ObservationKind::Inventory { vms, .. }] if vms.iter().any(|v| v.name == first)
        ));
        assert_eq!(exec.stats(), Stats::default(), "no listing was overlapped");
    });
}

#[test]
fn should_hold_the_target_and_capacity_when_an_effect_runs_past_its_deadline() {
    run(async {
        // given
        let fakes = Fakes::new(probe(4));
        fakes
            .provider
            .seed(&vm(1).to_string(), described("0a:00:00:00:01:01", false));
        let mut exec = fakes.open(2).await;
        let held = fakes.provider.hold_next(Op::Start);
        let clock = fakes.clock.clone();
        let start = DirectiveKind::StartVm {
            vm_name: vm(1),
            run_options: support::run_options(),
        };
        let verify = DirectiveKind::VerifyGuest {
            vm_name: vm(1),
            nonce: mayfly_protocol::Nonce::from_u128(3),
        };
        let inventory = |o: &mayfly_protocol::Observation| {
            o.answers.is_none() && matches!(o.kind, ObservationKind::Inventory { .. })
        };

        // when
        let (_, seen) = session(&mut exec, |feed, mut tap| async move {
            feed.send(due(1, start, Millis::new(5_000))).await;
            held.started().await;
            clock.advance(Millis::new(5_000));
            tap.wait_for(answers(1)).await;
            clock.advance(OBSERVE);
            feed.send(directive(2, verify)).await;
            feed.send(directive(3, DirectiveKind::ReportCapacity)).await;
            tap.wait_for(answers(3)).await;
            let before = tap.drain();
            assert!(
                answers_to(&before, 2).is_empty(),
                "the verify waits while the start may still run on its VM"
            );
            assert!(
                !before.iter().any(inventory),
                "no listing is taken while the start may still run"
            );
            let held_memory = before.iter().rev().find_map(|o| match &o.kind {
                ObservationKind::CapacityReported(report) => {
                    Some(report.used.resources.memory_bytes)
                }
                _ => None,
            });
            assert_eq!(held_memory, Some(8 * GIB), "its capacity stays reserved");
            held.release();
            held.finished().await;
            tap.wait_for(answers(2)).await;
            tap.wait_for(inventory).await;
            tap
        })
        .await;

        // then
        assert_eq!(
            answers_to(&seen, 1),
            [ObservationKind::OutcomeUnknown {
                reason: UnknownReason::Deadline
            }],
            "answered at its deadline, once, never success"
        );
        let ledger = exec.ledger();
        assert!(
            ledger.vms[&vm(1)].running,
            "counted from the listing once it settled"
        );
        assert!(ledger.reservations.is_empty());
    });
}

#[test]
fn should_reobserve_with_the_provider_s_inventory_when_a_stop_or_delete_arrives() {
    run(async {
        // given
        let fakes = Fakes::new(probe(4));
        let (one, two) = ("0a:00:00:00:01:01", "0a:00:00:00:01:02");
        fakes
            .provider
            .seed(&vm(1).to_string(), described(one, false));
        fakes
            .provider
            .seed(&vm(2).to_string(), described(two, true));
        let mut exec = fakes.open(2).await;
        let delete = DirectiveKind::DeleteVm {
            proof: proof(1, &instance(one)),
        };
        let stop = DirectiveKind::StopVm {
            grace: Millis::new(100),
            proof: proof(2, &instance(two)),
        };

        // when
        let (_, seen) = session(&mut exec, |feed, mut tap| async move {
            feed.send(directive(1, delete)).await;
            tap.wait_for(answers(1)).await;
            feed.send(directive(2, stop)).await;
            tap.wait_for(answers(2)).await;
            tap
        })
        .await;

        // then
        assert_eq!(answers_to(&seen, 1), [ObservationKind::VmDeleted]);
        assert_eq!(answers_to(&seen, 2), [ObservationKind::VmStopped]);
        assert_eq!(
            fakes.provider.log(),
            [
                Op::Inventory,
                Op::Inventory,
                Op::Delete,
                Op::Inventory,
                Op::Stop
            ]
        );
        assert!(!fakes.provider.vms().contains_key(&vm(1).to_string()));
    });
}

#[test]
fn should_never_touch_the_target_when_the_reobservation_cannot_prove_it() {
    run(async {
        // given
        let fakes = Fakes::new(probe(4));
        let (one, two) = ("0a:00:00:00:01:01", "0a:00:00:00:01:02");
        fakes
            .provider
            .seed(&vm(1).to_string(), described(one, true));
        fakes
            .provider
            .seed(&vm(2).to_string(), described(two, true));
        fakes
            .provider
            .set_instance(&vm(1).to_string(), Some(instance("0a:ff")));
        let mut exec = fakes.open(2).await;
        let provider = fakes.provider.clone();
        let delete = DirectiveKind::DeleteVm {
            proof: proof(1, &instance(one)),
        };
        let stop = DirectiveKind::StopVm {
            grace: Millis::new(100),
            proof: proof(2, &instance(two)),
        };

        // when
        let (_, seen) = session(&mut exec, |feed, mut tap| async move {
            feed.send(directive(1, delete)).await;
            tap.wait_for(answers(1)).await;
            provider.fail_next(Op::Inventory, Fault::Incomplete(InventoryGap::Unparseable));
            feed.send(directive(2, stop)).await;
            tap.wait_for(answers(2)).await;
            tap
        })
        .await;

        // then
        assert_eq!(
            answers_to(&seen, 1),
            [ObservationKind::DirectiveRefused {
                reason: RefusalReason::ProofMismatch
            }]
        );
        assert_eq!(
            answers_to(&seen, 2),
            [ObservationKind::OutcomeUnknown {
                reason: UnknownReason::ListingIncomplete
            }]
        );
        assert_eq!(fakes.provider.calls(Op::Delete), 0);
        assert_eq!(fakes.provider.calls(Op::Stop), 0);
        assert_eq!(fakes.provider.vms().len(), 2);
    });
}

#[test]
fn should_answer_done_without_an_effect_when_the_outcome_already_holds() {
    run(async {
        // given
        let fakes = Fakes::new(probe(4));
        let stopped = "0a:00:00:00:01:06";
        fakes
            .provider
            .seed(&vm(6).to_string(), described(stopped, false));
        let mut exec = fakes.open(2).await;
        let delete = DirectiveKind::DeleteVm {
            proof: proof(5, &instance("0a:00:00:00:01:05")),
        };
        let stop = DirectiveKind::StopVm {
            grace: Millis::new(100),
            proof: proof(6, &instance(stopped)),
        };

        // when
        let (_, seen) = session(&mut exec, |feed, mut tap| async move {
            feed.send(directive(1, delete)).await;
            feed.send(directive(2, stop)).await;
            tap.wait_for(answers(1)).await;
            tap.wait_for(answers(2)).await;
            tap
        })
        .await;

        // then
        assert_eq!(answers_to(&seen, 1), [ObservationKind::VmDeleted]);
        assert_eq!(answers_to(&seen, 2), [ObservationKind::VmStopped]);
        assert_eq!(fakes.provider.calls(Op::Delete), 0);
        assert_eq!(fakes.provider.calls(Op::Stop), 0);
    });
}

#[test]
fn should_refuse_capacity_unknown_when_a_vm_s_facts_are_missing() {
    run(async {
        // given
        let fakes = Fakes::new(probe(4));
        let undescribed = FakeVm {
            memory_bytes: None,
            ..described("0a:00:00:00:01:01", true)
        };
        fakes.provider.seed(&vm(1).to_string(), undescribed);
        fakes
            .provider
            .seed(&vm(2).to_string(), described("0a:00:00:00:01:02", false));
        let mut exec = fakes.open(2).await;
        let start = DirectiveKind::StartVm {
            vm_name: vm(2),
            run_options: support::run_options(),
        };

        // when
        let (_, seen) = session(&mut exec, |feed, mut tap| async move {
            feed.send(directive(1, start)).await;
            tap.wait_for(answers(1)).await;
            tap
        })
        .await;

        // then
        assert_eq!(
            answers_to(&seen, 1),
            [ObservationKind::DirectiveRefused {
                reason: RefusalReason::CapacityUnknown {
                    dimension: ResourceDimension::Memory
                }
            }]
        );
        assert_eq!(fakes.provider.calls(Op::Start), 0);
        let Some(ObservationKind::CapacityReported(opened)) = seen.first().map(|o| o.kind.clone())
        else {
            panic!("a session opens with a capacity report: {seen:?}");
        };
        let memory = opened
            .unknown
            .iter()
            .find(|u| u.dimension == ResourceDimension::Memory)
            .expect("memory is unknown");
        assert!(memory.causes.contains(&UnknownCause::VmFactUnknown {
            vm_name: vm(1).to_string()
        }));
        assert_eq!(opened.free.resources.memory_bytes, 0);
    });
}

#[test]
fn should_run_at_most_the_bound_at_once_when_directives_queue() {
    run(async {
        // given
        let fakes = Fakes::new(probe(4));
        let mut exec = fakes.open(1).await;
        let held = fakes.provider.hold_next(Op::Clone);
        let provider = fakes.provider.clone();

        // when
        let (_, seen) = session(&mut exec, |feed, mut tap| async move {
            feed.send(directive(1, clone(1))).await;
            feed.send(directive(2, clone(2))).await;
            held.started().await;
            feed.send(directive(3, DirectiveKind::ReportCapacity)).await;
            tap.wait_for(answers(3)).await;
            assert_eq!(
                provider.calls(Op::Clone),
                1,
                "the second waits for the first"
            );
            held.release();
            tap.wait_for(answers(2)).await;
            tap
        })
        .await;

        // then
        assert!(matches!(
            answers_to(&seen, 1).as_slice(),
            [ObservationKind::VmCloned { .. }]
        ));
        assert!(matches!(
            answers_to(&seen, 2).as_slice(),
            [ObservationKind::VmCloned { .. }]
        ));
        assert_eq!(fakes.provider.calls(Op::Clone), 2);
    });
}

#[test]
fn should_run_one_directive_per_vm_at_a_time_when_two_target_it() {
    run(async {
        // given
        let fakes = Fakes::new(probe(4));
        let mut exec = fakes.open(4).await;
        let held = fakes.provider.hold_next(Op::Clone);
        let verify = DirectiveKind::VerifyGuest {
            vm_name: vm(1),
            nonce: mayfly_protocol::Nonce::from_u128(9),
        };

        // when
        let (_, seen) = session(&mut exec, |feed, mut tap| async move {
            feed.send(directive(1, clone(1))).await;
            feed.send(directive(2, verify)).await;
            held.started().await;
            feed.send(directive(3, DirectiveKind::ReportCapacity)).await;
            tap.wait_for(answers(3)).await;
            held.release();
            tap.wait_for(answers(2)).await;
            tap
        })
        .await;

        // then
        let order: Vec<u128> = seen
            .iter()
            .filter_map(|o| o.answers.map(|a| a.directive_id.as_u128()))
            .collect();
        assert_eq!(order, [3, 1, 2]);
    });
}

#[test]
fn should_not_start_a_directive_when_its_deadline_passed_while_it_waited() {
    run(async {
        // given
        let fakes = Fakes::new(probe(4));
        let mut exec = fakes.open(1).await;
        let held = fakes.provider.hold_next(Op::Clone);
        let clock = fakes.clock.clone();
        let soon = Millis::new(2_000);
        let delete = DirectiveKind::DeleteVm {
            proof: proof(9, &instance("0a:09")),
        };

        // when
        let (_, seen) = session(&mut exec, |feed, mut tap| async move {
            feed.send(directive(1, clone(1))).await;
            held.started().await;
            feed.send(due(2, clone(2), soon)).await;
            feed.send(due(3, delete, soon)).await;
            feed.send(directive(4, DirectiveKind::ReportCapacity)).await;
            tap.wait_for(answers(4)).await;
            clock.advance(Millis::new(5_000));
            held.release();
            tap.wait_for(answers(3)).await;
            tap
        })
        .await;

        // then
        assert_eq!(
            answers_to(&seen, 2),
            [ObservationKind::DirectiveRefused {
                reason: RefusalReason::DeadlineExpired
            }]
        );
        assert_eq!(
            answers_to(&seen, 3),
            [ObservationKind::DirectiveRefused {
                reason: RefusalReason::DeadlineExpired
            }]
        );
        assert_eq!(fakes.provider.calls(Op::Clone), 1);
        assert_eq!(fakes.provider.calls(Op::Delete), 0);
        assert!(exec.ledger().reservations.is_empty());
    });
}

#[test]
fn should_report_the_inventory_and_capacity_when_the_observer_runs() {
    run(async {
        // given
        let fakes = Fakes::new(probe(4));
        let mut exec = fakes.open(2).await;
        let (clock, provider, host) = (
            fakes.clock.clone(),
            fakes.provider.clone(),
            fakes.host.clone(),
        );
        let unsolicited_inventory = |o: &mayfly_protocol::Observation| {
            o.answers.is_none() && matches!(o.kind, ObservationKind::Inventory { .. })
        };
        let unsolicited_gap = |o: &mayfly_protocol::Observation| {
            o.answers.is_none() && matches!(o.kind, ObservationKind::InventoryIncomplete { .. })
        };

        // when
        let (_, seen) = session(&mut exec, |feed, mut tap| async move {
            let _open = feed;
            tap.wait_for(|o| matches!(o.kind, ObservationKind::CapacityReported(_)))
                .await;
            clock.advance(OBSERVE);
            tap.wait_for(unsolicited_inventory).await;
            provider.fail_next(Op::Inventory, Fault::Definite);
            host.fail(true);
            clock.advance(OBSERVE);
            tap.wait_for(unsolicited_gap).await;
            tap
        })
        .await;

        // then
        let reports: Vec<_> = seen
            .iter()
            .filter_map(|o| match &o.kind {
                ObservationKind::CapacityReported(report) => Some(report.clone()),
                _ => None,
            })
            .collect();
        let last = reports.last().expect("capacity was reported");
        assert!(reports.len() >= 2, "on the handshake and on the change");
        assert!(reports.windows(2).all(|w| w[0].report_id < w[1].report_id));
        assert_eq!(last.unknown.len(), 4, "a failed probe sizes no foreign VM");
        assert!(
            last.unknown
                .iter()
                .all(|u| u.causes.contains(&UnknownCause::ForeignVmUnsized))
        );
        assert!(seen.iter().any(|o| o.kind
            == ObservationKind::InventoryIncomplete {
                reason: InventoryGap::ListFailed
            }));
    });
}

#[test]
fn should_answer_an_incomplete_inventory_when_no_listing_finishes_by_the_deadline() {
    run(async {
        // given
        let fakes = Fakes::new(probe(4));
        let mut exec = fakes.open(2).await;
        let hung = fakes.provider.hold_next(Op::Inventory);
        let clock = fakes.clock.clone();
        let report = due(1, DirectiveKind::ReportInventory, Millis::new(2_000));

        // when
        let (_, seen) = session(&mut exec, |feed, mut tap| async move {
            feed.send(report).await;
            hung.started().await;
            clock.advance(Millis::new(1_500));
            tap.wait_for(answers(1)).await;
            clock.advance(LISTING);
            tap.wait_for(|o| {
                o.answers.is_none() && matches!(o.kind, ObservationKind::InventoryIncomplete { .. })
            })
            .await;
            tap
        })
        .await;

        // then
        assert_eq!(
            answers_to(&seen, 1),
            [ObservationKind::InventoryIncomplete {
                reason: InventoryGap::Deadline
            }]
        );
        assert!(
            exec.ledger().inventory_read,
            "the start's listing still stands"
        );
    });
}
