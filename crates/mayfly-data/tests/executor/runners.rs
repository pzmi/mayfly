//! A runner's exit (DP-09): the launcher's status read in each watched VM on the executor's own
//! cadence, `RunnerExited` sent once per session, nothing for an answer that cannot be read.

use mayfly_data::{Config, RunnerStatus, SessionEnd, VmError};
use mayfly_domain::{Epoch, Millis, ProviderInstanceId, SecretHandleId};
use mayfly_protocol::{DirectiveKind, LinkError, Observation, ObservationKind, SecretHandle};
use mayfly_testkit_data::{Op, StaticWelcome};

use crate::support::{
    EPOCH, Fakes, LISTING, STATUS, answers, config, described, directive, instance, probe, proof,
    run, run_options, session, vm, welcome,
};

const MAC: &str = "0a:00:00:00:01:01";

fn launch(n: u128) -> DirectiveKind {
    DirectiveKind::LaunchRunner {
        vm_name: vm(n),
        launch: SecretHandle {
            handle_id: SecretHandleId::from_u128(n),
        },
        instance_id: Some(ProviderInstanceId::new(MAC).expect("an instance id")),
    }
}

fn start(n: u128) -> DirectiveKind {
    DirectiveKind::StartVm {
        vm_name: vm(n),
        run_options: run_options(),
    }
}

fn is_exit(o: &Observation) -> bool {
    matches!(o.kind, ObservationKind::RunnerExited { .. })
}

fn exits(seen: &[Observation]) -> Vec<ObservationKind> {
    seen.iter()
        .filter(|o| is_exit(o))
        .map(|o| o.kind.clone())
        .collect()
}

/// Fakes with `vm(1)` stopped when the executor starts, so it is not adopted: its runner is
/// read only once this process launched it.
fn stopped_vm() -> Fakes {
    let fakes = Fakes::new(probe(4));
    fakes
        .provider
        .seed(&vm(1).to_string(), described(MAC, false));
    fakes
}

#[test]
fn should_report_an_exit_once_when_a_launched_runner_exits() {
    run(async {
        // given: vm 1 started and its runner launched in this process
        let fakes = stopped_vm();
        let mut exec = fakes.open(2).await;
        let clock = fakes.clock.clone();
        let guest = fakes.guest.clone();

        // when: its runner exits; three status periods pass
        let (_, seen) = session(&mut exec, |feed, mut tap| async move {
            feed.send(directive(1, start(1))).await;
            tap.wait_for(answers(1)).await;
            feed.send(directive(2, launch(1))).await;
            tap.wait_for(answers(2)).await;
            guest.set_status(&vm(1), Ok(RunnerStatus::Exited { code: 3 }));
            clock.advance(STATUS);
            tap.wait_for(is_exit).await;
            clock.advance(STATUS);
            clock.advance(STATUS);
            // a directive answered after the ticks: the executor handled them first
            feed.send(directive(3, DirectiveKind::ReportCapacity)).await;
            tap.wait_for(answers(3)).await;
            tap
        })
        .await;

        // then: one exit, unsolicited, in the session's epoch; the guest read once
        assert_eq!(
            exits(&seen),
            [ObservationKind::RunnerExited {
                vm_name: vm(1),
                code: 3
            }]
        );
        let exit = seen.iter().find(|o| is_exit(o)).expect("the exit");
        assert_eq!((exit.answers, exit.epoch), (None, EPOCH));
        assert_eq!(fakes.guest.status_reads(&vm(1)), 1);
    });
}

