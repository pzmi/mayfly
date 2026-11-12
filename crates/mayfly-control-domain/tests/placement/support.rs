//! Paths through a placement's life, driven through its public `decide` and `evolve`. Every
//! directive of a path has the same id, so one answer stimulus names whatever is pending.

use mayfly_control_domain::attempt::RefusalReason;
use mayfly_control_domain::image::RunnerVersion;
use mayfly_control_domain::placement::{
    EvictionScope, ImagePlacement, PlacementCommand, PlacementEvent, PlacementRejection,
    PlacementState,
};
use mayfly_control_domain::{Actor, Aggregate, Command};
use mayfly_domain::{CommandId, Deadline, DirectiveId, Epoch, HostId, ImageDigest, Millis, Sample};

pub type Step = Command<PlacementCommand>;

pub const POLICY: Actor = Actor::Automation {
    policy: "placement_manager",
};

/// Before the deadline every path's directives carry.
pub const EARLY: u64 = 1_000;
/// After it.
pub const LATE: u64 = 70_000;

pub fn at(run_epoch: u64, now: u64, body: PlacementCommand) -> Step {
    Command::new(
        CommandId::from_u128(0xC1),
        Millis::new(now),
        Epoch::new(run_epoch),
        POLICY,
        body,
    )
}

pub fn auto(body: PlacementCommand) -> Step {
    at(1, EARLY, body)
}

pub const fn runner(minor: u32) -> RunnerVersion {
    RunnerVersion::new(2, minor, 0)
}

pub const fn id() -> DirectiveId {
    DirectiveId::from_u128(0xD001)
}

pub fn digest() -> ImageDigest {
    ImageDigest::sample()
}

pub fn other_digest() -> ImageDigest {
    ImageDigest::new(&format!("sha256:{}", "1".repeat(64))).unwrap_or_else(|e| panic!("{e}"))
}

pub fn request() -> PlacementCommand {
    PlacementCommand::RequestPlacement {
        host_id: HostId::sample(),
        digest: digest(),
        runner_version: runner(330),
    }
}

pub const fn issue() -> PlacementCommand {
    PlacementCommand::IssueDirective {
        directive_id: id(),
        deadline: Deadline::at(Millis::new(60_000)),
    }
}

pub const fn sent() -> PlacementCommand {
    PlacementCommand::RecordSent {
        directive_id: id(),
        deadline: Deadline::at(Millis::new(60_000)),
    }
}

pub const fn pulled() -> PlacementCommand {
    PlacementCommand::RecordPulled { directive_id: id() }
}

pub const fn materialised() -> PlacementCommand {
    PlacementCommand::RecordMaterialised {
        directive_id: id(),
        disk_bytes: None,
    }
}

pub const fn evicted() -> PlacementCommand {
    PlacementCommand::RecordEvicted { directive_id: id() }
}

pub const fn refused() -> PlacementCommand {
    PlacementCommand::RecordRefused {
        directive_id: id(),
        reason: RefusalReason::EffectFailed,
    }
}

pub const fn unknown() -> PlacementCommand {
    PlacementCommand::RecordOutcomeUnknown { directive_id: id() }
}

pub const fn not_applied() -> PlacementCommand {
    PlacementCommand::RecordNotApplied { directive_id: id() }
}

pub const fn refresh(minor: u32) -> PlacementCommand {
    PlacementCommand::RequestRefresh {
        runner_version: runner(minor),
    }
}

pub const fn evict(scope: EvictionScope, references: u32) -> PlacementCommand {
    PlacementCommand::RequestEviction { scope, references }
}

/// Decides `step` and folds what it accepted.
pub fn apply(
    state: &PlacementState,
    step: Step,
) -> Result<(PlacementState, Vec<PlacementEvent>), PlacementRejection> {
    let events = ImagePlacement::decide(state, step)?;
    let next = events.iter().fold(state.clone(), ImagePlacement::evolve);
    Ok((next, events))
}

/// The state after `steps`, every one accepted.
///
/// # Panics
/// When a step is refused: the path is wrong, not the aggregate under test.
pub fn reach(steps: impl IntoIterator<Item = Step>) -> PlacementState {
    steps
        .into_iter()
        .enumerate()
        .fold(ImagePlacement::initial(), |state, (index, step)| {
            apply(&state, step)
                .unwrap_or_else(|r| panic!("step {index} of the path refused: {r:?}"))
                .0
        })
}

