//! The lifecycle rules of plan §6, each the Rust side of an invariant of
//! `formal/lifecycle.qnt`.

use mayfly_control_domain::attempt::{
    AttemptCommand, AttemptEvent, AttemptRejection, AttemptState, DirectiveKind, Fact, Jit,
    Outcome, Phase, QuarantineReason, RefusalReason, RetireReason, RunnerRequestId, Stage,
    VmCondition,
};
use mayfly_control_domain::{Actor, Rejection};

use super::support::Step;
use super::support::{
    OPERATOR, SAGA, activating, apply, at, backoff_holding_registration, booting, busy, cloning,
    directive, doubted, event, facts, finished, handle, instance, issue, jit_issued, listening,
    observe, observe_in, prepared, quarantined, reach, retiring, saga, stimuli, stopped, then,
    unknown_clone, vanished, verified_activation, verifying,
};

fn issued_kind(events: &[AttemptEvent]) -> Option<DirectiveKind> {
    events.iter().find_map(|e| match e {
        AttemptEvent::DirectiveIssued(issued) => Some(issued.kind.clone()),
        AttemptEvent::AttemptStarted(..)
        | AttemptEvent::DirectiveSent(..)
        | AttemptEvent::DirectiveRefused(..)
        | AttemptEvent::DirectiveOutcomeUnknown(..)
        | AttemptEvent::DirectiveObservedNotApplied(..)
        | AttemptEvent::VmCloned(..)
        | AttemptEvent::VmBooted(..)
        | AttemptEvent::GuestVerified(..)
        | AttemptEvent::GuestRejected(..)
        | AttemptEvent::AttemptPrepared(..)
        | AttemptEvent::ActivationStarted(..)
        | AttemptEvent::JitHandleIssued(..)
        | AttemptEvent::JitHandleTaken(..)
        | AttemptEvent::JitHandleOrphaned(..)
        | AttemptEvent::RunnerLaunched(..)
        | AttemptEvent::RunnerLaunchUnknown(..)
        | AttemptEvent::JobBound(..)
        | AttemptEvent::RunnerExited(..)
        | AttemptEvent::VmObservedStopped(..)
        | AttemptEvent::AttemptBackedOff(..)
        | AttemptEvent::RetirementRequested(..)
        | AttemptEvent::RetirementWithdrawn(..)
        | AttemptEvent::AttemptRetired(..)
        | AttemptEvent::DiagnosticsExported(..)
        | AttemptEvent::DiagnosticsLost(..)
        | AttemptEvent::VmStopped(..)
        | AttemptEvent::VmDeleted(..)
        | AttemptEvent::AttemptLost(..)
        | AttemptEvent::AttemptQuarantined(..)
        | AttemptEvent::AttemptAdopted(..)
        | AttemptEvent::StaleObservationIgnored(..)
        | AttemptEvent::AttemptClosed(..) => None,
    })
}

fn code(result: Result<impl Sized, AttemptRejection>) -> Option<&'static str> {
    result.err().map(|rejection| rejection.code())
}

fn record_jit() -> AttemptCommand {
    AttemptCommand::RecordJitHandle {
        handle_id: handle(),
        registration_ref: event(2),
    }
}

#[test]
fn should_refuse_a_jit_handle_when_activation_lacks_a_grant_or_a_fresh_verification() {
    // given
    let (prepared, _) = reach(&prepared());
    let (unverified, _) = reach(&activating());
    let (verified, _) = reach(&verified_activation());
    let adopted = apply(
        &verified,
        at(
            2,
            Actor::Startup,
            AttemptCommand::Adopt {
                inventory_ref: event(3),
            },
        ),
    )
    .map(|(state, _)| state);
    // when
    let without_grant = apply(&prepared, saga(record_jit()));
    let without_verification = apply(&unverified, saga(record_jit()));
    let after_restart = adopted
        .clone()
        .and_then(|state| apply(&state, at(2, SAGA, record_jit())));
    let reverified = adopted
        .and_then(|state| apply(&state, at(2, SAGA, issue(9))))
        .and_then(|(state, events)| {
            assert_eq!(issued_kind(&events), Some(DirectiveKind::VerifyGuest));
            apply(
                &state,
                at(
                    2,
                    SAGA,
                    AttemptCommand::RecordObservation(
                        mayfly_control_domain::attempt::Observation {
                            observation_id: mayfly_domain::ObservationId::from_u128(0x0C1),
                            epoch: mayfly_domain::Epoch::new(2),
                            generation: mayfly_domain::Generation::FIRST,
                            fact: Fact::GuestVerified {
                                directive_id: directive(9),
                                facts: facts(),
                            },
                        },
                    ),
                ),
            )
        })
        .and_then(|(state, _)| apply(&state, at(2, SAGA, record_jit())));
    // then
    assert_eq!(code(without_grant), Some("no_grant"));
    assert_eq!(code(without_verification), Some("no_fresh_verification"));
    assert_eq!(code(after_restart), Some("no_fresh_verification"));
    assert_eq!(
        reverified.map(|(state, _)| state.jit),
        Ok(Jit::Issued(handle()))
    );
}

