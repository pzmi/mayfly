//! Trace replay (ver §6.2 items 3 and 4): a [`Driver`] turns each model action into a stimulus
//! of the real code and reports a canonical [`Observation`] built from what production exposes;
//! [`replay`] compares it, at every state **the initial one included**, with the projection of
//! the model's state, and stops at the first divergence with a [`Divergence`] report.
//!
//! A driver reads the model only through a [`ModelView`], which refuses every path its
//! [`Driver::MODEL_READS`] does not declare: the environment's choices outside the
//! implementation (the provider's world), never the implementation's own state (pinned by
//! `should_refuse_a_read_when_the_path_is_not_declared`).

use std::collections::BTreeMap;
use std::fmt;

use crate::itf::{State, Trace, TraceError, Value};
use crate::manifest::Entry;

/// A canonical observation: named fields, each rendered as text, compared field by field.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Observation(pub BTreeMap<&'static str, String>);

impl Observation {
    /// Sets `field` to `value`.
    pub fn set(&mut self, field: &'static str, value: impl fmt::Display) {
        self.0.insert(field, value.to_string());
    }

    /// The fields whose values differ between `self` (expected) and `other` (actual), or that
    /// only one side has.
    #[must_use]
    pub fn differing(&self, other: &Self) -> Vec<&'static str> {
        let mut names: Vec<&'static str> = self.0.keys().chain(other.0.keys()).copied().collect();
        names.sort_unstable();
        names.dedup();
        names
            .into_iter()
            .filter(|name| self.0.get(name) != other.0.get(name))
            .collect()
    }
}

impl fmt::Display for Observation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (name, value) in &self.0 {
            writeln!(f, "  {name} = {value}")?;
        }
        Ok(())
    }
}

/// Why a driver could not apply a step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DriverError {
    /// The driver read a model path its `MODEL_READS` does not declare.
    UndeclaredRead(String),
    /// A declared path is missing from the state or has an unexpected type.
    BadRead(String),
    /// The model took an action the driver does not map.
    UnknownAction(String),
    /// The real code failed outside its decision (storage, codec): not a divergence.
    Harness(String),
}

impl fmt::Display for DriverError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UndeclaredRead(path) => {
                write!(
                    f,
                    "the driver read {path}, which MODEL_READS does not declare"
                )
            }
            Self::BadRead(what) => write!(f, "model read failed: {what}"),
            Self::UnknownAction(action) => write!(f, "no stimulus for model action {action}"),
            Self::Harness(what) => write!(f, "harness error: {what}"),
        }
    }
}

impl std::error::Error for DriverError {}

/// The parts of one model state a driver may read: the paths of `MODEL_READS` only.
#[derive(Debug, Clone, Copy)]
pub struct ModelView<'s> {
    state: &'s State,
    reads: &'static [&'static str],
}

impl<'s> ModelView<'s> {
    /// A view of `state` allowing `reads`.
    #[must_use]
    pub const fn new(state: &'s State, reads: &'static [&'static str]) -> Self {
        Self { state, reads }
    }

    /// The value at `path` (`var.field.field`), if `path` is declared.
    ///
    /// # Errors
    /// [`DriverError::UndeclaredRead`] for an undeclared path, [`DriverError::BadRead`] for a
    /// missing one.
    pub fn read(&self, path: &str) -> Result<&'s Value, DriverError> {
        if !self.reads.contains(&path) {
            return Err(DriverError::UndeclaredRead(path.to_owned()));
        }
        let mut parts = path.split('.');
        let first = parts.next().unwrap_or_default();
        let mut value = self
            .state
            .vars
            .get(first)
            .ok_or_else(|| DriverError::BadRead(format!("no variable {first}")))?;
        for part in parts {
            value = value
                .field(part)
                .ok_or_else(|| DriverError::BadRead(format!("no field {part} in {path}")))?;
        }
        Ok(value)
    }

