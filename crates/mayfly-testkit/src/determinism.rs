//! The `DETERMINISM_CASES` harness (ver §3.1, fnd §6.2).
//!
//! For every aggregate of a registry, [`verify`] requires at least one accepted and one
//! refused case, at least one case with a non-empty history, unique case names and a valid
//! `Rejection::CODES` list. For each case it folds the history twice into two states, decides
//! the command once from each, and requires the two to agree, refusals included, then folds
//! the accepted events onto both states again. Agreement is checked four ways, because a
//! hand-written `PartialEq` can hide a field: `PartialEq`, the `Debug` form of states and
//! results, each event's encoded bytes ([`crate::golden::encode`]) and the refusal's
//! `Rejection::code()`. The result must match the declared outcome, a refusal's code must be in
//! `Rejection::CODES`, and every code listed there must be returned by some refused case, so
//! each one is pinned in the decisions golden below.
//!
//! Each aggregate's codes and case outcomes are also pinned in a decisions golden,
//! `<root>/<aggregate>.txt` (for the control plane, `crates/mayfly-control-domain/tests/
//! fixtures/decisions/`): it catches what one process cannot (an outcome that changes between
//! builds or runs), and makes a renamed refusal code show in review. Unlike event fixtures it may
//! be regenerated, deliberately (ver §4.2): a missing or different golden fails with the text
//! to commit.
//!
//! Each failure is pinned by a test in `tests/harness/determinism.rs`. A registry admits only
//! `DeterminismCases` aggregates, so "an aggregate without cases" is either a compile error (no
//! impl) or a [`DeterminismFailure::NoAcceptedCase`] / [`DeterminismFailure::NoRefusedCase`]
//! here.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{self, Write as _};
use std::fs;
use std::path::{Path, PathBuf};

use mayfly_control_domain::{
    Aggregate, AggregateRegistry, AggregateVisitor, DeterminismCase, DeterminismCases, EventKind,
    EventSet, Expect, Rejection, replay,
};

