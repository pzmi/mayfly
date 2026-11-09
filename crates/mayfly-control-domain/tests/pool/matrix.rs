//! Every pool mode against every command type: each pair is a defined transition or a defined
//! refusal. The outcomes are pinned in `tests/fixtures/matrix/pool.txt`, regenerated
//! deliberately from this test's failure text (ver §4.2).

use std::collections::BTreeSet;
use std::fmt::Write as _;

use mayfly_control_domain::pool::{Pool, PoolCommand, PoolMode};
use mayfly_control_domain::{Aggregate, CommandBody, EventSet, Rejection};

use super::support::{matrix_states, stimuli};

/// Every mode, by an exhaustive match: a new mode does not compile until it is numbered here,
/// and the matrix then fails until a state of it is in `matrix_states`.
const fn mode_ordinal(mode: PoolMode) -> usize {
    match mode {
        PoolMode::Unconfigured => 0,
        PoolMode::Active => 1,
        PoolMode::Paused => 2,
        PoolMode::Draining => 3,
        PoolMode::Drained => 4,
        PoolMode::Decommissioned => 5,
    }
}

const MODES: usize = 6;

const GOLDEN: &str = include_str!("../fixtures/matrix/pool.txt");

#[test]
fn should_define_every_pair_when_each_mode_meets_each_command() {
    // given
    let states = matrix_states();
    let stimuli = stimuli();
    let modes: BTreeSet<usize> = states.iter().map(|(_, s)| mode_ordinal(s.mode)).collect();
    let types: BTreeSet<&str> = stimuli.iter().map(|(_, c)| c.body.command_type()).collect();
    let mut text = String::from(
        "# pool: every mode against every command: accepted events or the refusal code\n",
    );
    // when
    for (name, state) in &states {
        for (stimulus, command) in &stimuli {
            let outcome = match Pool::decide(state, command.clone()) {
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
    assert_eq!(modes, (0..MODES).collect(), "a state of every mode");
    assert_eq!(
        types,
        <PoolCommand as CommandBody>::TYPES
            .iter()
            .copied()
            .collect(),
        "a stimulus of every command type"
    );
    assert!(
        GOLDEN == text,
        "the matrix differs from tests/fixtures/matrix/pool.txt; if these outcomes are intended, commit it as:\n{text}"
    );
}