#[test]
fn should_leave_unknown_only_on_an_observation_when_every_command_is_tried() {
    // given
    let (unknown, _) = reach(&unknown_clone());
    let still_unknown = |state: &mayfly_control_domain::attempt::AttemptState| {
        matches!(
            state.pending.map(|p| p.outcome),
            Some(Outcome::Unknown { .. })
        )
    };
    // when
    let after_commands: Vec<(&str, bool)> = stimuli(&unknown)
        .into_iter()
        .filter(|(_, step)| !matches!(step.body, AttemptCommand::RecordObservation(..)))
        .map(|(name, step)| {
            let next = apply(&unknown, step).map_or(unknown.clone(), |(state, _)| state);
            (name, still_unknown(&next))
        })
        .collect();
    let repeated = apply(
        &unknown,
        saga(observe(Fact::OutcomeUnknown {
            directive_id: directive(1),
        })),
    );
    let observed = apply(
        &unknown,
        saga(observe(Fact::VmPresent {
            instance_id: instance(),
            running: false,
        })),
    );
    // then
    assert_eq!(after_commands.len(), 14);
    assert!(
        after_commands.iter().all(|(_, unknown)| *unknown),
        "{after_commands:?}"
    );
    assert!(repeated.is_ok_and(|(state, events)| events.is_empty() && still_unknown(&state)));
    let (state, _) = observed.unwrap_or_else(|r| panic!("{r:?}"));
    assert_eq!(state.pending, None);
    assert_eq!(state.phase, Phase::Booting);
    assert_eq!(state.instance_id, Some(instance()));
}

#[test]
fn should_keep_name_and_generation_when_a_stage_is_retried() {
    // given
    let (cloning, _) = reach(&cloning());
    let name = cloning.identity.clone().map(|i| (i.vm_name, i.generation));
    let refuse = |n| {
        saga(observe(Fact::DirectiveRefused {
            directive_id: directive(n),
            reason: mayfly_control_domain::attempt::RefusalReason::EffectFailed,
        }))
    };
    // when
    let first = apply(&cloning, refuse(1));
    let retried = first
        .clone()
        .and_then(|(state, _)| apply(&state, saga(issue(2))));
    let second = retried
        .clone()
        .and_then(|(state, _)| apply(&state, refuse(2)));
    // then
    let (backoff, _) = first.unwrap_or_else(|r| panic!("{r:?}"));
    assert!(matches!(backoff.phase, Phase::Backoff { n: 1, .. }));
    let (retry, events) = retried.unwrap_or_else(|r| panic!("{r:?}"));
    assert_eq!(issued_kind(&events), Some(DirectiveKind::CloneVm));
    assert_eq!(retry.phase, Phase::Cloning);
    assert_eq!(retry.identity.map(|i| (i.vm_name, i.generation)), name);
    let (spent, _) = second.unwrap_or_else(|r| panic!("{r:?}"));
    assert_eq!(spent.retire_reason, Some(RetireReason::StageFailed));
    assert_eq!(spent.phase, Phase::Deleted);
}

#[test]
fn should_bind_one_job_when_a_second_is_offered() {
    // given
    let (busy, _) = reach(&busy());
    let bind = |n| {
        saga(AttemptCommand::BindJob {
            request_id: RunnerRequestId(n),
        })
    };
    // when
    let other = apply(&busy, bind(2));
    let same = apply(&busy, bind(1));
    // then
    assert_eq!(code(other), Some("job_already_bound"));
    assert!(same.is_ok_and(|(state, events)| events.is_empty() && state == busy));
    assert_eq!(busy.job_binding, Some(RunnerRequestId(1)));
}

#[test]
fn should_never_return_to_prepared_when_a_runner_has_served() {
    // given
    let served = [
        reach(&listening()).0,
        reach(&busy()).0,
        reach(&finished()).0,
        reach(&retiring()).0,
    ];
    // when
    let reached: Vec<(Phase, &str, Phase)> = served
        .iter()
        .flat_map(|state| {
            stimuli(state).into_iter().filter_map(move |(name, step)| {
                apply(state, step)
                    .ok()
                    .map(|(next, _)| (state.phase, name, next.phase))
            })
        })
        .collect();
    // then
    assert!(!reached.is_empty());
    assert!(
        reached
            .iter()
            .all(|(_, _, next)| !matches!(next, Phase::Prepared | Phase::Activating)),
        "{reached:?}"
    );
}

#[test]
fn should_never_launch_again_when_a_launch_outcome_is_unknown() {
    // given
    let (jit, _) = reach(&jit_issued());
    let launched = apply(&jit, saga(issue(5)));
    // when
    let unknown = launched.and_then(|(state, events)| {
        assert!(matches!(
            issued_kind(&events),
            Some(DirectiveKind::LaunchRunner { .. })
        ));
        apply(
            &state,
            saga(observe(Fact::RunnerLaunchUnknown {
                directive_id: directive(5),
            })),
        )
    });
    // then
    let (state, _) = unknown.unwrap_or_else(|r| panic!("{r:?}"));
    assert_eq!(state.jit, Jit::Taken(handle()));
    assert_eq!(
        code(apply(&state, saga(issue(6)))),
        Some("nothing_to_issue")
    );
    assert_eq!(
        code(apply(&state, saga(record_jit()))),
        Some("jit_already_issued")
    );
    assert_eq!(
        code(apply(
            &state,
            saga(AttemptCommand::BindJob {
                request_id: RunnerRequestId(1)
            })
        )),
        Some("not_listening")
    );
}

#[test]
fn should_carry_the_recorded_instance_id_when_a_delete_is_issued() {
    // given
    let (stopped, _) = reach(&stopped());
    // when
    let issued = apply(&stopped, saga(issue(8)));
    // then
    let (state, events) = issued.unwrap_or_else(|r| panic!("{r:?}"));
    let Some(DirectiveKind::DeleteVm { proof }) = issued_kind(&events) else {
        panic!("no delete in {events:?}");
    };
    let identity = state.identity.unwrap_or_else(|| panic!("no identity"));
    assert_eq!(proof.instance_id, instance());
    assert_eq!(proof.vm_name, identity.vm_name);
    assert_eq!(proof.generation, identity.generation);
    assert_eq!(proof.attempt_id, identity.attempt_id);
    assert_eq!(state.phase, Phase::Deleting);
}