    /// The integer at `path`.
    ///
    /// # Errors
    /// As [`ModelView::read`], or [`DriverError::BadRead`] when it is not an integer.
    pub fn int(&self, path: &str) -> Result<i128, DriverError> {
        self.read(path)?
            .as_int()
            .ok_or_else(|| DriverError::BadRead(format!("{path} is not an integer")))
    }

    /// The boolean at `path`.
    ///
    /// # Errors
    /// As [`ModelView::read`], or [`DriverError::BadRead`] when it is not a boolean.
    pub fn bool(&self, path: &str) -> Result<bool, DriverError> {
        self.read(path)?
            .as_bool()
            .ok_or_else(|| DriverError::BadRead(format!("{path} is not a boolean")))
    }
}

/// One step as a driver sees it: the action, its picks, and the declared parts of the states
/// before and after it.
#[derive(Debug, Clone, Copy)]
pub struct Step<'s> {
    /// `mbt::actionTaken`.
    pub action: &'s str,
    /// `mbt::nondetPicks` of the state reached.
    pub picks: &'s BTreeMap<String, Option<Value>>,
    /// The state the action started from (`None` for `init`).
    pub before: Option<ModelView<'s>>,
    /// The state it reached.
    pub after: ModelView<'s>,
}

impl Step<'_> {
    /// The pick `name`, which the action must have made.
    ///
    /// # Errors
    /// [`DriverError::BadRead`] when the action did not pick it.
    pub fn pick(&self, name: &str) -> Result<&Value, DriverError> {
        self.picks
            .get(name)
            .and_then(Option::as_ref)
            .ok_or_else(|| DriverError::BadRead(format!("{} picked no {name}", self.action)))
    }

    /// The declared state before the action.
    ///
    /// # Errors
    /// [`DriverError::BadRead`] for `init`.
    pub fn before(&self) -> Result<ModelView<'_>, DriverError> {
        self.before
            .ok_or_else(|| DriverError::BadRead(format!("{} has no state before it", self.action)))
    }
}

/// A conformance driver for one model.
pub trait Driver {
    /// The model's name (`lifecycle`).
    const MODEL: &'static str;
    /// The model paths the driver may read: environment choices only (ver §6.2 item 3).
    const MODEL_READS: &'static [&'static str];

    /// The canonical observation of a model state: what the comparison expects.
    ///
    /// # Errors
    /// [`DriverError::BadRead`] when the state lacks a projected variable.
    fn expected(state: &State) -> Result<Observation, DriverError>;

    /// Applies one step (`init` included) to the real code.
    ///
    /// # Errors
    /// [`DriverError`] when the step cannot be driven; a refusal by the real code is not an
    /// error but part of the observation.
    fn apply(&mut self, step: &Step<'_>) -> Result<(), DriverError>;

    /// The canonical observation of the real code, built from what production exposes.
    ///
    /// # Errors
    /// [`DriverError::Harness`] when it cannot be read.
    fn observe(&self) -> Result<Observation, DriverError>;
}

/// The first point where the real code and the model disagree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Divergence {
    /// The model.
    pub model: &'static str,
    /// The trace (its file name).
    pub trace: String,
    /// The seed it was made with.
    pub seed: String,
    /// The state index: 0 is the initial state.
    pub step: usize,
    /// The action that led there (`init` at step 0).
    pub action: String,
    /// The action's picks, `name = value`.
    pub picks: Vec<String>,
    /// The projection of the model's state.
    pub expected: Observation,
    /// The driver's observation of the real code.
    pub actual: Observation,
    /// The fields that differ.
    pub fields: Vec<&'static str>,
}

impl fmt::Display for Divergence {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "divergence: model {} trace {} seed {} step {} action {} picks [{}]",
            self.model,
            self.trace,
            self.seed,
            self.step,
            self.action,
            self.picks.join(", ")
        )?;
        for field in &self.fields {
            writeln!(
                f,
                "- {field} = {}",
                self.expected
                    .0
                    .get(field)
                    .map_or("<absent>", String::as_str)
            )?;
            writeln!(
                f,
                "+ {field} = {}",
                self.actual.0.get(field).map_or("<absent>", String::as_str)
            )?;
        }
        writeln!(f, "expected (model):")?;
        write!(f, "{}", self.expected)?;
        writeln!(f, "actual (implementation):")?;
        write!(f, "{}", self.actual)
    }
}