use crate::golden;

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
    /// Every case of the aggregate starts from the initial state.
    NoHistoryCase {
        /// The aggregate.
        aggregate: &'static str,
    },
    /// `Rejection::CODES` has a code that is not `[a-z][a-z0-9_]*`, or one twice.
    InvalidCodes {
        /// The aggregate.
        aggregate: &'static str,
        /// What is wrong.
        detail: String,
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
        /// The two results: `Debug`, then the encoded events or the refusal code.
        first: String,
        /// See `first`.
        second: String,
    },
    /// An accepted event did not encode.
    Unencodable {
        /// The aggregate.
        aggregate: &'static str,
        /// The case.
        case: &'static str,
        /// The serializer's message.
        error: String,
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
    /// A refusal's code is not in `Rejection::CODES`.
    UndeclaredCode {
        /// The aggregate.
        aggregate: &'static str,
        /// The case.
        case: &'static str,
        /// The code.
        code: &'static str,
    },
    /// A code in `Rejection::CODES` that no refused case returns, so the decisions golden pins
    /// no outcome for it.
    UncoveredCode {
        /// The aggregate.
        aggregate: &'static str,
        /// The code.
        code: &'static str,
    },
    /// The aggregate's decisions golden does not exist.
    MissingDecisions {
        /// The aggregate.
        aggregate: &'static str,
        /// Where it was expected.
        path: PathBuf,
        /// What it should hold, if these outcomes are intended.
        actual: String,
    },
    /// The aggregate's codes or case outcomes differ from its decisions golden.
    DecisionsDiffer {
        /// The aggregate.
        aggregate: &'static str,
        /// The golden.
        path: PathBuf,
        /// The golden's content.
        expected: String,
        /// The run's.
        actual: String,
    },
    /// The decisions directory, an entry of it or a golden could not be read.
    UnreadableDecisions {
        /// The path.
        path: PathBuf,
        /// The I/O error.
        error: String,
    },
    /// A file in the decisions directory that belongs to no registered aggregate.
    UnregisteredDecisions {
        /// The file.
        path: PathBuf,
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
            Self::NoHistoryCase { aggregate } => {
                write!(f, "{aggregate}: no determinism case has a history")
            }
            Self::InvalidCodes { aggregate, detail } => {
                write!(f, "{aggregate}: Rejection::CODES {detail}")
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
            Self::Unencodable {
                aggregate,
                case,
                error,
            } => write!(f, "{aggregate}/{case}: an event does not encode: {error}"),
            Self::UnexpectedOutcome {
                aggregate,
                case,
                expected,
                actual,
            } => write!(f, "{aggregate}/{case}: expected {expected:?}, got {actual}"),
            Self::UndeclaredCode {
                aggregate,
                case,
                code,
            } => write!(
                f,
                "{aggregate}/{case}: refusal code {code} is not in Rejection::CODES"
            ),
            Self::UncoveredCode { aggregate, code } => write!(
                f,
                "{aggregate}: no refused case returns code {code}; add a determinism case that \
                 does"
            ),
            Self::MissingDecisions {
                aggregate,
                path,
                actual,
            } => write!(
                f,
                "{aggregate}: missing decisions golden {}; if these outcomes are intended, \
                 commit it as:\n{actual}",
                path.display()
            ),
            Self::DecisionsDiffer {
                aggregate,
                path,
                expected,
                actual,
            } => write!(
                f,
                "{aggregate}: decisions differ from {}; if the change is intended, commit \
                 the new text and say why:\n--- golden\n{expected}--- run\n{actual}",
                path.display()
            ),
            Self::UnreadableDecisions { path, error } => {
                write!(f, "cannot read {}: {error}", path.display())
            }
            Self::UnregisteredDecisions { path } => {
                write!(
                    f,
                    "decisions golden of no registered aggregate: {}",
                    path.display()
                )
            }
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

/// Runs every aggregate's cases and compares each aggregate's decisions with
/// `<decisions>/<aggregate>.txt`. A missing `decisions` directory holds no goldens.
///
/// # Errors
/// Every [`DeterminismFailure`] found, not just the first.
pub fn verify<R: AggregateRegistry>(
    decisions: &Path,
) -> Result<DeterminismSummary, DeterminismFailures> {
    let mut run = Run {
        root: decisions,
        names: BTreeSet::new(),
        cases: 0,
        failures: Vec::new(),
    };
    R::accept(&mut run);
    run.check_unregistered();
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
pub fn assert_deterministic<R: AggregateRegistry>(decisions: &Path) -> DeterminismSummary {
    match verify::<R>(decisions) {
        Ok(summary) => summary,
        Err(failures) => panic!("{failures}"),
    }
}

/// The decisions golden's text for `aggregate`: its codes, then each case's outcome in name
/// order (`outcomes` maps a case name to its rendered outcome).
fn render(aggregate: &str, codes: &[&str], outcomes: &BTreeMap<&str, String>) -> String {
    let mut text =
        format!("# {aggregate}: refusal codes, then the outcome of each determinism case\n");
    let codes: BTreeSet<&str> = codes.iter().copied().collect();
    for code in codes {
        let _ = writeln!(text, "code {code}");
    }
    for (case, outcome) in outcomes {
        let _ = write!(text, "case {case} {outcome}");
    }
    text
}

/// What `decide` returned, in the forms compared: each event's kind and encodings, or the
/// refusal's code.
#[derive(PartialEq)]
struct Outcome {
    events: Vec<(EventKind, String, String)>,
    code: Option<&'static str>,
}

impl Outcome {
    fn of<A: Aggregate>(
        result: &Result<Vec<A::Event>, A::Rejection>,
    ) -> Result<Self, serde_json::Error> {
        match result {
            Ok(events) => {
                let events = events
                    .iter()
                    .map(|e| Ok((e.kind(), golden::encode(e)?, compact(e)?)))
                    .collect::<Result<_, serde_json::Error>>()?;
                Ok(Self { events, code: None })
            }
            Err(rejection) => Ok(Self {
                events: Vec::new(),
                code: Some(rejection.code()),
            }),
        }
    }

    fn show(&self) -> String {
        match self.code {
            Some(code) => format!("refused {code}\n"),
            None => {
                let mut text = String::from("accepted\n");
                for (kind, _, compact) in &self.events {
                    let _ = writeln!(text, "  {kind} {compact}");
                }
                text
            }
        }
    }
}

fn compact<E: EventSet>(event: &E) -> Result<String, serde_json::Error> {
    let mut out = Vec::new();
    event.encode(&mut serde_json::Serializer::new(&mut out))?;
    Ok(String::from_utf8(out).expect("serde_json writes UTF-8"))
}

fn valid_code(code: &str) -> bool {
    let mut bytes = code.bytes();
    bytes.next().is_some_and(|b| b.is_ascii_lowercase())
        && bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

struct Run<'r> {
    root: &'r Path,
    names: BTreeSet<&'static str>,
    cases: usize,
    failures: Vec<DeterminismFailure>,
}

impl AggregateVisitor for Run<'_> {
    fn visit<A: DeterminismCases>(&mut self) {
        let aggregate = A::NAME;
        if !self.names.insert(aggregate) {
            self.failures
                .push(DeterminismFailure::DuplicateAggregate { aggregate });
            return;
        }
        let before = self.failures.len();
        let cases = A::determinism_cases();
        if !cases.iter().any(|c| c.expect == Expect::Accepted) {
            self.failures
                .push(DeterminismFailure::NoAcceptedCase { aggregate });
        }
        if !cases.iter().any(|c| c.expect == Expect::Refused) {
            self.failures
                .push(DeterminismFailure::NoRefusedCase { aggregate });
        }
        if !cases.iter().any(|c| !c.history.is_empty()) {
            self.failures
                .push(DeterminismFailure::NoHistoryCase { aggregate });
        }
        let codes = <A::Rejection as Rejection>::CODES;
        self.check_codes(aggregate, codes);
        let mut outcomes = BTreeMap::new();
        let mut refused = BTreeSet::new();
        for case in cases {
            self.cases += 1;
            let name = case.name;
            if outcomes.contains_key(name) {
                self.failures.push(DeterminismFailure::DuplicateCase {
                    aggregate,
                    case: name,
                });
                continue;
            }
            match check_case::<A>(case) {
                Ok(outcome) => {
                    refused.extend(outcome.code);
                    outcomes.insert(name, outcome.show());
                }
                Err(failure) => self.failures.push(failure),
            }
        }
        // Uncovered codes and the golden are checked only for an aggregate that kept every
        // other rule, so one broken case reports itself once, not again as a changed golden.
        if self.failures.len() == before {
            for code in codes.iter().filter(|code| !refused.contains(*code)) {
                self.failures
                    .push(DeterminismFailure::UncoveredCode { aggregate, code });
            }
        }
        if self.failures.len() == before {
            self.check_golden(aggregate, &render(aggregate, codes, &outcomes));
        }
    }
}

impl Run<'_> {
    fn check_codes(&mut self, aggregate: &'static str, codes: &[&'static str]) {
        let mut seen = BTreeSet::new();
        for code in codes {
            let detail = if !valid_code(code) {
                format!("has {code:?}, which is not [a-z][a-z0-9_]*")
            } else if !seen.insert(*code) {
                format!("lists {code} twice")
            } else {
                continue;
            };
            self.failures
                .push(DeterminismFailure::InvalidCodes { aggregate, detail });
        }
    }

    fn check_golden(&mut self, aggregate: &'static str, actual: &str) {
        let path = self.root.join(format!("{aggregate}.txt"));
        match fs::read_to_string(&path) {
            Ok(expected) if expected == actual => {}
            Ok(expected) => self.failures.push(DeterminismFailure::DecisionsDiffer {
                aggregate,
                path,
                expected,
                actual: actual.to_owned(),
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                self.failures.push(DeterminismFailure::MissingDecisions {
                    aggregate,
                    path,
                    actual: actual.to_owned(),
                });
            }
            Err(e) => self.failures.push(DeterminismFailure::UnreadableDecisions {
                path,
                error: e.to_string(),
            }),
        }
    }

    fn check_unregistered(&mut self) {
        let entries = match fs::read_dir(self.root) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
            Err(e) => {
                self.failures.push(DeterminismFailure::UnreadableDecisions {
                    path: self.root.to_owned(),
                    error: e.to_string(),
                });
                return;
            }
        };
        let expected: BTreeSet<String> = self.names.iter().map(|n| format!("{n}.txt")).collect();
        let mut orphans = Vec::new();
        for entry in entries {
            match entry {
                Ok(entry) => {
                    let known = entry
                        .file_name()
                        .to_str()
                        .is_some_and(|name| expected.contains(name));
                    if !known {
                        orphans.push(entry.path());
                    }
                }
                Err(e) => self.failures.push(DeterminismFailure::UnreadableDecisions {
                    path: self.root.to_owned(),
                    error: e.to_string(),
                }),
            }
        }
        orphans.sort();
        self.failures.extend(
            orphans
                .into_iter()
                .map(|path| DeterminismFailure::UnregisteredDecisions { path }),
        );
    }
}

/// Two values that a hand-written `PartialEq` or `Debug` cannot pass off as equal.
fn same<T: PartialEq + fmt::Debug>(first: &T, second: &T) -> bool {
    first == second && format!("{first:?}") == format!("{second:?}")
}

/// One case: its outcome, or the first rule it breaks.
fn check_case<A: Aggregate>(case: DeterminismCase<A>) -> Result<Outcome, DeterminismFailure> {
    let aggregate = A::NAME;
    let name = case.name;
    let evolve_failure =
        |first: &A::State, second: &A::State| DeterminismFailure::NonDeterministicEvolve {
            aggregate,
            case: name,
            first: format!("{first:?}"),
            second: format!("{second:?}"),
        };
    let first_state = replay::<A>(&case.history);
    let second_state = replay::<A>(&case.history);
    if !same(&first_state, &second_state) {
        return Err(evolve_failure(&first_state, &second_state));
    }
    let first = A::decide(&first_state, case.command.clone());
    let second = A::decide(&second_state, case.command);
    let unencodable = |e: serde_json::Error| DeterminismFailure::Unencodable {
        aggregate,
        case: name,
        error: e.to_string(),
    };
    let first_outcome = Outcome::of::<A>(&first).map_err(unencodable)?;
    let second_outcome = Outcome::of::<A>(&second).map_err(unencodable)?;
    if !same(&first, &second) || first_outcome != second_outcome {
        return Err(DeterminismFailure::NonDeterministicDecide {
            aggregate,
            case: name,
            first: format!("{first:?} {}", first_outcome.show().trim_end()),
            second: format!("{second:?} {}", second_outcome.show().trim_end()),
        });
    }
    let outcome = if first.is_ok() {
        Expect::Accepted
    } else {
        Expect::Refused
    };
    if outcome != case.expect {
        return Err(DeterminismFailure::UnexpectedOutcome {
            aggregate,
            case: name,
            expected: case.expect,
            actual: format!("{first:?}"),
        });
    }
    if let Some(code) = first_outcome.code
        && !<A::Rejection as Rejection>::CODES.contains(&code)
    {
        return Err(DeterminismFailure::UndeclaredCode {
            aggregate,
            case: name,
            code,
        });
    }
    let events = first.unwrap_or_default();
    let after_first = events.iter().fold(first_state, A::evolve);
    let after_second = events.iter().fold(second_state, A::evolve);
    if !same(&after_first, &after_second) {
        return Err(evolve_failure(&after_first, &after_second));
    }
    Ok(first_outcome)
}