/// The refusal code of `step` on `state`, or `"accepted"`.
pub fn outcome(state: &PlacementState, step: Step) -> String {
    match ImagePlacement::decide(state, step) {
        Ok(_) => "accepted".to_owned(),
        Err(rejection) => mayfly_control_domain::Rejection::code(&rejection).to_owned(),
    }
}

/// The steps to a materialised base of runner 2.330.0.
pub fn to_materialised() -> Vec<Step> {
    [request(), issue(), pulled(), issue(), materialised()]
        .into_iter()
        .map(auto)
        .collect()
}

/// The steps to a base of 2.331.0 with 2.330.0 kept as the previous.
pub fn to_refreshed() -> Vec<Step> {
    let mut path = to_materialised();
    path.extend([refresh(331), issue(), materialised()].map(auto));
    path
}

fn path(steps: impl IntoIterator<Item = PlacementCommand>) -> PlacementState {
    reach(steps.into_iter().map(auto))
}

fn after(base: Vec<Step>, more: impl IntoIterator<Item = PlacementCommand>) -> PlacementState {
    let mut steps = base;
    steps.extend(more.into_iter().map(auto));
    reach(steps)
}

/// One state per phase, and the variants that decide differently (a pending directive, an
/// unknown outcome, a previous base kept).
pub fn matrix_states() -> Vec<(&'static str, PlacementState)> {
    let mut pull_unknown = vec![auto(request()), auto(issue()), auto(sent())];
    pull_unknown.push(at(1, LATE, unknown()));
    vec![
        ("unrequested", ImagePlacement::initial()),
        ("wanted", path([request()])),
        ("pulling", path([request(), issue()])),
        ("pull_sent", path([request(), issue(), sent()])),
        ("pull_unknown", reach(pull_unknown)),
        ("pulled", path([request(), issue(), pulled()])),
        (
            "materialising",
            path([request(), issue(), pulled(), issue()]),
        ),
        ("materialised", reach(to_materialised())),
        (
            "refreshing",
            after(to_materialised(), [refresh(331), issue()]),
        ),
        ("refreshed", reach(to_refreshed())),
        (
            "eviction_requested",
            after(to_materialised(), [evict(EvictionScope::Placement, 0)]),
        ),
        (
            "evicting",
            after(
                to_materialised(),
                [evict(EvictionScope::Placement, 0), issue()],
            ),
        ),
        (
            "evicted",
            after(
                to_materialised(),
                [evict(EvictionScope::Placement, 0), issue(), evicted()],
            ),
        ),
        ("failed", path([request(), issue(), refused()])),
    ]
}

/// One stimulus of every command type (and the variants that decide differently).
pub fn stimuli() -> Vec<(&'static str, Step)> {
    vec![
        ("request_same", auto(request())),
        (
            "request_other_digest",
            auto(PlacementCommand::RequestPlacement {
                host_id: HostId::sample(),
                digest: other_digest(),
                runner_version: runner(330),
            }),
        ),
        ("issue", auto(issue())),
        ("record_sent", auto(sent())),
        ("record_pulled", auto(pulled())),
        (
            "record_drift",
            auto(PlacementCommand::RecordDrift {
                directive_id: id(),
                observed: other_digest(),
            }),
        ),
        ("record_materialised", auto(materialised())),
        ("record_evicted", auto(evicted())),
        ("record_refused", auto(refused())),
        ("unknown_before_deadline", auto(unknown())),
        ("unknown_after_deadline", at(1, LATE, unknown())),
        ("record_not_applied", auto(not_applied())),
        ("refresh_new", auto(refresh(332))),
        ("refresh_current", auto(refresh(330))),
        ("roll_back", auto(PlacementCommand::RollBackBase)),
        (
            "evict_placement_unreferenced",
            auto(evict(EvictionScope::Placement, 0)),
        ),
        (
            "evict_placement_referenced",
            auto(evict(EvictionScope::Placement, 2)),
        ),
        ("evict_previous", auto(evict(EvictionScope::Previous, 0))),
        ("stale_run", at(0, EARLY, issue())),
    ]
}