#[test]
fn should_record_nothing_when_the_status_cannot_be_read() {
    run(async {
        // given: vm 1's runner launched, its status unreadable
        let fakes = stopped_vm();
        let mut exec = fakes.open(2).await;
        let clock = fakes.clock.clone();
        let guest = fakes.guest.clone();

        // when: one period reads a definite failure, the next an ambiguous one, the third the
        // exit
        let (_, seen) = session(&mut exec, |feed, mut tap| async move {
            feed.send(directive(1, start(1))).await;
            tap.wait_for(answers(1)).await;
            feed.send(directive(2, launch(1))).await;
            tap.wait_for(answers(2)).await;
            guest.set_status(&vm(1), Err(VmError::Definite));
            clock.advance(STATUS);
            feed.send(directive(3, DirectiveKind::ReportCapacity)).await;
            tap.wait_for(answers(3)).await;
            guest.set_status(
                &vm(1),
                Err(VmError::Ambiguous(mayfly_data::Ambiguity::Deadline)),
            );
            clock.advance(STATUS);
            feed.send(directive(4, DirectiveKind::ReportCapacity)).await;
            tap.wait_for(answers(4)).await;
            let unread = tap.drain();
            guest.set_status(&vm(1), Ok(RunnerStatus::Exited { code: 0 }));
            clock.advance(STATUS);
            tap.wait_for(is_exit).await;
            assert!(exits(&unread).is_empty(), "{unread:?}");
            tap
        })
        .await;

        // then: the unreadable answers recorded nothing and the VM was read again each time
        assert_eq!(exits(&seen).len(), 1, "{seen:?}");
        assert_eq!(fakes.guest.status_reads(&vm(1)), 3);
    });
}

#[test]
fn should_read_no_runner_when_no_launch_reached_its_vm() {
    run(async {
        // given: vm 1 started in this process, never launched
        let fakes = stopped_vm();
        let mut exec = fakes.open(2).await;
        let clock = fakes.clock.clone();

        // when: two periods pass
        let (_, seen) = session(&mut exec, |feed, mut tap| async move {
            feed.send(directive(1, start(1))).await;
            tap.wait_for(answers(1)).await;
            clock.advance(STATUS);
            clock.advance(STATUS);
            feed.send(directive(2, DirectiveKind::ReportCapacity)).await;
            tap.wait_for(answers(2)).await;
            tap
        })
        .await;

        // then: its guest is never asked
        assert!(exits(&seen).is_empty());
        assert_eq!(fakes.guest.status_reads(&vm(1)), 0);
    });
}

#[test]
fn should_report_an_exit_during_an_outage_when_the_executor_restarts() {
    run(async {
        // given: a restarted process finds vm 1 and vm 2 running; vm 1's runner exited during
        // the outage, no launch ever reached vm 2
        let fakes = Fakes::new(probe(4));
        fakes
            .provider
            .seed(&vm(1).to_string(), described(MAC, true));
        fakes
            .provider
            .seed(&vm(2).to_string(), described("0a:00:00:00:01:02", true));
        fakes
            .guest
            .set_status(&vm(1), Ok(RunnerStatus::Exited { code: 1 }));
        let mut exec = fakes.open(2).await;
        let clock = fakes.clock.clone();

        // when: the session opens, then two periods pass
        let (_, seen) = session(&mut exec, |feed, mut tap| async move {
            tap.wait_for(is_exit).await;
            clock.advance(STATUS);
            clock.advance(STATUS);
            feed.send(directive(1, DirectiveKind::ReportCapacity)).await;
            tap.wait_for(answers(1)).await;
            tap
        })
        .await;

        // then: vm 1's exit is reported as the session opens, once; vm 2 is read once and
        // dropped
        assert_eq!(
            exits(&seen),
            [ObservationKind::RunnerExited {
                vm_name: vm(1),
                code: 1
            }]
        );
        assert_eq!(fakes.guest.status_reads(&vm(1)), 1);
        assert_eq!(fakes.guest.status_reads(&vm(2)), 1);
    });
}

