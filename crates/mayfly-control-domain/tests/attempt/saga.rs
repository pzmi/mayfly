//! The attempt saga's decision table (`attempt::next_step`, policy P6 with P7's deadline): read
//! from the attempt's state, so a late or repeated event never issues what the aggregate would
//! refuse, and an `Unknown` outcome makes the saga wait for an observation (I10).

use mayfly_control_domain::Aggregate;
use mayfly_control_domain::attempt::{
    Attempt, AttemptCommand, Outcome, Phase, SagaStep, next_step,
};
use mayfly_domain::{Deadline, Millis};

use super::support::{SAGA, at, directive, matrix_states};

/// What every matrix state must yield, by name: the decision table of `next_step`.
fn expected(name: &str) -> &'static str {
    match name {
        // nothing pending, and the phase has a directive to issue (a running quarantined VM
        // with a recorded instance is stopped; a refused delete is retried within budget)
        "reserved" | "booting" | "verifying" | "activating" | "retiring" | "deleting"
        | "backoff" | "backoff/registered" | "quarantined" => "issue",
        // sent: its deadline runs; every other in-flight state is issued and not sent yet,
        // held by the relay with no deadline running, so it is idle below
        "cloning/sent" => "await",
        // a vanished VM's attempt closes like any lost one (DEC-07)
        "deleted" | "lost" | "lost/vanished_on_start" | "lost/doubt_released" => "close",
        // a Backoff whose retirement waits for its deregistration does not retry (KER-04G);
        // not started or closed; issued and not sent; an Unknown outcome waits for an observation (I10); another
        // policy moves prepared, listening, busy and finished attempts; a doubted VM has no
        // proof, so nothing is stopped
        _ => "idle",
    }
}

fn shown(step: &SagaStep) -> &'static str {
    match step {
        SagaStep::Issue(..) => "issue",
        SagaStep::AwaitAnswer { .. } => "await",
        SagaStep::Close => "close",
        SagaStep::Idle => "idle",
    }
}

#[test]
fn should_follow_the_decision_table_when_each_matrix_state_is_read() {
    // given
    let states = matrix_states();

    // when
    let steps: Vec<(&str, &str)> = states
        .iter()
        .map(|(name, state)| (*name, shown(&next_step(state))))
        .collect();

    // then
    let wrong: Vec<_> = steps
        .iter()
        .filter(|(name, step)| expected(name) != *step)
        .collect();
    assert!(wrong.is_empty(), "{wrong:?} of {steps:?}");
}

#[test]
fn should_issue_exactly_when_issue_next_would_be_accepted() {
    // given
    let states = matrix_states();

    // when
    let disagreements: Vec<String> = states
        .iter()
        .filter_map(|(name, state)| {
            let epoch = state.epoch.get().max(1);
            let issue = at(
                epoch,
                SAGA,
                AttemptCommand::IssueNext {
                    directive_id: directive(77),
                    deadline: Deadline::at(Millis::new(60_000)),
                },
            );
            let accepted = Attempt::decide(state, issue).is_ok_and(|events| !events.is_empty());
            let issues = matches!(next_step(state), SagaStep::Issue(..));
            (accepted != issues).then(|| format!("{name}: accepted {accepted}, saga {issues}"))
        })
        .collect();

    // then
    assert!(disagreements.is_empty(), "{disagreements:?}");
}

#[test]
fn should_await_only_a_sent_directive_and_never_issue_when_one_is_pending() {
    // given
    let states = matrix_states();

    // when
    let awaited: Vec<(&str, bool)> = states
        .iter()
        .filter_map(|(name, state)| {
            let pending = state.pending?;
            let step = next_step(state);
            Some((
                *name,
                match pending.outcome {
                    Outcome::InFlight if pending.sent => {
                        step == SagaStep::AwaitAnswer {
                            directive_id: pending.directive_id,
                            deadline: pending.deadline,
                        }
                    }
                    Outcome::InFlight | Outcome::Unknown { .. } => step == SagaStep::Idle,
                },
            ))
        })
        .collect();

    // then
    assert!(awaited.len() >= 10, "{awaited:?}");
    assert!(awaited.iter().all(|(_, ok)| *ok), "{awaited:?}");
    assert!(
        states
            .iter()
            .all(|(_, s)| s.phase != Phase::Closed || next_step(s) == SagaStep::Idle)
    );
}
