//! The placement's `DETERMINISM_CASES` (ver §3.1): accepted cases along a placement's life (pull,
//! materialise, refresh, rollback, eviction, failure, unknown outcomes) and one refused case per
//! [`PlacementRejection`] code. Histories are produced by `decide` itself from command paths
//! (each path is checked to be accepted step by step by
//! `should_accept_every_step_when_a_case_path_is_replayed`).

use mayfly_domain::{CommandId, Deadline, DirectiveId, Epoch, HostId, ImageDigest, Millis, Sample};

use super::events::PlacementEvent;
use super::state::{EvictionScope, PlacementCommand, PlacementRejection, PlacementState};
use super::{ImagePlacement, decide, evolve};
use crate::aggregate::{DeterminismCase, Expect};
use crate::attempt::RefusalReason;
use crate::command::{Actor, Command};
use crate::image::RunnerVersion;

type Step = Command<PlacementCommand>;

const POLICY: Actor = Actor::Automation {
    policy: "placement_manager",
};

/// Before every deadline below.
const EARLY: u64 = 1_000;
/// After every deadline below.
const LATE: u64 = 70_000;

fn at(run_epoch: u64, now: u64, body: PlacementCommand) -> Step {
    Command::new(
        CommandId::from_u128(0xC1),
        Millis::new(now),
        Epoch::new(run_epoch),
        POLICY,
        body,
    )
}

fn auto(body: PlacementCommand) -> Step {
    at(1, EARLY, body)
}

const fn runner(minor: u32) -> RunnerVersion {
    RunnerVersion::new(2, minor, 0)
}

const fn id(n: u128) -> DirectiveId {
    DirectiveId::from_u128(0xD000 + n)
}

fn request() -> PlacementCommand {
    PlacementCommand::RequestPlacement {
        host_id: HostId::sample(),
        digest: ImageDigest::sample(),
        runner_version: runner(330),
    }
}

const fn issue(n: u128) -> PlacementCommand {
    PlacementCommand::IssueDirective {
        directive_id: id(n),
        deadline: Deadline::at(Millis::new(60_000)),
    }
}

const fn sent(n: u128) -> PlacementCommand {
    PlacementCommand::RecordSent {
        directive_id: id(n),
        deadline: Deadline::at(Millis::new(60_000)),
    }
}

fn other_digest() -> ImageDigest {
    ImageDigest::new(&format!("sha256:{}", "1".repeat(64)))
        .ok()
        .unwrap_or_else(ImageDigest::sample)
}

fn then(mut path: Vec<Step>, more: impl IntoIterator<Item = PlacementCommand>) -> Vec<Step> {
    path.extend(more.into_iter().map(auto));
    path
}

fn requested() -> Vec<Step> {
    vec![auto(request())]
}

fn pulling() -> Vec<Step> {
    then(requested(), [issue(1)])
}

fn pull_sent() -> Vec<Step> {
    then(pulling(), [sent(1)])
}

fn pull_unknown() -> Vec<Step> {
    let mut path = pull_sent();
    path.push(at(
        1,
        LATE,
        PlacementCommand::RecordOutcomeUnknown {
            directive_id: id(1),
        },
    ));
    path
}

fn failed() -> Vec<Step> {
    then(
        pulling(),
        [PlacementCommand::RecordRefused {
            directive_id: id(1),
            reason: RefusalReason::EffectFailed,
        }],
    )
}

fn pulled() -> Vec<Step> {
    then(
        pulling(),
        [PlacementCommand::RecordPulled {
            directive_id: id(1),
        }],
    )
}

fn materialising() -> Vec<Step> {
    then(pulled(), [issue(2)])
}

fn materialised() -> Vec<Step> {
    then(
        materialising(),
        [PlacementCommand::RecordMaterialised {
            directive_id: id(2),
        }],
    )
}

fn refreshing() -> Vec<Step> {
    then(
        materialised(),
        [
            PlacementCommand::RequestRefresh {
                runner_version: runner(331),
            },
            issue(3),
        ],
    )
}

fn refreshed() -> Vec<Step> {
    then(
        refreshing(),
        [PlacementCommand::RecordMaterialised {
            directive_id: id(3),
        }],
    )
}

fn eviction_requested() -> Vec<Step> {
    then(
        materialised(),
        [PlacementCommand::RequestEviction {
            scope: EvictionScope::Placement,
            references: 0,
        }],
    )
}

fn evicting() -> Vec<Step> {
    then(eviction_requested(), [issue(4)])
}