/// Why a replay did not pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplayError {
    /// The trace is unusable: a failure, never a pass.
    Trace {
        /// The trace.
        trace: String,
        /// Why.
        error: TraceError,
    },
    /// The driver could not apply a step.
    Driver {
        /// The trace.
        trace: String,
        /// The state index.
        step: usize,
        /// The action.
        action: String,
        /// Why.
        error: DriverError,
    },
    /// The real code diverged from the model.
    Divergence(Box<Divergence>),
}

impl ReplayError {
    /// The divergence, if that is what this is.
    #[must_use]
    pub fn divergence(&self) -> Option<&Divergence> {
        match self {
            Self::Divergence(d) => Some(d),
            Self::Trace { .. } | Self::Driver { .. } => None,
        }
    }
}

impl fmt::Display for ReplayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Trace { trace, error } => write!(f, "trace {trace}: {error}"),
            Self::Driver {
                trace,
                step,
                action,
                error,
            } => write!(f, "trace {trace} step {step} action {action}: {error}"),
            Self::Divergence(d) => write!(f, "{d}"),
        }
    }
}

impl std::error::Error for ReplayError {}

/// What a passing replay covered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Replayed {
    /// States compared, the initial one included.
    pub states: usize,
    /// Actions taken, by name.
    pub actions: BTreeMap<String, usize>,
}

/// Checks a trace's length against its manifest entry: at most `max_steps + 1` states. A
/// generated trace may be shorter, since Quint ends a run when no action is enabled (the
/// lifecycle stops at `Closed`); a committed one is pinned byte for byte by `formal-freshness`.
///
/// # Errors
/// [`TraceError::Length`] for a longer trace.
pub fn check_length(trace: &Trace, entry: &Entry) -> Result<(), TraceError> {
    let most = entry.max_steps.saturating_add(1);
    if trace.states.len() > most {
        return Err(TraceError::Length {
            found: trace.states.len(),
            most,
            exact: false,
        });
    }
    Ok(())
}