#[test]
fn should_never_delete_when_quarantined_until_an_operator_resolves() {
    // given
    let (held, _) = reach(&quarantined());
    // when
    let stop = apply(&held, saga(issue(10)));
    let after_stop = stop.clone().and_then(|(state, _)| {
        apply(
            &state,
            saga(observe(Fact::VmStopped {
                directive_id: directive(10),
            })),
        )
    });
    let (stopped, _) = after_stop.unwrap_or_else(|r| panic!("{r:?}"));
    let automated: Vec<Option<DirectiveKind>> = stimuli(&stopped)
        .into_iter()
        .filter(|(_, step)| !matches!(step.actor, Actor::Operator { .. }))
        .filter_map(|(_, step)| apply(&stopped, step).ok())
        .map(|(_, events)| issued_kind(&events))
        .collect();
    let by_automation = apply(&stopped, saga(AttemptCommand::ResolveQuarantine));
    let by_operator = apply(&stopped, at(1, OPERATOR, AttemptCommand::ResolveQuarantine))
        .and_then(|(state, _)| apply(&state, saga(issue(11))));
    // then
    assert!(matches!(
        stop.map(|(_, events)| issued_kind(&events)),
        Ok(Some(DirectiveKind::StopVm { .. }))
    ));
    assert!(matches!(
        stopped.phase,
        Phase::Quarantined {
            reason: QuarantineReason::GuestIdentityRefused,
            ..
        }
    ));
    assert!(
        automated
            .iter()
            .all(|kind| !matches!(kind, Some(DirectiveKind::DeleteVm { .. }))),
        "{automated:?}"
    );
    assert_eq!(code(by_automation), Some("operator_only"));
    assert!(matches!(
        by_operator.map(|(_, events)| issued_kind(&events)),
        Ok(Some(DirectiveKind::DeleteVm { .. }))
    ));
}

#[test]
fn should_refuse_an_older_run_epoch_when_a_command_arrives() {
    // given
    let (prepared, _) = reach(&prepared());
    let adopted = apply(
        &prepared,
        at(
            3,
            Actor::Startup,
            AttemptCommand::Adopt {
                inventory_ref: event(3),
            },
        ),
    );
    // when
    let (state, _) = adopted.unwrap_or_else(|r| panic!("{r:?}"));
    let zombie = apply(
        &state,
        at(
            2,
            SAGA,
            AttemptCommand::BeginActivation {
                grant_ref: event(1),
            },
        ),
    );
    let early = apply(
        &prepared,
        at(
            2,
            SAGA,
            AttemptCommand::BeginActivation {
                grant_ref: event(1),
            },
        ),
    );
    // then
    assert_eq!(code(zombie), Some("stale_run"));
    assert_eq!(code(early), Some("not_adopted"));
    assert_eq!(state.epoch, mayfly_domain::Epoch::new(3));
}

#[test]
fn should_wait_for_deregistration_when_a_registered_runner_is_retired() {
    // given
    let (listening, _) = reach(&listening());
    let retire = saga(AttemptCommand::RequestRetirement {
        reason: RetireReason::Surplus,
    });
    // when
    let requested = apply(&listening, retire);
    let (waiting, _) = requested.unwrap_or_else(|r| panic!("{r:?}"));
    let nothing = apply(&waiting, saga(issue(6)));
    let raced = apply(
        &waiting,
        saga(AttemptCommand::BindJob {
            request_id: RunnerRequestId(1),
        }),
    );
    let confirmed = apply(&waiting, saga(AttemptCommand::ConfirmRetirement));
    // then
    assert_eq!(waiting.phase, Phase::Listening);
    assert_eq!(waiting.retirement_requested, Some(RetireReason::Surplus));
    assert_eq!(code(nothing), Some("nothing_to_issue"));
    let (bound, _) = raced.unwrap_or_else(|r| panic!("{r:?}"));
    assert_eq!(
        (bound.phase, bound.retirement_requested),
        (Phase::Busy, None)
    );
    let (retired, _) = confirmed.unwrap_or_else(|r| panic!("{r:?}"));
    assert_eq!(retired.phase, Phase::Retiring);
}

#[test]
fn should_finish_and_reclaim_when_the_job_ends() {
    // given
    let (finished, _) = reach(&finished());
    // when
    let retired = apply(
        &finished,
        saga(AttemptCommand::RequestRetirement {
            reason: RetireReason::JobDone,
        }),
    );
    let export = retired
        .clone()
        .and_then(|(state, _)| apply(&state, saga(issue(6))));
    // then
    assert_eq!(finished.phase, Phase::Finished);
    assert_eq!(retired.map(|(state, _)| state.phase), Ok(Phase::Retiring));
    assert!(matches!(
        export.map(|(_, events)| issued_kind(&events)),
        Ok(Some(DirectiveKind::ExportDiagnostics))
    ));
}