#[test]
fn should_send_a_reported_exit_again_once_when_a_new_session_opens() {
    run(async {
        // given: vm 1's exit reported in the session of EPOCH, which then ended
        let fakes = Fakes::new(probe(4));
        fakes
            .provider
            .seed(&vm(1).to_string(), described(MAC, true));
        fakes
            .guest
            .set_status(&vm(1), Ok(RunnerStatus::Exited { code: 4 }));
        let mut exec = fakes.open(2).await;
        let (_, first) = session(&mut exec, |_feed, mut tap| async move {
            tap.wait_for(is_exit).await;
            tap
        })
        .await;
        let next = Epoch::new(EPOCH.get() + 1);
        let mut link = StaticWelcome::new(Ok(welcome(next)));
        assert_eq!(exec.enter(&mut link, &fakes.floor).await, Ok(next));

        // when: the next session opens and serves a directive
        let (_, second) = session(&mut exec, |feed, mut tap| async move {
            feed.send(directive(1, DirectiveKind::ReportCapacity)).await;
            tap.wait_for(answers(1)).await;
            tap
        })
        .await;

        // then: the exit is sent again in the new session, once, from memory: the guest was
        // read only in the first
        assert_eq!(exits(&first).len(), 1, "{first:?}");
        assert_eq!(exits(&second).len(), 1, "{second:?}");
        let again = second.iter().find(|o| is_exit(o)).expect("the exit");
        assert_eq!(again.epoch, next);
        assert!(matches!(
            second.first().map(|o| &o.kind),
            Some(ObservationKind::CapacityReported(_))
        ));
        assert_eq!(fakes.guest.status_reads(&vm(1)), 1);
    });
}

#[test]
fn should_start_a_directive_when_a_status_read_hangs() {
    run(async {
        // given: one directive at a time; vm 1's runner launched, and its guest never answering
        // a status read
        let fakes = stopped_vm();
        let mut exec = fakes.open(1).await;
        let clock = fakes.clock.clone();
        let guest = fakes.guest.clone();

        // when: a read starts and hangs; a directive on the same VM arrives; the guest answers
        // again and its runner has exited; the hung read's time runs out
        let (_, seen) = session(&mut exec, |feed, mut tap| async move {
            feed.send(directive(1, start(1))).await;
            tap.wait_for(answers(1)).await;
            feed.send(directive(2, launch(1))).await;
            tap.wait_for(answers(2)).await;
            guest.hang_status(&vm(1), true);
            clock.advance(STATUS);
            feed.send(directive(
                3,
                DirectiveKind::ExportDiagnostics { vm_name: vm(1) },
            ))
            .await;
            tap.wait_for(answers(3)).await;
            guest.hang_status(&vm(1), false);
            guest.set_status(&vm(1), Ok(RunnerStatus::Exited { code: 0 }));
            clock.advance(LISTING);
            feed.send(directive(4, DirectiveKind::ReportCapacity)).await;
            tap.wait_for(answers(4)).await;
            // one more period: the next read starts by then, whichever of the hung read's
            // deadline and the period's tick the executor handled first (the exit may already
            // be in, read before the barrier's answer)
            clock.advance(STATUS);
            if exits(&tap.drain()).is_empty() {
                tap.wait_for(is_exit).await;
            }
            tap
        })
        .await;

        // then: the export ran beside the hung read (no room in the bound taken), the read was
        // dropped at its deadline, and the next read found the exit
        assert!(seen.iter().any(
            |o| answers(3)(o) && matches!(o.kind, ObservationKind::DiagnosticsExported { .. })
        ));
        assert_eq!(exits(&seen).len(), 1, "{seen:?}");
        assert_eq!(fakes.guest.status_reads(&vm(1)), 2);
    });
}

/// Starts vm 1 and launches its runner, answered both.
async fn launched(feed: &mayfly_testkit_data::DirectiveFeed, tap: &mut mayfly_testkit_data::Tap) {
    feed.send(directive(1, start(1))).await;
    tap.wait_for(answers(1)).await;
    feed.send(directive(2, launch(1))).await;
    tap.wait_for(answers(2)).await;
}

#[test]
fn should_end_the_session_when_a_status_read_hangs() {
    run(async {
        // given: vm 1's runner launched, and its guest never answering a status read
        let fakes = stopped_vm();
        let mut exec = fakes.open(2).await;
        let clock = fakes.clock.clone();
        let guest = fakes.guest.clone();

        // when: a read starts and hangs, then the control plane's end closes (the clock never
        // reaches the read's deadline)
        let (end, _) = session(&mut exec, |feed, mut tap| async move {
            launched(&feed, &mut tap).await;
            guest.hang_status(&vm(1), true);
            clock.advance(STATUS);
            feed.send(directive(3, DirectiveKind::ReportCapacity)).await;
            tap.wait_for(answers(3)).await;
            tap
        })
        .await;

        // then: the session ended at once, the hung read dropped rather than awaited
        assert_eq!(end, SessionEnd::SourceClosed(LinkError::Closed));
        assert_eq!(fakes.guest.status_reads(&vm(1)), 1);
    });
}