fn evicted() -> Vec<Step> {
    then(
        evicting(),
        [PlacementCommand::RecordEvicted {
            directive_id: id(4),
        }],
    )
}

fn at_epoch_two() -> Vec<Step> {
    let mut path = requested();
    path.push(at(2, EARLY, issue(1)));
    path
}

/// Folds `path` through `decide` and `evolve`; the first refused step, if any.
fn replay_path(path: Vec<Step>) -> (Vec<PlacementEvent>, Option<(usize, PlacementRejection)>) {
    let mut state = PlacementState::initial();
    let mut history = Vec::new();
    for (index, step) in path.into_iter().enumerate() {
        match decide(&state, step) {
            Ok(events) => {
                for event in &events {
                    state = evolve(state, event);
                }
                history.extend(events);
            }
            Err(rejection) => return (history, Some((index, rejection))),
        }
    }
    (history, None)
}

fn case(
    name: &'static str,
    path: Vec<Step>,
    command: Step,
    expect: Expect,
) -> DeterminismCase<ImagePlacement> {
    DeterminismCase {
        name,
        history: replay_path(path).0,
        command,
        expect,
    }
}

fn accepted(
    name: &'static str,
    path: Vec<Step>,
    body: PlacementCommand,
) -> DeterminismCase<ImagePlacement> {
    case(name, path, auto(body), Expect::Accepted)
}

fn refused(
    name: &'static str,
    path: Vec<Step>,
    body: PlacementCommand,
) -> DeterminismCase<ImagePlacement> {
    case(name, path, auto(body), Expect::Refused)
}

/// Every path a case starts from, for the replay check.
#[cfg(test)]
fn paths() -> Vec<Vec<Step>> {
    vec![
        requested(),
        pulling(),
        pull_sent(),
        pull_unknown(),
        failed(),
        pulled(),
        materialising(),
        materialised(),
        refreshing(),
        refreshed(),
        eviction_requested(),
        evicting(),
        evicted(),
        at_epoch_two(),
    ]
}

pub(super) fn cases() -> Vec<DeterminismCase<ImagePlacement>> {
    let mut cases = pull_cases();
    cases.extend(base_cases());
    cases.extend(refused_cases());
    cases
}

/// Accepted: requests, the pull and its answers.
fn pull_cases() -> Vec<DeterminismCase<ImagePlacement>> {
    let pulled_answer = PlacementCommand::RecordPulled {
        directive_id: id(1),
    };
    vec![
        accepted("request_placement", Vec::new(), request()),
        accepted("request_again_records_nothing", pulling(), request()),
        accepted("request_after_failure", failed(), request()),
        accepted("request_after_eviction", evicted(), request()),
        accepted("issue_pull", requested(), issue(1)),
        accepted("record_pulled", pulling(), pulled_answer.clone()),
        accepted(
            "record_drift",
            pulling(),
            PlacementCommand::RecordDrift {
                directive_id: id(1),
                observed: other_digest(),
            },
        ),
        accepted(
            "pull_refused_fails",
            pulling(),
            PlacementCommand::RecordRefused {
                directive_id: id(1),
                reason: RefusalReason::SafeMode,
            },
        ),
        accepted("record_sent", pulling(), sent(1)),
        accepted("sent_again_records_nothing", pull_sent(), sent(1)),
        case(
            "pull_outcome_unknown",
            pull_sent(),
            at(
                1,
                LATE,
                PlacementCommand::RecordOutcomeUnknown {
                    directive_id: id(1),
                },
            ),
            Expect::Accepted,
        ),
        case(
            "unknown_again_records_nothing",
            pull_unknown(),
            at(
                1,
                LATE,
                PlacementCommand::RecordOutcomeUnknown {
                    directive_id: id(1),
                },
            ),
            Expect::Accepted,
        ),
        accepted(
            "pull_observed_not_applied",
            pull_unknown(),
            PlacementCommand::RecordNotApplied {
                directive_id: id(1),
            },
        ),
        accepted("unknown_pull_answered", pull_unknown(), pulled_answer),
        case(
            "newer_epoch_is_recorded",
            requested(),
            at(2, EARLY, issue(1)),
            Expect::Accepted,
        ),
    ]
}