#[test]
fn should_quarantine_when_the_data_plane_refuses_the_proof_of_a_delete() {
    // given
    let (deleting, _) = reach(&then(stopped(), [issue(8)]));
    // when
    let refused = apply(
        &deleting,
        saga(observe(Fact::DirectiveRefused {
            directive_id: directive(8),
            reason: RefusalReason::ProofMismatch,
        })),
    );
    // then
    let (state, _) = refused.unwrap_or_else(|r| panic!("{r:?}"));
    assert!(matches!(
        state.phase,
        Phase::Quarantined {
            reason: QuarantineReason::ProviderIdentityMismatch,
            ..
        }
    ));
    assert_eq!(
        code(apply(&state, saga(issue(9)))),
        Some("nothing_to_issue")
    );
}

#[test]
fn should_quarantine_as_vanished_when_a_stop_or_start_finds_its_vm_absent() {
    // given: a start of the cloned VM in flight, and a stop of the running one
    let starting = reach(&then(booting(), [issue(2)])).0;
    let stopping = reach(&then(
        retiring(),
        [
            issue(6),
            observe(Fact::DiagnosticsExported {
                directive_id: directive(6),
                bytes: 4_096,
                sha256: [7; 32],
            }),
            issue(7),
        ],
    ))
    .0;

    // when
    let refused: Vec<_> = [(starting, 2), (stopping, 7)]
        .into_iter()
        .map(|(state, n)| {
            apply(
                &state,
                saga(observe(Fact::DirectiveRefused {
                    directive_id: directive(n),
                    reason: RefusalReason::TargetAbsent,
                })),
            )
            .unwrap_or_else(|r| panic!("{r:?}"))
        })
        .collect();

    // then: a desync kept for review, the VM not running, nothing more issued
    for (state, events) in refused {
        assert_eq!(
            state.phase,
            Phase::Quarantined {
                reason: QuarantineReason::VmVanishedExternally,
                vm: VmCondition::Stopped,
            }
        );
        assert!(!state.vm_running);
        assert!(matches!(
            events.last(),
            Some(AttemptEvent::AttemptQuarantined(_))
        ));
        assert_eq!(
            code(apply(&state, saga(issue(9)))),
            Some("nothing_to_issue")
        );
    }
}

#[test]
fn should_keep_a_vanished_vm_quarantined_when_the_inventory_shows_it_absent() {
    // given
    let (vanished, _) = reach(&vanished());

    // when
    let absent = apply(&vanished, saga(observe(Fact::VmAbsent)));
    let resolved = apply(
        &vanished,
        at(1, OPERATOR, AttemptCommand::ResolveQuarantine),
    )
    .and_then(|(state, _)| apply(&state, saga(issue(9))));

    // then
    let (state, events) = absent.unwrap_or_else(|r| panic!("{r:?}"));
    assert!(
        events.is_empty(),
        "absence is why it is quarantined: {events:?}"
    );
    assert_eq!(state.phase, vanished.phase);
    assert!(matches!(
        resolved.map(|(_, events)| issued_kind(&events)),
        Ok(Some(DirectiveKind::DeleteVm { .. }))
    ));
}

#[test]
fn should_quarantine_when_the_data_plane_refuses_the_proof_of_a_start() {
    // given: the start's name listed but unproven, or absent to a data plane older than 1.2
    let (starting, _) = reach(&then(booting(), [issue(2)]));

    // when
    let refused = apply(
        &starting,
        saga(observe(Fact::DirectiveRefused {
            directive_id: directive(2),
            reason: RefusalReason::ProofMismatch,
        })),
    );

    // then
    let (state, _) = refused.unwrap_or_else(|r| panic!("{r:?}"));
    assert!(matches!(
        state.phase,
        Phase::Quarantined {
            reason: QuarantineReason::ProviderIdentityMismatch,
            ..
        }
    ));
}

#[test]
fn should_return_to_the_failed_stage_when_a_backoff_is_retried() {
    // given
    let failing = [
        (then(booting(), [issue(2)]), 2, Stage::Boot, Phase::Booting),
        (
            then(verifying(), [issue(3)]),
            3,
            Stage::Verify,
            Phase::Verifying,
        ),
        (
            then(activating(), [issue(4)]),
            4,
            Stage::Activate,
            Phase::Activating,
        ),
    ];
    // when
    let retried: Vec<(Phase, Phase)> = failing
        .into_iter()
        .map(|(path, n, _, _)| {
            let (state, _) = reach(&then(
                path,
                [observe(Fact::DirectiveRefused {
                    directive_id: directive(n),
                    reason: RefusalReason::EffectFailed,
                })],
            ));
            let backoff = state.phase;
            let (again, _) = apply(&state, saga(issue(20))).unwrap_or_else(|r| panic!("{r:?}"));
            (backoff, again.phase)
        })
        .collect();
    // then
    assert_eq!(
        retried,
        [
            (
                Phase::Backoff {
                    stage: Stage::Boot,
                    n: 1
                },
                Phase::Booting
            ),
            (
                Phase::Backoff {
                    stage: Stage::Verify,
                    n: 1
                },
                Phase::Verifying
            ),
            (
                Phase::Backoff {
                    stage: Stage::Activate,
                    n: 1
                },
                Phase::Activating
            ),
        ]
    );
}

#[test]
fn should_ignore_an_observation_when_it_comes_from_an_older_epoch() {
    // given
    let (adopted, _) = reach(&then(cloning(), []));
    let (adopted, _) = apply(
        &adopted,
        at(
            2,
            Actor::Startup,
            AttemptCommand::Adopt {
                inventory_ref: event(3),
            },
        ),
    )
    .unwrap_or_else(|r| panic!("{r:?}"));
    // when
    let old = apply(
        &adopted,
        at(
            2,
            SAGA,
            observe(Fact::VmCloned {
                directive_id: directive(1),
                instance_id: instance(),
            }),
        ),
    );
    // then
    let (state, events) = old.unwrap_or_else(|r| panic!("{r:?}"));
    assert!(matches!(
        events.as_slice(),
        [AttemptEvent::StaleObservationIgnored(..)]
    ));
    assert_eq!(state.instance_id, None);
}