/// Replays `trace` (made as `entry` says) through `driver`, comparing the observation with the
/// model's projection after every state, the initial one included.
///
/// # Errors
/// [`ReplayError`]: the first divergence, a driver error or a trace error.
pub fn replay<D: Driver>(
    driver: &mut D,
    entry: &Entry,
    trace: &Trace,
) -> Result<Replayed, ReplayError> {
    check_length(trace, entry).map_err(|error| ReplayError::Trace {
        trace: entry.id().to_owned(),
        error,
    })?;
    let mut actions = BTreeMap::new();
    let mut previous: Option<&State> = None;
    for (index, state) in trace.states.iter().enumerate() {
        let failed = |error| ReplayError::Driver {
            trace: entry.id().to_owned(),
            step: index,
            action: state.action.clone(),
            error,
        };
        let step = Step {
            action: &state.action,
            picks: &state.picks,
            before: previous.map(|p| ModelView::new(p, D::MODEL_READS)),
            after: ModelView::new(state, D::MODEL_READS),
        };
        driver.apply(&step).map_err(failed)?;
        let expected = D::expected(state).map_err(failed)?;
        let actual = driver.observe().map_err(failed)?;
        let fields = expected.differing(&actual);
        if !fields.is_empty() {
            return Err(ReplayError::Divergence(Box::new(Divergence {
                model: D::MODEL,
                trace: entry.id().to_owned(),
                seed: entry.seed.clone(),
                step: index,
                action: state.action.clone(),
                picks: state
                    .picks
                    .iter()
                    .filter_map(|(k, v)| v.as_ref().map(|v| format!("{k} = {v}")))
                    .collect(),
                expected,
                actual,
                fields,
            })));
        }
        *actions.entry(state.action.clone()).or_insert(0) += 1;
        previous = Some(state);
    }
    Ok(Replayed {
        states: trace.states.len(),
        actions,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> State {
        let mut vars = BTreeMap::new();
        let mut world = BTreeMap::new();
        world.insert("instance".to_owned(), Value::Int(2));
        vars.insert("w".to_owned(), Value::Record(world));
        vars.insert("a".to_owned(), Value::Record(BTreeMap::new()));
        State {
            index: 0,
            vars,
            action: "init".to_owned(),
            picks: BTreeMap::new(),
        }
    }

    #[test]
    fn should_refuse_a_read_when_the_path_is_not_declared() {
        // given
        let state = state();
        let view = ModelView::new(&state, &["w.instance"]);

        // when
        let (declared, undeclared) = (view.int("w.instance"), view.read("a"));

        // then
        assert_eq!(declared, Ok(2));
        assert_eq!(undeclared, Err(DriverError::UndeclaredRead("a".to_owned())));
    }

    #[test]
    fn should_name_each_differing_field_when_observations_are_compared() {
        // given
        let mut expected = Observation::default();
        expected.set("phase", "Busy");
        expected.set("job", 1);
        let mut actual = expected.clone();
        actual.set("job", 2);
        actual.set("extra", true);

        // when
        let fields = expected.differing(&actual);

        // then
        assert_eq!(fields, ["extra", "job"]);
    }

    #[test]
    fn should_render_every_error_and_the_divergence_when_displayed() {
        // given
        let mut expected = Observation::default();
        expected.set("phase", "Busy");
        let mut actual = Observation::default();
        actual.set("phase", "Listening");
        let divergence = Divergence {
            model: "lifecycle",
            trace: "t.itf.json".to_owned(),
            seed: "0x1".to_owned(),
            step: 4,
            action: "bindJob".to_owned(),
            picks: vec!["job = 1".to_owned()],
            fields: expected.differing(&actual),
            expected,
            actual,
        };
        let errors = [
            DriverError::UndeclaredRead("a".to_owned()),
            DriverError::BadRead("b".to_owned()),
            DriverError::UnknownAction("c".to_owned()),
            DriverError::Harness("d".to_owned()),
        ];
        let replay_errors = [
            ReplayError::Trace {
                trace: "t".to_owned(),
                error: TraceError::NoMbt { state: 0 },
            },
            ReplayError::Driver {
                trace: "t".to_owned(),
                step: 1,
                action: "x".to_owned(),
                error: DriverError::UnknownAction("x".to_owned()),
            },
            ReplayError::Divergence(Box::new(divergence.clone())),
        ];

        // when
        let shown = divergence.to_string();
        let messages: Vec<String> = errors.iter().map(ToString::to_string).collect();
        let replays: Vec<String> = replay_errors.iter().map(ToString::to_string).collect();

        // then
        assert!(shown.starts_with(
            "divergence: model lifecycle trace t.itf.json seed 0x1 step 4 action bindJob picks [job = 1]"
        ));
        assert!(
            shown.contains("- phase = Busy\n+ phase = Listening\n"),
            "{shown}"
        );
        assert!(messages.iter().all(|m| !m.is_empty()));
        assert_eq!(replays[2], shown);
        assert!(replay_errors[0].divergence().is_none());
        assert!(replay_errors[2].divergence().is_some());
        assert!(replays[1].contains("step 1 action x"));
    }

    #[test]
    fn should_refuse_a_pick_or_a_prior_state_when_the_step_lacks_it() {
        // given
        let state = state();
        let picks = BTreeMap::from([("n".to_owned(), None)]);
        let step = Step {
            action: "init",
            picks: &picks,
            before: None,
            after: ModelView::new(&state, &["w", "w.instance", "w.absent"]),
        };

        // when
        let (pick, before) = (step.pick("n").is_err(), step.before().is_err());

        // then
        assert!(pick && before);
        assert!(step.after.int("w").is_err());
        assert!(step.after.bool("w.instance").is_err());
        assert!(step.after.read("w.absent").is_err());
    }
}
