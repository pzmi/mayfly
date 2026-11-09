//! The `DETERMINISM_CASES` harness (ver §3.1, fnd §6.2).
//!
//! For every aggregate of a registry, [`verify`] requires at least one accepted and one
//! refused case with unique names, then for each case folds the history twice into two equal
//! states, decides the command once from each, and requires equal results (refusals
//! included) with the declared outcome. Accepted events are folded onto both states again, so
//! `evolve` is held to the same rule. Each failure is pinned by a test in
//! `tests/harness/determinism.rs`.
//!
//! A registry admits only `DeterminismCases` aggregates, so "an aggregate without cases" is
//! either a compile error (no impl) or a [`DeterminismFailure::NoAcceptedCase`] /
//! [`DeterminismFailure::NoRefusedCase`] here.

use std::collections::BTreeSet;
use std::fmt;

use mayfly_control_domain::{
    Aggregate, AggregateRegistry, AggregateVisitor, DeterminismCase, DeterminismCases, Expect,
    replay,
};

/// One way an aggregate broke the determinism contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeterminismFailure {
    /// Two aggregates share a `NAME`.
    DuplicateAggregate {
        /// The name.
        aggregate: &'static str,
    },
    /// The aggregate declares no accepted case.
    NoAcceptedCase {
        /// The aggregate.
        aggregate: &'static str,
    },
    /// The aggregate declares no refused case.
    NoRefusedCase {
        /// The aggregate.
        aggregate: &'static str,
    },
    /// Two cases of one aggregate share a name.
    DuplicateCase {
        /// The aggregate.
        aggregate: &'static str,
        /// The case.
        case: &'static str,
    },
    /// Folding the same events twice gave different states.
    NonDeterministicEvolve {
        /// The aggregate.
        aggregate: &'static str,
        /// The case.
        case: &'static str,
        /// The two states, `Debug`-formatted.
        first: String,
        /// See `first`.
        second: String,
    },
    /// Deciding the same command from equal states gave different results.
    NonDeterministicDecide {
        /// The aggregate.
        aggregate: &'static str,
        /// The case.
        case: &'static str,
        /// The two results, `Debug`-formatted.
        first: String,
        /// See `first`.
        second: String,
    },
    /// `decide` was deterministic but not as the case declared.
    UnexpectedOutcome {
        /// The aggregate.
        aggregate: &'static str,
        /// The case.
        case: &'static str,
        /// The declared outcome.
        expected: Expect,
        /// The result, `Debug`-formatted.
        actual: String,
    },
}

impl fmt::Display for DeterminismFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateAggregate { aggregate } => {
                write!(f, "aggregate {aggregate} is registered twice")
            }
            Self::NoAcceptedCase { aggregate } => {
                write!(f, "{aggregate}: no accepted determinism case")
            }
            Self::NoRefusedCase { aggregate } => {
                write!(f, "{aggregate}: no refused determinism case")
            }
            Self::DuplicateCase { aggregate, case } => {
                write!(f, "{aggregate}: case {case} is declared twice")
            }
            Self::NonDeterministicEvolve {
                aggregate,
                case,
                first,
                second,
            } => write!(
                f,
                "{aggregate}/{case}: evolve is not deterministic: {first} != {second}"
            ),
            Self::NonDeterministicDecide {
                aggregate,
                case,
                first,
                second,
            } => write!(
                f,
                "{aggregate}/{case}: decide is not deterministic: {first} != {second}"
            ),
            Self::UnexpectedOutcome {
                aggregate,
                case,
                expected,
                actual,
            } => write!(f, "{aggregate}/{case}: expected {expected:?}, got {actual}"),
        }
    }
}

/// Every failure of one run, in registry and case order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeterminismFailures(pub Vec<DeterminismFailure>);

impl fmt::Display for DeterminismFailures {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "{} determinism failure(s):", self.0.len())?;
        for failure in &self.0 {
            writeln!(f, "- {failure}")?;
        }
        Ok(())
    }
}

impl std::error::Error for DeterminismFailures {}

/// What a passing run covered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeterminismSummary {
    /// Aggregates visited.
    pub aggregates: usize,
    /// Cases decided.
    pub cases: usize,
}

/// Runs every aggregate's cases.
///
/// # Errors
/// Every [`DeterminismFailure`] found, not just the first.
pub fn verify<R: AggregateRegistry>() -> Result<DeterminismSummary, DeterminismFailures> {
    let mut run = Run::default();
    R::accept(&mut run);
    if run.failures.is_empty() {
        Ok(DeterminismSummary {
            aggregates: run.names.len(),
            cases: run.cases,
        })
    } else {
        Err(DeterminismFailures(run.failures))
    }
}

/// [`verify`], panicking with the full report: the one-line form for a test.
///
/// # Panics
/// When [`verify`] fails.
pub fn assert_deterministic<R: AggregateRegistry>() -> DeterminismSummary {
    match verify::<R>() {
        Ok(summary) => summary,
        Err(failures) => panic!("{failures}"),
    }
}

#[derive(Default)]
struct Run {
    names: BTreeSet<&'static str>,
    cases: usize,
    failures: Vec<DeterminismFailure>,
}

impl AggregateVisitor for Run {
    fn visit<A: DeterminismCases>(&mut self) {
        let aggregate = A::NAME;
        if !self.names.insert(aggregate) {
            self.failures
                .push(DeterminismFailure::DuplicateAggregate { aggregate });
            return;
        }
        let cases = A::determinism_cases();
        if !cases.iter().any(|c| c.expect == Expect::Accepted) {
            self.failures
                .push(DeterminismFailure::NoAcceptedCase { aggregate });
        }
        if !cases.iter().any(|c| c.expect == Expect::Refused) {
            self.failures
                .push(DeterminismFailure::NoRefusedCase { aggregate });
        }
        let mut seen = BTreeSet::new();
        for case in cases {
            if !seen.insert(case.name) {
                self.failures.push(DeterminismFailure::DuplicateCase {
                    aggregate,
                    case: case.name,
                });
            }
            self.cases += 1;
            if let Some(failure) = check_case::<A>(case) {
                self.failures.push(failure);
            }
        }
    }
}

fn check_case<A: Aggregate>(case: DeterminismCase<A>) -> Option<DeterminismFailure> {
    let aggregate = A::NAME;
    let name = case.name;
    let first_state = replay::<A>(&case.history);
    let second_state = replay::<A>(&case.history);
    if first_state != second_state {
        return Some(DeterminismFailure::NonDeterministicEvolve {
            aggregate,
            case: name,
            first: format!("{first_state:?}"),
            second: format!("{second_state:?}"),
        });
    }
    let first = A::decide(&first_state, case.command.clone());
    let second = A::decide(&second_state, case.command);
    if first != second {
        return Some(DeterminismFailure::NonDeterministicDecide {
            aggregate,
            case: name,
            first: format!("{first:?}"),
            second: format!("{second:?}"),
        });
    }
    let outcome = if first.is_ok() {
        Expect::Accepted
    } else {
        Expect::Refused
    };
    if outcome != case.expect {
        return Some(DeterminismFailure::UnexpectedOutcome {
            aggregate,
            case: name,
            expected: case.expect,
            actual: format!("{first:?}"),
        });
    }
    let events = first.unwrap_or_default();
    let after_first = events.iter().fold(first_state, A::evolve);
    let after_second = events.iter().fold(second_state, A::evolve);
    if after_first != after_second {
        return Some(DeterminismFailure::NonDeterministicEvolve {
            aggregate,
            case: name,
            first: format!("{after_first:?}"),
            second: format!("{after_second:?}"),
        });
    }
    None
}