#[test]
fn should_resolve_an_unknown_start_or_stop_when_the_inventory_shows_the_vm() {
    // given
    let (start_unknown, _) = reach(&then(
        booting(),
        [
            issue(2),
            observe(Fact::OutcomeUnknown {
                directive_id: directive(2),
            }),
        ],
    ));
    let (stop_unknown, _) = reach(&then(
        retiring(),
        [
            issue(6),
            observe(Fact::DiagnosticsExported {
                directive_id: directive(6),
                bytes: 1,
                sha256: [0; 32],
            }),
            issue(7),
            observe(Fact::OutcomeUnknown {
                directive_id: directive(7),
            }),
        ],
    ));
    let present = |running| {
        saga(observe(Fact::VmPresent {
            instance_id: instance(),
            running,
        }))
    };
    // when
    let not_started = apply(&start_unknown, present(false));
    let stopped = apply(&stop_unknown, present(false));
    // then
    let (state, _) = not_started.unwrap_or_else(|r| panic!("{r:?}"));
    assert_eq!(
        state.phase,
        Phase::Backoff {
            stage: Stage::Boot,
            n: 1
        }
    );
    let (state, _) = stopped.unwrap_or_else(|r| panic!("{r:?}"));
    // a stop the listing shows took effect is the stop's success: the delete has its own budget
    // (VER-01F, the model's `unknownStopResolvedByListingTest`)
    assert_eq!(
        (state.pending, state.vm_running, state.tries),
        (None, false, 0)
    );
}

#[test]
fn should_finish_a_presumed_leaked_runner_when_it_exits() {
    // given
    let (unknown, _) = reach(&then(
        jit_issued(),
        [
            issue(5),
            observe(Fact::RunnerLaunchUnknown {
                directive_id: directive(5),
            }),
        ],
    ));
    // when
    let requested = apply(
        &unknown,
        saga(AttemptCommand::RequestRetirement {
            reason: RetireReason::JitPresumedLeaked,
        }),
    );
    let (waiting, _) = requested.unwrap_or_else(|r| panic!("{r:?}"));
    let twice = apply(
        &waiting,
        saga(AttemptCommand::RequestRetirement {
            reason: RetireReason::Surplus,
        }),
    );
    let withdrawn = apply(&waiting, saga(AttemptCommand::WithdrawRetirement));
    let exited = apply(&waiting, saga(observe(Fact::RunnerExited { code: 1 })));
    // then
    assert_eq!(code(twice), Some("already_retiring"));
    assert_eq!(withdrawn.map(|(s, _)| s.retirement_requested), Ok(None));
    assert_eq!(exited.map(|(s, _)| s.phase), Ok(Phase::Finished));
}

#[test]
fn should_record_a_late_launch_and_never_bind_a_job_when_a_restart_orphaned_its_handle() {
    // given
    let (launching, _) = reach(&then(jit_issued(), [issue(5)]));
    let (adopted, adopt_events) = apply(
        &launching,
        at(
            2,
            Actor::Startup,
            AttemptCommand::Adopt {
                inventory_ref: event(3),
            },
        ),
    )
    .unwrap_or_else(|r| panic!("{r:?}"));
    // when
    let launched = apply(
        &adopted,
        at(
            2,
            SAGA,
            observe_in(
                2,
                Fact::RunnerLaunched {
                    directive_id: directive(5),
                },
            ),
        ),
    );
    // then
    assert!(matches!(
        adopt_events.as_slice(),
        [
            AttemptEvent::AttemptAdopted(..),
            AttemptEvent::JitHandleOrphaned(..)
        ]
    ));
    let (state, _) = launched.unwrap_or_else(|r| panic!("{r:?}"));
    assert_eq!(
        (state.phase, state.pending, state.jit),
        (Phase::Listening, None, Jit::Orphaned(handle()))
    );
    let bind = at(
        2,
        SAGA,
        AttemptCommand::BindJob {
            request_id: RunnerRequestId(1),
        },
    );
    assert_eq!(code(apply(&state, bind)), Some("jit_presumed_leaked"));
    let retire = at(
        2,
        SAGA,
        AttemptCommand::RequestRetirement {
            reason: RetireReason::JitPresumedLeaked,
        },
    );
    assert_eq!(
        apply(&state, retire).map(|(s, _)| s.retirement_requested),
        Ok(Some(RetireReason::JitPresumedLeaked))
    );
}

#[test]
fn should_quarantine_when_a_delete_is_refused_again_and_again() {
    // given
    let (stopped, _) = reach(&stopped());
    let refuse = |n| {
        saga(observe(Fact::DirectiveRefused {
            directive_id: directive(n),
            reason: RefusalReason::SafeMode,
        }))
    };
    // when
    let after = apply(&stopped, saga(issue(8)))
        .and_then(|(s, _)| apply(&s, refuse(8)))
        .and_then(|(s, _)| {
            assert_eq!((s.phase, s.pending), (Phase::Deleting, None));
            apply(&s, saga(issue(9)))
        })
        .and_then(|(s, _)| apply(&s, refuse(9)));
    // then
    let (state, _) = after.unwrap_or_else(|r| panic!("{r:?}"));
    assert!(matches!(
        state.phase,
        Phase::Quarantined {
            reason: QuarantineReason::DeleteRefusedRepeatedly,
            ..
        }
    ));
    assert_eq!(
        code(apply(&state, saga(issue(10)))),
        Some("nothing_to_issue")
    );
}

