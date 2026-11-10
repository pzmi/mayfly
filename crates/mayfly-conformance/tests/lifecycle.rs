//! The lifecycle driver over the committed regression traces (conformance level, ver §2 and
//! §6.2): every trace replays against the real `Attempt`; each known divergence still
//! reproduces; a deliberately mutated transition is caught at the first divergent step, named
//! by step, action and field (ver §6.2 item 7); and every observed field is sensitive to the
//! model (item 8).

use std::path::PathBuf;

use mayfly_conformance::itf::{self, Trace, Value};
use mayfly_conformance::lifecycle::{self, KNOWN_DIVERGENCES, canonical_action};
use mayfly_conformance::manifest::{self, Entry, Manifest};
use mayfly_conformance::replay::{Divergence, ReplayError};
use mayfly_control_domain::attempt::{
    Attempt, AttemptCommand, AttemptEvent, AttemptRejection, AttemptState, Jit, Phase,
};
use mayfly_control_domain::{Aggregate, Command};

fn regression() -> Manifest {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../formal/traces/regression/manifest.toml");
    manifest::load(&path).unwrap_or_else(|e| panic!("{e}"))
}

fn lifecycle_entries(manifest: &Manifest) -> Vec<Entry> {
    manifest
        .entries
        .iter()
        .filter(|e| e.model_name() == "lifecycle")
        .cloned()
        .collect()
}

fn load(manifest: &Manifest, step: &str) -> (Entry, Trace) {
    let entry = manifest
        .entries
        .iter()
        .find(|e| e.step == step)
        .unwrap_or_else(|| panic!("no regression trace {step}"))
        .clone();
    let text = std::fs::read_to_string(manifest.path_of(&entry)).unwrap_or_else(|e| panic!("{e}"));
    let trace = itf::parse(&text).unwrap_or_else(|e| panic!("{e}"));
    (entry, trace)
}

fn divergence(result: Result<impl std::fmt::Debug, ReplayError>) -> Divergence {
    match result {
        Err(ReplayError::Divergence(d)) => *d,
        other => panic!("expected a divergence, got {other:?}"),
    }
}

