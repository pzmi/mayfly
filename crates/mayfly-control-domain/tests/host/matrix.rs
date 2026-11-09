//! Every named host state against every command type: each pair is a defined transition or a
//! defined refusal. The outcomes are pinned in `tests/fixtures/matrix/host.txt`, regenerated
//! deliberately from this test's failure text (ver §4.2), so a pair that changes shows in
//! review.

use std::collections::BTreeSet;
use std::fmt::Write as _;

use mayfly_control_domain::host::{Host, HostCommand};
use mayfly_control_domain::{Aggregate, CommandBody, EventSet, Rejection};

use super::support::{matrix_states, stimuli};

const GOLDEN: &str = include_str!("../fixtures/matrix/host.txt");

#[test]
fn should_define_every_pair_when_each_state_meets_each_command() {
    // given
    let states = matrix_states();
    let stimuli = stimuli();
    let types: BTreeSet<&str> = stimuli.iter().map(|(_, c)| c.body.command_type()).collect();
    let mut text = String::from(
        "# host: every named state against every command: accepted events or the refusal code\n",
    );
    // when
    for (name, state) in &states {
        for (stimulus, command) in &stimuli {
            let outcome = match Host::decide(state, command.clone()) {
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
    assert_eq!(
        types,
        <HostCommand as CommandBody>::TYPES
            .iter()
            .copied()
            .collect(),
        "a stimulus of every command type"
    );
    assert!(
        GOLDEN == text,
        "the matrix differs from tests/fixtures/matrix/host.txt; if these outcomes are intended, commit it as:\n{text}"
    );
}