#[test]
fn should_stop_trying_when_quarantined_stops_are_refused() {
    // given
    let (held, _) = reach(&quarantined());
    let refuse = |n, reason| {
        saga(observe(Fact::DirectiveRefused {
            directive_id: directive(n),
            reason,
        }))
    };
    // when
    let after = apply(&held, saga(issue(10)))
        .and_then(|(s, _)| apply(&s, refuse(10, RefusalReason::ProofMismatch)))
        .and_then(|(s, events)| {
            assert_eq!(events.len(), 1, "{events:?}");
            apply(&s, saga(issue(11)))
        })
        .and_then(|(s, _)| apply(&s, refuse(11, RefusalReason::SafeMode)));
    // then
    let (state, _) = after.unwrap_or_else(|r| panic!("{r:?}"));
    assert!(matches!(
        state.phase,
        Phase::Quarantined {
            reason: QuarantineReason::GuestIdentityRefused,
            ..
        }
    ));
    assert_eq!(
        code(apply(&state, saga(issue(12)))),
        Some("nothing_to_issue")
    );
}

#[test]
fn should_resolve_an_unknown_stop_when_quarantined_and_the_inventory_shows_the_vm() {
    // given
    let (unknown, _) = reach(&then(
        quarantined(),
        [
            issue(10),
            observe(Fact::OutcomeUnknown {
                directive_id: directive(10),
            }),
        ],
    ));
    // when
    let resolved = apply(
        &unknown,
        saga(observe(Fact::VmPresent {
            instance_id: instance(),
            running: false,
        })),
    );
    // then
    let (state, _) = resolved.unwrap_or_else(|r| panic!("{r:?}"));
    assert_eq!((state.pending, state.vm_running), (None, false));
    assert!(matches!(state.phase, Phase::Quarantined { .. }));
}

#[test]
fn should_release_a_doubted_quarantine_when_the_vm_is_gone() {
    // given
    let (doubted, _) = reach(&doubted());
    // when
    let stop = apply(&doubted, saga(issue(10)));
    let gone = apply(&doubted, saga(observe(Fact::VmAbsent)));
    // then
    assert_eq!(code(stop), Some("nothing_to_issue"));
    assert_eq!(gone.map(|(s, _)| s.phase), Ok(Phase::Lost));
}

fn timeout_at(at_ms: u64, n: u128) -> mayfly_control_domain::Command<AttemptCommand> {
    mayfly_control_domain::Command::new(
        mayfly_domain::CommandId::from_u128(0xC1),
        mayfly_domain::Millis::new(at_ms),
        mayfly_domain::Epoch::new(1),
        SAGA,
        AttemptCommand::RecordDirectiveTimeout {
            directive_id: directive(n),
        },
    )
}

fn sent(n: u128, deadline_ms: u64) -> AttemptCommand {
    AttemptCommand::RecordDirectiveSent {
        directive_id: directive(n),
        deadline: mayfly_domain::Deadline::at(mayfly_domain::Millis::new(deadline_ms)),
    }
}

#[test]
fn should_make_the_outcome_unknown_never_success_when_a_deadline_passes() {
    // given: the clone was sent with its deadline at 60 s
    let (cloning, _) = reach(&then(cloning(), [sent(1, 60_000)]));

    // when
    let early = apply(&cloning, timeout_at(59_999, 1));
    let other = apply(&cloning, timeout_at(60_000, 9));
    let (unknown, events) =
        apply(&cloning, timeout_at(60_000, 1)).unwrap_or_else(|r| panic!("{r:?}"));
    let again = apply(&unknown, timeout_at(61_000, 1));
    let reissued = apply(&unknown, saga(issue(2)));

    // then
    assert_eq!(early.err(), Some(AttemptRejection::DeadlineNotReached));
    assert_eq!(other.err(), Some(AttemptRejection::DirectiveNotPending));
    assert!(matches!(
        events.as_slice(),
        [AttemptEvent::DirectiveOutcomeUnknown(..)]
    ));
    assert_eq!(unknown.phase, Phase::Cloning);
    assert_eq!(unknown.instance_id, None);
    assert!(matches!(
        unknown.pending.map(|p| p.outcome),
        Some(Outcome::Unknown { .. })
    ));
    assert!(again.is_ok_and(|(state, events)| events.is_empty() && state == unknown));
    assert_eq!(reissued.err(), Some(AttemptRejection::OutcomeUnknown));
}

#[test]
fn should_time_out_only_a_sent_directive_when_its_deadline_passes() {
    // given: the clone issued with its deadline at 60 s, held unsent long past it
    let (held, _) = reach(&cloning());

    // when: timed out while held; then sent at 3,600 s with a fresh deadline
    let while_held = apply(&held, timeout_at(3_600_000, 1));
    let (sent_late, events) =
        apply(&held, saga(sent(1, 3_660_000))).unwrap_or_else(|r| panic!("{r:?}"));
    let repeat = apply(&sent_late, saga(sent(1, 3_700_000)));
    let before_new = apply(&sent_late, timeout_at(3_659_999, 1));
    let after_new = apply(&sent_late, timeout_at(3_660_000, 1));
    let answered = apply(&reach(&booting()).0, saga(sent(1, 60_000)));

    // then: never Unknown while held; the deadline runs from the send; tries unchanged
    assert_eq!(while_held.err(), Some(AttemptRejection::DirectiveNotSent));
    assert!(matches!(
        events.as_slice(),
        [AttemptEvent::DirectiveSent(..)]
    ));
    assert_eq!(sent_late.tries, held.tries);
    assert!(sent_late.pending.is_some_and(|p| p.sent
        && p.deadline == mayfly_domain::Deadline::at(mayfly_domain::Millis::new(3_660_000))));
    assert!(repeat.is_ok_and(|(state, events)| events.is_empty() && state == sent_late));
    assert_eq!(before_new.err(), Some(AttemptRejection::DeadlineNotReached));
    assert!(after_new.is_ok_and(|(_, events)| matches!(
        events.as_slice(),
        [AttemptEvent::DirectiveOutcomeUnknown(..)]
    )));
    assert_eq!(answered.err(), Some(AttemptRejection::DirectiveNotPending));
}