#[test]
fn should_replay_every_committed_lifecycle_trace_when_the_aggregate_conforms() {
    // given
    let manifest = regression();
    let entries = lifecycle_entries(&manifest);

    // when
    let failures: Vec<String> = entries
        .iter()
        .filter(|e| !e.step.starts_with("divergence"))
        .filter_map(|entry| {
            let (_, trace) = load(&manifest, &entry.step);
            lifecycle::replay::<Attempt>(entry, &trace)
                .err()
                .map(|e| e.to_string())
        })
        .collect();

    // then
    assert!(entries.len() >= 12, "{entries:?}");
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn should_reproduce_each_known_divergence_when_its_regression_trace_is_replayed() {
    // given
    let manifest = regression();

    for known in KNOWN_DIVERGENCES {
        let (entry, trace) = load(&manifest, known.regression);

        // when
        let found = divergence(lifecycle::replay::<Attempt>(&entry, &trace));

        // then: at the script's last step, and classified as exactly this entry
        assert_eq!(found.step, trace.states.len() - 1, "{found}");
        assert!(
            known.action == "*" || canonical_action(&found.action) == known.action,
            "{found}"
        );
        assert_eq!(
            lifecycle::known(&found).map(|k| k.id),
            Some(known.id),
            "{found}"
        );
    }
}

/// `Attempt` with one row answered differently: `RequestRetirement` of a runner that holds a
/// JIT handle retires at once instead of waiting for its deregistration (the I21 row).
#[derive(Debug, Clone, Copy, Default)]
struct RetiresAtOnce;

impl Aggregate for RetiresAtOnce {
    const NAME: &'static str = Attempt::NAME;
    type State = AttemptState;
    type Body = AttemptCommand;
    type Event = AttemptEvent;
    type Rejection = AttemptRejection;

    fn initial() -> AttemptState {
        Attempt::initial()
    }

    fn decide(
        state: &AttemptState,
        command: Command<AttemptCommand>,
    ) -> Result<Vec<AttemptEvent>, AttemptRejection> {
        // decided as if no handle were held, so it retires at once
        match (&command.body, state.phase) {
            (AttemptCommand::RequestRetirement { .. }, Phase::Listening) => {
                let unregistered = AttemptState {
                    jit: Jit::None,
                    ..state.clone()
                };
                Attempt::decide(&unregistered, command)
            }
            _ => Attempt::decide(state, command),
        }
    }

    fn evolve(state: AttemptState, event: &AttemptEvent) -> AttemptState {
        Attempt::evolve(state, event)
    }
}

/// `Attempt` whose `evolve` forgets that a stop took effect: `VmStopped` leaves `vm_running`.
#[derive(Debug, Clone, Copy, Default)]
struct StopLeavesRunning;

impl Aggregate for StopLeavesRunning {
    const NAME: &'static str = Attempt::NAME;
    type State = AttemptState;
    type Body = AttemptCommand;
    type Event = AttemptEvent;
    type Rejection = AttemptRejection;

    fn initial() -> AttemptState {
        Attempt::initial()
    }

    fn decide(
        state: &AttemptState,
        command: Command<AttemptCommand>,
    ) -> Result<Vec<AttemptEvent>, AttemptRejection> {
        Attempt::decide(state, command)
    }

    fn evolve(state: AttemptState, event: &AttemptEvent) -> AttemptState {
        let running = state.vm_running;
        let next = Attempt::evolve(state, event);
        match event {
            AttemptEvent::VmStopped(..) => AttemptState {
                vm_running: running,
                ..next
            },
            _ => next,
        }
    }
}

#[test]
fn should_catch_a_mutated_decision_at_the_first_divergent_step_when_one_row_answers_differently() {
    // given: the I21 scenario, Listening with a handle, then RequestRetirement at step 13
    let manifest = regression();
    let (entry, trace) = load(&manifest, "retirementWaitsForDeregistration");
    let first_retirement = trace
        .states
        .iter()
        .position(|s| s.action == "requestRetirement")
        .unwrap_or_else(|| panic!("no requestRetirement"));

    // when
    let real = lifecycle::replay::<Attempt>(&entry, &trace);
    let mutant = divergence(lifecycle::replay::<RetiresAtOnce>(&entry, &trace));

    // then
    assert!(real.is_ok(), "{real:?}");
    assert_eq!(first_retirement, 13);
    assert_eq!(mutant.step, 13, "{mutant}");
    assert_eq!(mutant.action, "requestRetirement");
    assert_eq!(mutant.fields, ["phase", "retire_requested"], "{mutant}");
    assert_eq!(mutant.expected.0["phase"], "Listening");
    assert_eq!(mutant.actual.0["phase"], "Retiring");
    assert_eq!(lifecycle::known(&mutant), None);
}

#[test]
fn should_catch_a_mutated_evolve_at_the_first_divergent_step_when_a_stop_is_forgotten() {
    // given: the happy path, whose stop is answered at step 19
    let manifest = regression();
    let (entry, trace) = load(&manifest, "happyPath");

    // when
    let mutant = divergence(lifecycle::replay::<StopLeavesRunning>(&entry, &trace));

    // then
    assert_eq!(mutant.step, 19, "{mutant}");
    assert_eq!(canonical_action(&mutant.action), "replySucceeded");
    assert_eq!(mutant.fields, ["vm_running"], "{mutant}");
    assert_eq!(mutant.expected.0["vm_running"], "false");
    assert_eq!(mutant.actual.0["vm_running"], "true");
}

/// Sets field `field` of the model's `a` at state `index`.
fn edit(trace: &mut Trace, index: usize, field: &str, value: Value) {
    let Some(Value::Record(a)) = trace.states[index].vars.get_mut("a") else {
        panic!("state {index} has no record a");
    };
    a.insert(field.to_owned(), value);
}

fn text(s: &str) -> Value {
    Value::Str(s.to_owned())
}

#[test]
fn should_diverge_exactly_there_when_one_model_variable_is_edited_for_each_observed_field() {
    // given: (trace, state, model field, new value, the observed field that must differ)
    let cases: Vec<(&str, usize, &str, Value, &str)> = vec![
        ("happyPath", 3, "phase", text("Verifying"), "phase"),
        ("happyPath", 3, "tries", Value::Int(2), "tries"),
        ("happyPath", 3, "pending", text("VerifyGuest"), "pending"),
        ("happyPath", 3, "unknown", Value::Bool(true), "unknown"),
        ("happyPath", 3, "name", Value::Int(2), "name"),
        ("happyPath", 3, "generation", Value::Int(2), "generation"),
        ("happyPath", 3, "instance", Value::Int(2), "instance"),
        ("happyPath", 3, "vmRunning", Value::Bool(true), "vm_running"),
        ("happyPath", 3, "epoch", Value::Int(2), "epoch"),
        ("happyPath", 3, "granted", Value::Bool(true), "granted"),
        (
            "happyPath",
            3,
            "freshEpoch",
            Value::Int(1),
            "fresh_verification",
        ),
        ("happyPath", 3, "jit", text("issued"), "jit"),
        ("happyPath", 13, "job", Value::Int(2), "job"),
        (
            "happyPath",
            3,
            "retireRequested",
            Value::Bool(true),
            "retire_requested",
        ),
        ("happyPath", 3, "diagnostics", text("lost"), "diagnostics"),
        (
            "happyPath",
            3,
            "runnerExited",
            Value::Bool(true),
            "runner_exited",
        ),
        (
            "stageFailureBacksOffThenRetires",
            2,
            "stage",
            text("boot"),
            "stage",
        ),
        (
            "stopOfVanishedVmQuarantines",
            12,
            "vanished",
            Value::Bool(false),
            "vanished",
        ),
    ];
    let manifest = regression();

    for (script, index, field, value, observed) in cases {
        let (entry, mut trace) = load(&manifest, script);
        edit(&mut trace, index, field, value);

        // when
        let found = divergence(lifecycle::replay::<Attempt>(&entry, &trace));

        // then
        assert_eq!(
            (found.step, found.fields.as_slice()),
            (index, [observed].as_slice()),
            "{script} a.{field}: {found}"
        );
    }
}

#[test]
fn should_fail_the_replay_when_a_trace_is_longer_than_its_manifest_allows() {
    // given
    let manifest = regression();
    let (mut entry, trace) = load(&manifest, "happyPath");
    entry.max_steps = trace.states.len() - 2;

    // when
    let result = lifecycle::replay::<Attempt>(&entry, &trace);

    // then
    assert!(
        matches!(
            result,
            Err(ReplayError::Trace {
                error: itf::TraceError::Length { .. },
                ..
            })
        ),
        "{result:?}"
    );
}

#[test]
fn should_fail_the_replay_when_the_model_takes_an_action_the_driver_does_not_map() {
    // given
    let manifest = regression();
    let (entry, mut trace) = load(&manifest, "happyPath");
    trace.states[1].action = "teleport".to_owned();

    // when
    let result = lifecycle::replay::<Attempt>(&entry, &trace);

    // then
    assert!(
        matches!(&result, Err(ReplayError::Driver { step: 1, .. })),
        "{result:?}"
    );
}

#[test]
fn should_compare_the_initial_state_when_the_model_starts_elsewhere() {
    // given
    let manifest = regression();
    let (entry, mut trace) = load(&manifest, "happyPath");
    edit(&mut trace, 0, "epoch", Value::Int(3));

    // when
    let found = divergence(lifecycle::replay::<Attempt>(&entry, &trace));

    // then
    assert_eq!(found.step, 0);
    assert_eq!(found.action, "tinit");
    assert_eq!(found.fields, ["epoch"]);
}

#[test]
fn should_classify_no_divergence_as_known_when_a_field_or_value_differs() {
    // given: the known clone divergence with another actual value
    let manifest = regression();
    let (entry, trace) = load(&manifest, "divergenceCloneAnsweredAfterRemoval");
    let mut found = divergence(lifecycle::replay::<Attempt>(&entry, &trace));
    found.actual.set("instance", 2);

    // when
    let classified = lifecycle::known(&found);

    // then
    assert_eq!(classified, None);
}
