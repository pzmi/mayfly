//! The State × Stimulus matrix of plan §6 rule 8: every phase meets every stimulus, and each
//! pair is a defined transition or a defined refusal. The outcomes are pinned in
//! `tests/fixtures/matrix/attempt.txt`, regenerated deliberately from this test's failure text
//! (ver §4.2), so a pair that changes shows in review.

use std::collections::BTreeSet;
use std::fmt::Write as _;

use mayfly_control_domain::attempt::{Attempt, AttemptCommand, Fact, Phase};
use mayfly_control_domain::{Aggregate, EventSet, Rejection};

use super::support::{matrix_states, stimuli};

/// Every phase, by an exhaustive match: a new phase does not compile until it is numbered here,
/// and the matrix then fails until a state of it is in `matrix_states`.
const fn phase_ordinal(phase: Phase) -> usize {
    match phase {
        Phase::NotStarted => 0,
        Phase::Reserved => 1,
        Phase::Cloning => 2,
        Phase::Booting => 3,
        Phase::Verifying => 4,
        Phase::Prepared => 5,
        Phase::Activating => 6,
        Phase::Listening => 7,
        Phase::Busy => 8,
        Phase::Finished => 9,
        Phase::Retiring => 10,
        Phase::Deleting => 11,
        Phase::Deleted => 12,
        Phase::Lost => 13,
        Phase::Backoff { .. } => 14,
        Phase::Quarantined { .. } => 15,
        Phase::Closed => 16,
    }
}

const PHASES: usize = 17;

/// Every stimulus (each command, and each observation fact), by an exhaustive match.
const fn stimulus_ordinal(body: &AttemptCommand) -> usize {
    match body {
        AttemptCommand::StartAttempt(..) => 0,
        AttemptCommand::IssueNext { .. } => 1,
        AttemptCommand::BeginActivation { .. } => 2,
        AttemptCommand::RecordJitHandle { .. } => 3,
        AttemptCommand::BindJob { .. } => 4,
        AttemptCommand::RequestRetirement { .. } => 5,
        AttemptCommand::ConfirmRetirement => 6,
        AttemptCommand::WithdrawRetirement => 7,
        AttemptCommand::Quarantine { .. } => 8,
        AttemptCommand::Adopt { .. } => 9,
        AttemptCommand::ResolveQuarantine => 10,
        AttemptCommand::Close => 11,
        AttemptCommand::RecordObservation(observation) => match observation.fact {
            Fact::VmCloned { .. } => 12,
            Fact::VmBooted { .. } => 13,
            Fact::GuestVerified { .. } => 14,
            Fact::GuestRejected { .. } => 15,
            Fact::DirectiveRefused { .. } => 16,
            Fact::OutcomeUnknown { .. } => 17,
            Fact::RunnerLaunched { .. } => 18,
            Fact::RunnerLaunchUnknown { .. } => 19,
            Fact::DiagnosticsExported { .. } => 20,
            Fact::DiagnosticsLost { .. } => 21,
            Fact::VmStopped { .. } => 22,
            Fact::VmDeleted { .. } => 23,
            Fact::RunnerExited { .. } => 24,
            Fact::VmObservedStopped => 25,
            Fact::VmPresent { .. } => 26,
            Fact::VmAbsent => 27,
        },
    }
}

const STIMULI: usize = 28;

/// The committed matrix. Read at compile time: the pure crate's tests do no file I/O
/// (its `clippy.toml`), and a deliberate regeneration rewrites this file.
const GOLDEN: &str = include_str!("../fixtures/matrix/attempt.txt");

#[test]
fn should_define_every_pair_when_each_phase_meets_each_stimulus() {
    // given
    let states = matrix_states();
    let phases: BTreeSet<usize> = states.iter().map(|(_, s)| phase_ordinal(s.phase)).collect();
    let mut text = String::from(
        "# attempt: every phase against every stimulus (plan §6 rule 8): accepted events or the refusal code\n",
    );
    let mut pairs = 0;
    let mut expected_pairs = 0;
    // when
    for (phase, state) in &states {
        let stimuli = stimuli(state);
        let kinds: BTreeSet<usize> = stimuli
            .iter()
            .map(|(_, c)| stimulus_ordinal(&c.body))
            .collect();
        assert_eq!(kinds, (0..STIMULI).collect(), "stimuli of {phase}");
        expected_pairs += stimuli.len();
        for (stimulus, command) in stimuli {
            let outcome = match Attempt::decide(state, command) {
                Ok(events) if events.is_empty() => "accepted, no event".to_owned(),
                Ok(events) => {
                    let kinds: Vec<String> = events.iter().map(|e| e.kind().to_string()).collect();
                    format!("accepted {}", kinds.join(" "))
                }
                Err(rejection) => format!("refused {}", rejection.code()),
            };
            let _ = writeln!(text, "{phase} x {stimulus} -> {outcome}");
            pairs += 1;
        }
    }
    // then
    assert_eq!(phases, (0..PHASES).collect(), "a state of every phase");
    assert_eq!(pairs, expected_pairs);
    assert!(
        GOLDEN == text,
        "the matrix differs from tests/fixtures/matrix/attempt.txt; if these outcomes are intended, commit it as:\n{text}"
    );
}