#[test]
fn should_wait_for_adoption_when_a_timeout_comes_from_a_newer_run() {
    // given
    let (cloning, _) = reach(&cloning());
    let newer = mayfly_control_domain::Command::new(
        mayfly_domain::CommandId::from_u128(0xC2),
        mayfly_domain::Millis::new(60_000),
        mayfly_domain::Epoch::new(2),
        SAGA,
        AttemptCommand::RecordDirectiveTimeout {
            directive_id: directive(1),
        },
    );

    // when
    let decided = apply(&cloning, newer);

    // then
    assert_eq!(decided.err(), Some(AttemptRejection::NotAdopted));
}

/// A retirement requested while activating, after a restart orphaned the handle; then the
/// fresh verification of the new epoch is refused, so the attempt backs off with the request
/// still waiting for its deregistration.
fn retirement_waiting_in_backoff() -> AttemptState {
    let mut path = jit_issued();
    path.push(at(
        2,
        Actor::Startup,
        AttemptCommand::Adopt {
            inventory_ref: event(3),
        },
    ));
    path.push(at(
        2,
        SAGA,
        AttemptCommand::RequestRetirement {
            reason: RetireReason::Surplus,
        },
    ));
    path.push(at(2, SAGA, issue(5)));
    path.push(at(
        2,
        SAGA,
        observe_in(
            2,
            Fact::DirectiveRefused {
                directive_id: directive(5),
                reason: RefusalReason::EffectFailed,
            },
        ),
    ));
    reach(&path).0
}

#[test]
fn should_retire_when_a_retirement_is_confirmed_after_the_attempt_backed_off() {
    // given
    let waiting = retirement_waiting_in_backoff();
    // when
    let confirmed = apply(&waiting, at(2, SAGA, AttemptCommand::ConfirmRetirement));
    // then
    assert_eq!(
        (waiting.phase, waiting.retirement_requested),
        (
            Phase::Backoff {
                stage: Stage::Activate,
                n: 1
            },
            Some(RetireReason::Surplus)
        )
    );
    let (retired, events) = confirmed.unwrap_or_else(|r| panic!("{r:?}"));
    assert_eq!(
        (
            retired.phase,
            retired.retirement_requested,
            retired.retire_reason
        ),
        (Phase::Retiring, None, Some(RetireReason::Surplus))
    );
    assert!(matches!(
        events.as_slice(),
        [AttemptEvent::AttemptRetired(..)]
    ));
}

#[test]
fn should_withdraw_when_a_busy_answer_arrives_after_the_attempt_backed_off() {
    // given
    let waiting = retirement_waiting_in_backoff();
    // when
    let withdrawn = apply(&waiting, at(2, SAGA, AttemptCommand::WithdrawRetirement));
    // then
    let (state, _) = withdrawn.unwrap_or_else(|r| panic!("{r:?}"));
    assert!(matches!(state.phase, Phase::Backoff { .. }));
    assert_eq!(state.retirement_requested, None);
}

#[test]
fn should_retire_when_a_retirement_is_confirmed_after_the_runner_exited() {
    // given: a launch of unknown outcome, the retirement requested, then the runner exits
    let (waiting, _) = reach(&then(
        jit_issued(),
        [
            issue(5),
            observe(Fact::RunnerLaunchUnknown {
                directive_id: directive(5),
            }),
            AttemptCommand::RequestRetirement {
                reason: RetireReason::JitPresumedLeaked,
            },
            observe(Fact::RunnerExited { code: 1 }),
        ],
    ));
    // when
    let confirmed = apply(&waiting, saga(AttemptCommand::ConfirmRetirement));
    // then
    assert_eq!(
        (waiting.phase, waiting.retirement_requested),
        (Phase::Finished, Some(RetireReason::JitPresumedLeaked))
    );
    let (retired, _) = confirmed.unwrap_or_else(|r| panic!("{r:?}"));
    assert_eq!(
        (retired.phase, retired.retire_reason),
        (Phase::Retiring, Some(RetireReason::JitPresumedLeaked))
    );
}

fn request_in_epoch_2(reason: RetireReason) -> Step {
    at(2, SAGA, AttemptCommand::RequestRetirement { reason })
}

#[test]
fn should_wait_for_the_deregistration_when_a_retirement_is_requested_in_backoff_holding_a_registration()
 {
    // given
    let (backoff, _) = reach(&backoff_holding_registration());
    // when
    let requested = apply(&backoff, request_in_epoch_2(RetireReason::Surplus));
    // then: no AttemptRetired, so no stop is issued before P11's deregistration answers
    assert!(matches!(
        (backoff.phase, backoff.jit),
        (
            Phase::Backoff {
                stage: Stage::Activate,
                ..
            },
            Jit::Taken(..)
        )
    ));
    let (waiting, events) = requested.unwrap_or_else(|r| panic!("{r:?}"));
    assert!(matches!(
        events.as_slice(),
        [AttemptEvent::RetirementRequested(..)]
    ));
    assert!(matches!(waiting.phase, Phase::Backoff { .. }));
    assert_eq!(waiting.retirement_requested, Some(RetireReason::Surplus));
}

