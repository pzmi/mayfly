//! Every registration phase against every command type: each pair is a defined transition or a
//! defined refusal. The outcomes are pinned in `tests/fixtures/matrix/registration.txt`,
//! regenerated deliberately from this test's failure text (ver §4.2).

use std::collections::BTreeSet;
use std::fmt::Write as _;

use mayfly_control_domain::registration::{Phase, RegistrationCommand, RunnerRegistration};
use mayfly_control_domain::{Aggregate, CommandBody, EventSet, Rejection};

use super::support::{matrix_states, stimuli};

/// Every phase, by an exhaustive match: a new phase does not compile until it is numbered here,
/// and the matrix then fails until a state of it is in `matrix_states`.
const fn phase_ordinal(phase: Phase) -> usize {
    match phase {
        Phase::Unrequested => 0,
        Phase::Requested => 1,
        Phase::IssueUnknown => 2,
        Phase::Issued => 3,
        Phase::Delivered => 4,
        Phase::Connected => 5,
        Phase::Consumed => 6,
        Phase::Completed => 7,
        Phase::Deregistered => 8,
        Phase::Absent => 9,
    }
}

const PHASES: usize = 10;

const GOLDEN: &str = include_str!("../fixtures/matrix/registration.txt");

#[test]
fn should_define_every_pair_when_each_phase_meets_each_command() {
    // given
    let states = matrix_states();
    let stimuli = stimuli();
    let phases: BTreeSet<usize> = states.iter().map(|(_, s)| phase_ordinal(s.phase)).collect();
    let types: BTreeSet<&str> = stimuli.iter().map(|(_, c)| c.body.command_type()).collect();
    let mut text = String::from(
        "# registration: every phase against every command: accepted events or the refusal code\n",
    );
    // when
    for (name, state) in &states {
        for (stimulus, command) in &stimuli {
            let outcome = match RunnerRegistration::decide(state, command.clone()) {
                Ok(events) if events.is_empty() => "accepted, no event".to_owned(),
                Ok(events) => {
                    let kinds: Vec<String> = events.iter().map(|e| e.kind().to_string()).collect();
                    format!("accepted {}", kinds.join(" "))
                }
                Err(rejection) => format!("refused {}", rejection.code()),
            };
            let _ = writeln!(text, "{name} x {stimulus} -> {outcome}");
        }
    }
    // then
    assert_eq!(phases, (0..PHASES).collect(), "a state of every phase");
    assert_eq!(
        types,
        <RegistrationCommand as CommandBody>::TYPES
            .iter()
            .copied()
            .collect(),
        "a stimulus of every command type"
    );
    assert!(
        GOLDEN == text,
        "the matrix differs from tests/fixtures/matrix/registration.txt; if these outcomes are intended, commit it as:\n{text}"
    );
}