#[test]
fn should_read_a_vm_once_at_a_time_when_its_read_hangs() {
    run(async {
        // given: vm 1's runner launched, and its guest never answering a status read
        let fakes = stopped_vm();
        let mut exec = fakes.open(2).await;
        let clock = fakes.clock.clone();
        let guest = fakes.guest.clone();

        // when: a read starts and hangs, and the next period comes before its deadline
        let (_, _) = session(&mut exec, |feed, mut tap| async move {
            launched(&feed, &mut tap).await;
            guest.hang_status(&vm(1), true);
            clock.advance(STATUS);
            feed.send(directive(3, DirectiveKind::ReportCapacity)).await;
            tap.wait_for(answers(3)).await;
            clock.advance(STATUS);
            feed.send(directive(4, DirectiveKind::ReportCapacity)).await;
            tap.wait_for(answers(4)).await;
            tap
        })
        .await;

        // then: no second read of the VM started beside the first
        assert!(STATUS.get() * 2 < STATUS.get() + LISTING.get());
        assert_eq!(fakes.guest.status_reads(&vm(1)), 1);
    });
}

#[test]
fn should_skip_the_read_when_a_directive_works_on_the_vm() {
    run(async {
        // given: vm 1's runner launched, and a stop of vm 1 held in the provider
        let fakes = stopped_vm();
        let mut exec = fakes.open(2).await;
        let clock = fakes.clock.clone();
        let guest = fakes.guest.clone();
        let held = fakes.provider.hold_next(Op::Stop);

        // when: the period comes while the stop runs
        let (_, _) = session(&mut exec, |feed, mut tap| async move {
            launched(&feed, &mut tap).await;
            let stop = DirectiveKind::StopVm {
                grace: Millis::new(100),
                proof: proof(1, &instance(MAC)),
            };
            feed.send(directive(3, stop)).await;
            held.started().await;
            clock.advance(STATUS);
            feed.send(directive(4, DirectiveKind::ReportCapacity)).await;
            tap.wait_for(answers(4)).await;
            let during = guest.status_reads(&vm(1));
            held.release();
            tap.wait_for(answers(3)).await;
            assert_eq!(during, 0, "a read started on a VM a stop works on");
            tap
        })
        .await;

        // then: no read of vm 1 ever started (stopped since, it is not read either)
        assert_eq!(fakes.guest.status_reads(&vm(1)), 0);
    });
}

#[test]
fn should_read_no_faster_than_the_least_period_when_status_every_is_zero() {
    run(async {
        // given: a configured period of zero, and vm 1's runner launched and exited
        let fakes = stopped_vm();
        let mut exec = fakes
            .open_with(Config {
                status_every: Millis::new(0),
                ..config(2)
            })
            .await;
        let clock = fakes.clock.clone();
        let guest = fakes.guest.clone();

        // when: half the least period passes, then the rest
        let (_, seen) = session(&mut exec, |feed, mut tap| async move {
            launched(&feed, &mut tap).await;
            guest.set_status(&vm(1), Ok(RunnerStatus::Exited { code: 0 }));
            let half = Millis::new(mayfly_data::MIN_STATUS_EVERY.get() / 2);
            clock.advance(half);
            feed.send(directive(3, DirectiveKind::ReportCapacity)).await;
            tap.wait_for(answers(3)).await;
            let early = guest.status_reads(&vm(1));
            clock.advance(half);
            tap.wait_for(is_exit).await;
            assert_eq!(early, 0, "read before the least period passed");
            tap
        })
        .await;

        // then: read once the least period passed, the loop never spun on a zero period
        assert_eq!(exits(&seen).len(), 1);
        assert_eq!(fakes.guest.status_reads(&vm(1)), 1);
    });
}