#[test]
fn should_refuse_a_second_request_when_a_retirement_waits_in_backoff() {
    // given
    let (backoff, _) = reach(&backoff_holding_registration());
    let (waiting, _) = apply(&backoff, request_in_epoch_2(RetireReason::Surplus))
        .unwrap_or_else(|r| panic!("{r:?}"));
    // when
    let again = apply(&waiting, request_in_epoch_2(RetireReason::Drain));
    // then
    assert_eq!(code(again), Some("already_retiring"));
}

#[test]
fn should_not_retry_the_stage_when_a_retirement_waits_in_backoff() {
    // given
    let (backoff, _) = reach(&backoff_holding_registration());
    let (waiting, _) = apply(&backoff, request_in_epoch_2(RetireReason::Surplus))
        .unwrap_or_else(|r| panic!("{r:?}"));
    // when
    let retried = apply(&waiting, at(2, SAGA, issue(7)));
    // then: the stage is retried only once the retirement is withdrawn
    assert_eq!(code(retried), Some("nothing_to_issue"));
    let (withdrawn, _) = apply(&waiting, at(2, SAGA, AttemptCommand::WithdrawRetirement))
        .unwrap_or_else(|r| panic!("{r:?}"));
    let (_, events) = apply(&withdrawn, at(2, SAGA, issue(7))).unwrap_or_else(|r| panic!("{r:?}"));
    assert!(matches!(
        issued_kind(&events),
        Some(DirectiveKind::VerifyGuest)
    ));
}

#[test]
fn should_request_the_retirement_when_the_activation_budget_is_spent_holding_a_registration() {
    // given: the second fresh verification (the budget is 2) is refused
    let (retrying, _) = reach(&then_in_epoch_2(backoff_holding_registration(), issue(7)));
    // when
    let spent = apply(
        &retrying,
        at(
            2,
            SAGA,
            observe_in(
                2,
                Fact::DirectiveRefused {
                    directive_id: directive(7),
                    reason: RefusalReason::EffectFailed,
                },
            ),
        ),
    );
    // then: it backs off and waits for the deregistration instead of retiring at once
    let (waiting, events) = spent.unwrap_or_else(|r| panic!("{r:?}"));
    assert!(matches!(
        events.as_slice(),
        [
            AttemptEvent::DirectiveRefused(..),
            AttemptEvent::AttemptBackedOff(..),
            AttemptEvent::RetirementRequested(..)
        ]
    ));
    assert_eq!(
        waiting.retirement_requested,
        Some(RetireReason::StageFailed)
    );
    let (retired, _) = apply(&waiting, at(2, SAGA, AttemptCommand::ConfirmRetirement))
        .unwrap_or_else(|r| panic!("{r:?}"));
    assert_eq!(
        (retired.phase, retired.retire_reason),
        (Phase::Retiring, Some(RetireReason::StageFailed))
    );
}

/// Negative control of the two tests above: without a registration (no JIT issued), a spent
/// activation retires at once and a retirement requested in Backoff does too, as before.
#[test]
fn should_retire_at_once_when_the_activation_budget_is_spent_without_a_registration() {
    // given: a fresh verification refused twice before any JIT was issued
    let refused = |n| {
        observe(Fact::DirectiveRefused {
            directive_id: directive(n),
            reason: RefusalReason::EffectFailed,
        })
    };
    let (backoff, _) = reach(&then(activating(), [issue(4), refused(4)]));
    let (retrying, _) = reach(&then(activating(), [issue(4), refused(4), issue(5)]));
    // when
    let requested = apply(
        &backoff,
        saga(AttemptCommand::RequestRetirement {
            reason: RetireReason::Surplus,
        }),
    );
    let spent = apply(&retrying, saga(refused(5)));
    // then
    assert!(matches!(
        (backoff.phase, backoff.jit),
        (
            Phase::Backoff {
                stage: Stage::Activate,
                ..
            },
            Jit::None
        )
    ));
    let (_, events) = requested.unwrap_or_else(|r| panic!("{r:?}"));
    assert!(matches!(
        events.as_slice(),
        [AttemptEvent::AttemptRetired(..)]
    ));
    let (retired, events) = spent.unwrap_or_else(|r| panic!("{r:?}"));
    assert!(matches!(
        events.as_slice(),
        [
            AttemptEvent::DirectiveRefused(..),
            AttemptEvent::AttemptRetired(..)
        ]
    ));
    assert_eq!(retired.retire_reason, Some(RetireReason::StageFailed));
}

fn then_in_epoch_2(mut path: Vec<Step>, body: AttemptCommand) -> Vec<Step> {
    path.push(at(2, SAGA, body));
    path
}

#[test]
fn should_refuse_a_confirmation_when_the_vm_was_lost_while_a_retirement_waited() {
    // given
    let (lost, _) = reach(&then(
        listening(),
        [
            AttemptCommand::RequestRetirement {
                reason: RetireReason::Surplus,
            },
            observe(Fact::VmAbsent),
        ],
    ));
    // when
    let confirmed = apply(&lost, saga(AttemptCommand::ConfirmRetirement));
    // then
    assert_eq!(lost.phase, Phase::Lost);
    assert_eq!(code(confirmed), Some("no_retirement_requested"));
}
