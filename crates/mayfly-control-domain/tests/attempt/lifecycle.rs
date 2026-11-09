//! The lifecycle rules of plan §6, each the Rust side of an invariant of
//! `formal/lifecycle.qnt`.

use mayfly_control_domain::attempt::{
    AttemptCommand, AttemptEvent, AttemptRejection, DirectiveKind, Fact, Jit, Outcome, Phase,
    QuarantineReason, RefusalReason, RetireReason, RunnerRequestId, Stage,
};
use mayfly_control_domain::{Actor, Rejection};

use super::support::{
    OPERATOR, SAGA, activating, apply, at, booting, busy, cloning, directive, doubted, event,
    facts, finished, handle, instance, issue, jit_issued, listening, observe, observe_in, prepared,
    quarantined, reach, retiring, saga, stimuli, stopped, then, unknown_clone, verified_activation,
    verifying,
};

fn issued_kind(events: &[AttemptEvent]) -> Option<DirectiveKind> {
    events.iter().find_map(|e| match e {
        AttemptEvent::DirectiveIssued(issued) => Some(issued.kind.clone()),
        AttemptEvent::AttemptStarted(..)
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
    assert_eq!(after_commands.len(), 12);
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
    assert_eq!((state.pending, state.vm_running), (None, false));
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
