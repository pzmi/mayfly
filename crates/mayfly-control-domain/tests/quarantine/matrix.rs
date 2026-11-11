//! Every quarantine-case phase against every command type, by every kind of actor: each pair
//! is a defined transition or a defined refusal, and no resolution is accepted from an actor
//! other than an operator. The outcomes are pinned in `tests/fixtures/matrix/quarantine.txt`,
//! regenerated deliberately from this test's failure text (ver §4.2).

use std::collections::BTreeSet;
use std::fmt::Write as _;

use mayfly_control_domain::operations::quarantine::{
    CasePhase, QuarantineCase, QuarantineCaseCommand, QuarantineCaseEvent,
};
use mayfly_control_domain::{Actor, Aggregate, CommandBody, EventSet, Rejection};

use super::support::{matrix_states, stimuli};

/// Every phase, by an exhaustive match: a new phase does not compile until it is numbered here,
/// and the matrix then fails until a state of it is in `matrix_states`.
const fn phase_ordinal(phase: CasePhase) -> usize {
    match phase {
        CasePhase::NotOpened => 0,
        CasePhase::Open => 1,
        CasePhase::Inspected => 2,
        CasePhase::Resolved(_) => 3,
    }
}

const PHASES: usize = 4;

const GOLDEN: &str = include_str!("../fixtures/matrix/quarantine.txt");

#[test]
fn should_define_every_pair_when_each_phase_meets_each_command() {
    // given
    let states = matrix_states();
    let stimuli = stimuli();
    let phases: BTreeSet<usize> = states.iter().map(|(_, s)| phase_ordinal(s.phase)).collect();
    let types: BTreeSet<&str> = stimuli.iter().map(|(_, c)| c.body.command_type()).collect();
    let mut text = String::from(
        "# quarantine: every phase against every command by every actor: accepted events or the refusal code\n",
    );
    let mut resolved_by_non_operator = Vec::new();
    // when
    for (name, state) in &states {
        for (stimulus, command) in &stimuli {
            let outcome = match QuarantineCase::decide(state, command.clone()) {
                Ok(events) => {
                    let operator = matches!(command.actor, Actor::Operator { .. });
                    if !operator
                        && events
                            .iter()
                            .any(|e| matches!(e, QuarantineCaseEvent::QuarantineCaseResolved(_)))
                    {
                        resolved_by_non_operator.push(format!("{name} x {stimulus}"));
                    }
                    if events.is_empty() {
                        "accepted, no event".to_owned()
                    } else {
                        let kinds: Vec<String> =
                            events.iter().map(|e| e.kind().to_string()).collect();
                        format!("accepted {}", kinds.join(" "))
                    }
                }
                Err(rejection) => format!("refused {}", rejection.code()),
            };
            let _ = writeln!(text, "{name} x {stimulus} -> {outcome}");
        }
    }
    // then
    assert!(
        resolved_by_non_operator.is_empty(),
        "resolved without an operator: {resolved_by_non_operator:?}"
    );
    assert_eq!(phases, (0..PHASES).collect(), "a state of every phase");
    assert_eq!(
        types,
        <QuarantineCaseCommand as CommandBody>::TYPES
            .iter()
            .copied()
            .collect(),
        "a stimulus of every command type"
    );
    assert!(
        GOLDEN == text,
        "the matrix differs from tests/fixtures/matrix/quarantine.txt; if these outcomes are intended, commit it as:\n{text}"
    );
}
