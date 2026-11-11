//! Every image status against every command type: each pair is a defined transition or a
//! defined refusal. The outcomes are pinned in `tests/fixtures/matrix/image.txt`, regenerated
//! deliberately from this test's failure text (ver §4.2).

use std::collections::BTreeSet;
use std::fmt::Write as _;

use mayfly_control_domain::image::{Image, ImageCommand, ImageStatus};
use mayfly_control_domain::{Aggregate, CommandBody, EventSet, Rejection};

use super::support::{matrix_states, stimuli};

/// Every status, by an exhaustive match: a new status does not compile until it is numbered
/// here, and the matrix then fails until a state of it is in `matrix_states`.
const fn status_ordinal(status: ImageStatus) -> usize {
    match status {
        ImageStatus::Unregistered => 0,
        ImageStatus::Registered => 1,
        ImageStatus::Verified => 2,
        ImageStatus::Rejected => 3,
        ImageStatus::Withdrawn => 4,
    }
}

const STATUSES: usize = 5;

const GOLDEN: &str = include_str!("../fixtures/matrix/image.txt");

#[test]
fn should_define_every_pair_when_each_status_meets_each_command() {
    // given
    let states = matrix_states();
    let stimuli = stimuli();
    let statuses: BTreeSet<usize> = states
        .iter()
        .map(|(_, s)| status_ordinal(s.status))
        .collect();
    let types: BTreeSet<&str> = stimuli.iter().map(|(_, c)| c.body.command_type()).collect();
    let mut text = String::from(
        "# image: every status against every command: accepted events or the refusal code\n",
    );
    // when
    for (name, state) in &states {
        for (stimulus, command) in &stimuli {
            let outcome = match Image::decide(state, command.clone()) {
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
    assert_eq!(statuses, (0..STATUSES).collect(), "a state of every status");
    assert_eq!(
        types,
        <ImageCommand as CommandBody>::TYPES
            .iter()
            .copied()
            .collect(),
        "a stimulus of every command type"
    );
    assert!(
        GOLDEN == text,
        "the matrix differs from tests/fixtures/matrix/image.txt; if these outcomes are intended, commit it as:\n{text}"
    );
}