/// Accepted: materialisation, refresh, rollback and eviction.
fn base_cases() -> Vec<DeterminismCase<ImagePlacement>> {
    vec![
        accepted("issue_materialise", pulled(), issue(2)),
        accepted(
            "record_materialised",
            materialising(),
            PlacementCommand::RecordMaterialised {
                directive_id: id(2),
            },
        ),
        accepted(
            "first_materialise_refused_fails",
            materialising(),
            PlacementCommand::RecordRefused {
                directive_id: id(2),
                reason: RefusalReason::InsufficientResources,
            },
        ),
        accepted(
            "request_refresh",
            materialised(),
            PlacementCommand::RequestRefresh {
                runner_version: runner(331),
            },
        ),
        accepted(
            "refresh_of_current_records_nothing",
            materialised(),
            PlacementCommand::RequestRefresh {
                runner_version: runner(330),
            },
        ),
        accepted(
            "refresh_materialised_keeps_previous",
            refreshing(),
            PlacementCommand::RecordMaterialised {
                directive_id: id(3),
            },
        ),
        accepted(
            "refresh_refused_keeps_base",
            refreshing(),
            PlacementCommand::RecordRefused {
                directive_id: id(3),
                reason: RefusalReason::EffectFailed,
            },
        ),
        accepted(
            "roll_back_base",
            refreshed(),
            PlacementCommand::RollBackBase,
        ),
        accepted(
            "request_eviction",
            materialised(),
            PlacementCommand::RequestEviction {
                scope: EvictionScope::Placement,
                references: 0,
            },
        ),
        accepted(
            "evict_previous_base",
            refreshed(),
            PlacementCommand::RequestEviction {
                scope: EvictionScope::Previous,
                references: 0,
            },
        ),
        accepted(
            "eviction_again_records_nothing",
            evicting(),
            PlacementCommand::RequestEviction {
                scope: EvictionScope::Placement,
                references: 0,
            },
        ),
        accepted("issue_evict", eviction_requested(), issue(4)),
        accepted(
            "record_evicted",
            evicting(),
            PlacementCommand::RecordEvicted {
                directive_id: id(4),
            },
        ),
        accepted(
            "eviction_refused_keeps_base",
            evicting(),
            PlacementCommand::RecordRefused {
                directive_id: id(4),
                reason: RefusalReason::ProofMismatch,
            },
        ),
    ]
}

/// Refused, one per code.
fn refused_cases() -> Vec<DeterminismCase<ImagePlacement>> {
    vec![
        refused("not_requested", Vec::new(), issue(1)),
        refused(
            "stale_run",
            at_epoch_two(),
            PlacementCommand::RecordPulled {
                directive_id: id(1),
            },
        ),
        refused(
            "placement_differs",
            requested(),
            PlacementCommand::RequestPlacement {
                host_id: HostId::sample(),
                digest: other_digest(),
                runner_version: runner(330),
            },
        ),
        refused("directive_pending", pull_unknown(), issue(9)),
        refused("nothing_to_issue", materialised(), issue(9)),
        refused(
            "not_pending",
            pulled(),
            PlacementCommand::RecordPulled {
                directive_id: id(1),
            },
        ),
        refused(
            "no_drift",
            pulling(),
            PlacementCommand::RecordDrift {
                directive_id: id(1),
                observed: ImageDigest::sample(),
            },
        ),
        case(
            "not_sent",
            pulling(),
            at(
                1,
                LATE,
                PlacementCommand::RecordOutcomeUnknown {
                    directive_id: id(1),
                },
            ),
            Expect::Refused,
        ),
        refused(
            "deadline_not_passed",
            pull_sent(),
            PlacementCommand::RecordOutcomeUnknown {
                directive_id: id(1),
            },
        ),
        refused(
            "outcome_not_unknown",
            pulling(),
            PlacementCommand::RecordNotApplied {
                directive_id: id(1),
            },
        ),
        refused("not_materialised", pulled(), PlacementCommand::RollBackBase),
        refused(
            "base_referenced",
            materialised(),
            PlacementCommand::RequestEviction {
                scope: EvictionScope::Placement,
                references: 1,
            },
        ),
        refused(
            "no_previous_base",
            materialised(),
            PlacementCommand::RollBackBase,
        ),
        refused(
            "previous_base_held",
            refreshed(),
            PlacementCommand::RequestRefresh {
                runner_version: runner(332),
            },
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_accept_every_step_when_a_case_path_is_replayed() {
        // given
        let paths = paths();
        // when
        let refusals: Vec<(usize, Option<(usize, PlacementRejection)>)> = paths
            .into_iter()
            .map(|path| replay_path(path).1)
            .enumerate()
            .filter(|(_, refusal)| refusal.is_some())
            .collect();
        // then
        assert!(refusals.is_empty(), "{refusals:?}");
    }
}
