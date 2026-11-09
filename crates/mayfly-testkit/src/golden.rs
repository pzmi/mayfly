//! The event golden-fixture harness (ver §4, the "Events" and "Event upcasts" families).
//!
//! Layout, under one root (`crates/mayfly-control-domain/tests/fixtures/events/` for the
//! control plane):
//!
//! - `<type>/v<n>.json`: the payload of every registered kind, current and legacy, exactly as
//!   the codec writes it: pretty JSON (two-space indent) and one trailing newline.
//! - `<type>/v<n>.expected.json`: beside each **legacy** fixture, the current encoding its
//!   upcaster must produce.
//!
//! [`verify`] fails on a missing fixture, an unregistered file, a payload that does not
//! decode or does not re-encode byte for byte, a kind registered twice, a malformed kind, and
//! a version chain with a gap or a legacy version not older than the current one (each pinned
//! by a test in `tests/harness/golden.rs`, e.g. `should_fail_when_a_registered_kind_has_no_fixture`
//! and `should_fail_when_a_fixture_is_not_registered`). Fixtures are
//! never rewritten to get green (ver §4.1): a shape change adds a version.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use mayfly_control_domain::{
    AggregateRegistry, AggregateVisitor, DecodeError, DeterminismCases, EventKind, EventSet,
};

/// One reason the fixtures and the registry disagree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GoldenFailure {
    /// A registered kind has no `v<n>.json`.
    MissingFixture {
        /// The kind.
        kind: EventKind,
        /// Where the fixture was expected.
        path: PathBuf,
    },
    /// A legacy kind has no `v<n>.expected.json`.
    MissingUpcastExpectation {
        /// The legacy kind.
        kind: EventKind,
        /// Where the expectation was expected.
        path: PathBuf,
    },
    /// A file or directory under the root that no registered kind accounts for.
    UnregisteredFixture {
        /// The file.
        path: PathBuf,
    },
    /// A fixture that could not be read or decoded under its kind.
    Undecodable {
        /// The kind.
        kind: EventKind,
        /// The file.
        path: PathBuf,
        /// The reader's or decoder's message.
        error: String,
    },
    /// A current fixture decoded into an event of another kind.
    KindMismatch {
        /// The fixture's kind.
        kind: EventKind,
        /// The decoded event's kind.
        decoded: EventKind,
    },
    /// Decode then encode did not reproduce the file (or, for a legacy fixture, its
    /// expectation) byte for byte.
    NotByteIdentical {
        /// The kind.
        kind: EventKind,
        /// The file compared against.
        path: PathBuf,
        /// The file's content.
        expected: String,
        /// The codec's output.
        actual: String,
    },
    /// The same kind is registered more than once, or two aggregates share a name.
    Duplicate {
        /// What is duplicated.
        what: String,
    },
    /// An `EVENT_TYPE` that is not `[a-z][a-z0-9_]*` or a `SCHEMA_VERSION` of 0.
    InvalidKind {
        /// The kind.
        kind: EventKind,
    },
    /// An event type whose registered versions are not exactly `1..=current` with one current
    /// version at the top.
    VersionChain {
        /// The event type.
        event_type: &'static str,
        /// What is wrong.
        detail: String,
    },
}

impl fmt::Display for GoldenFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingFixture { kind, path } => {
                write!(f, "missing fixture for {kind}: {}", path.display())
            }
            Self::MissingUpcastExpectation { kind, path } => write!(
                f,
                "missing upcast expectation for legacy {kind}: {}",
                path.display()
            ),
            Self::UnregisteredFixture { path } => {
                write!(f, "unregistered fixture: {}", path.display())
            }
            Self::Undecodable { kind, path, error } => {
                write!(f, "{kind} does not decode from {}: {error}", path.display())
            }
            Self::KindMismatch { kind, decoded } => {
                write!(f, "fixture of {kind} decoded as {decoded}")
            }
            Self::NotByteIdentical {
                kind,
                path,
                expected,
                actual,
            } => write!(
                f,
                "{kind} does not re-encode byte for byte ({}):\n--- file\n{expected}--- codec\n{actual}",
                path.display()
            ),
            Self::Duplicate { what } => write!(f, "registered twice: {what}"),
            Self::InvalidKind { kind } => write!(
                f,
                "invalid kind {kind}: the type must be [a-z][a-z0-9_]* and the version at least 1"
            ),
            Self::VersionChain { event_type, detail } => {
                write!(f, "version chain of {event_type}: {detail}")
            }
        }
    }
}

/// Every failure of one run, in a stable order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoldenFailures(pub Vec<GoldenFailure>);

impl fmt::Display for GoldenFailures {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "{} golden fixture failure(s):", self.0.len())?;
        for failure in &self.0 {
            writeln!(f, "- {failure}")?;
        }
        Ok(())
    }
}

impl std::error::Error for GoldenFailures {}

/// What a passing run covered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GoldenSummary {
    /// Aggregates visited.
    pub aggregates: usize,
    /// Current kinds round-tripped.
    pub current: usize,
    /// Legacy kinds upcast.
    pub legacy: usize,
}

/// Checks every event kind of `R` against the fixtures under `root`.
///
/// A missing `root` holds no fixtures: every registered kind is then reported missing, so a
/// mistyped path never passes once anything is registered.
///
/// # Errors
/// Every [`GoldenFailure`] found, not just the first.
pub fn verify<R: AggregateRegistry>(root: &Path) -> Result<GoldenSummary, GoldenFailures> {
    let mut run = Run {
        root,
        aggregates: Vec::new(),
        current: Vec::new(),
        legacy: Vec::new(),
        failures: Vec::new(),
    };
    R::accept(&mut run);
    run.check_registry();
    run.check_unregistered();
    if run.failures.is_empty() {
        Ok(GoldenSummary {
            aggregates: run.aggregates.len(),
            current: run.current.len(),
            legacy: run.legacy.len(),
        })
    } else {
        Err(GoldenFailures(run.failures))
    }
}

/// [`verify`], panicking with the full report: the one-line form for a test.
///
/// # Panics
/// When [`verify`] fails.
pub fn assert_golden<R: AggregateRegistry>(root: &Path) -> GoldenSummary {
    match verify::<R>(root) {
        Ok(summary) => summary,
        Err(failures) => panic!("{failures}"),
    }
}

/// The canonical fixture form of `event`'s payload: pretty JSON and one trailing newline.
///
/// # Errors
/// The serializer's error.
pub fn encode<E: EventSet>(event: &E) -> Result<String, serde_json::Error> {
    let mut out = Vec::new();
    event.encode(&mut serde_json::Serializer::pretty(&mut out))?;
    out.push(b'\n');
    Ok(String::from_utf8(out).expect("serde_json writes UTF-8"))
}

/// Decodes a payload stored under `kind` from JSON text, refusing trailing data.
///
/// # Errors
/// [`DecodeError::UnknownKind`] or the JSON error.
pub fn decode<E: EventSet>(
    event_type: &str,
    schema_version: u32,
    json: &str,
) -> Result<E, DecodeError<serde_json::Error>> {
    let mut deserializer = serde_json::Deserializer::from_str(json);
    let event = E::decode(event_type, schema_version, &mut deserializer)?;
    deserializer.end().map_err(DecodeError::Malformed)?;
    Ok(event)
}

#[derive(Clone, Copy)]
enum Missing {
    Fixture,
    Expectation,
}

struct Run<'r> {
    root: &'r Path,
    aggregates: Vec<&'static str>,
    current: Vec<EventKind>,
    legacy: Vec<EventKind>,
    failures: Vec<GoldenFailure>,
}

impl AggregateVisitor for Run<'_> {
    fn visit<A: DeterminismCases>(&mut self) {
        self.aggregates.push(A::NAME);
        for &kind in <A::Event as EventSet>::KINDS {
            self.current.push(kind);
            self.check_current::<A::Event>(kind);
        }
        for &kind in <A::Event as EventSet>::LEGACY_KINDS {
            self.legacy.push(kind);
            self.check_legacy::<A::Event>(kind);
        }
    }
}

impl Run<'_> {
    fn fixture(&self, kind: EventKind, suffix: &str) -> PathBuf {
        self.root
            .join(kind.event_type)
            .join(format!("v{}{suffix}", kind.schema_version))
    }

    /// The file's text; `None` after recording a missing or unreadable file.
    fn read(&mut self, kind: EventKind, path: PathBuf, missing: Missing) -> Option<String> {
        match fs::read_to_string(&path) {
            Ok(text) => Some(text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                self.failures.push(match missing {
                    Missing::Fixture => GoldenFailure::MissingFixture { kind, path },
                    Missing::Expectation => GoldenFailure::MissingUpcastExpectation { kind, path },
                });
                None
            }
            Err(e) => {
                self.failures.push(GoldenFailure::Undecodable {
                    kind,
                    path,
                    error: e.to_string(),
                });
                None
            }
        }
    }

    fn decoded<E: EventSet>(&mut self, kind: EventKind, path: &Path, text: &str) -> Option<E> {
        match decode::<E>(kind.event_type, kind.schema_version, text) {
            Ok(event) => Some(event),
            Err(e) => {
                self.failures.push(GoldenFailure::Undecodable {
                    kind,
                    path: path.to_owned(),
                    error: e.to_string(),
                });
                None
            }
        }
    }

    fn compare<E: EventSet>(&mut self, kind: EventKind, path: &Path, expected: &str, event: &E) {
        let actual = match encode(event) {
            Ok(actual) => actual,
            Err(e) => {
                self.failures.push(GoldenFailure::Undecodable {
                    kind,
                    path: path.to_owned(),
                    error: e.to_string(),
                });
                return;
            }
        };
        if actual != expected {
            self.failures.push(GoldenFailure::NotByteIdentical {
                kind,
                path: path.to_owned(),
                expected: expected.to_owned(),
                actual,
            });
        }
    }

    fn check_current<E: EventSet>(&mut self, kind: EventKind) {
        let path = self.fixture(kind, ".json");
        let Some(text) = self.read(kind, path.clone(), Missing::Fixture) else {
            return;
        };
        let Some(event) = self.decoded::<E>(kind, &path, &text) else {
            return;
        };
        if event.kind() != kind {
            self.failures.push(GoldenFailure::KindMismatch {
                kind,
                decoded: event.kind(),
            });
            return;
        }
        self.compare(kind, &path, &text, &event);
    }

    fn check_legacy<E: EventSet>(&mut self, kind: EventKind) {
        let path = self.fixture(kind, ".json");
        let text = self.read(kind, path.clone(), Missing::Fixture);
        let expectation = self.fixture(kind, ".expected.json");
        let expected = self.read(kind, expectation.clone(), Missing::Expectation);
        let (Some(text), Some(expected)) = (text, expected) else {
            return;
        };
        let Some(event) = self.decoded::<E>(kind, &path, &text) else {
            return;
        };
        self.compare(kind, &expectation, &expected, &event);
    }

    fn check_registry(&mut self) {
        let mut names = BTreeSet::new();
        for name in &self.aggregates {
            if !names.insert(*name) {
                self.failures.push(GoldenFailure::Duplicate {
                    what: format!("aggregate {name}"),
                });
            }
        }
        let mut seen = BTreeSet::new();
        let mut chains: BTreeMap<&'static str, (BTreeSet<u32>, BTreeSet<u32>)> = BTreeMap::new();
        let all = self.current.iter().map(|k| (*k, true));
        let all = all.chain(self.legacy.iter().map(|k| (*k, false)));
        let all: Vec<_> = all.collect();
        for (kind, current) in all {
            if !valid_kind(kind) {
                self.failures.push(GoldenFailure::InvalidKind { kind });
            }
            if !seen.insert(kind) {
                self.failures.push(GoldenFailure::Duplicate {
                    what: format!("event kind {kind}"),
                });
            }
            let chain = chains.entry(kind.event_type).or_default();
            if current {
                chain.0.insert(kind.schema_version);
            } else {
                chain.1.insert(kind.schema_version);
            }
        }
        for (event_type, (current, legacy)) in chains {
            if let Some(detail) = chain_problem(&current, &legacy) {
                self.failures
                    .push(GoldenFailure::VersionChain { event_type, detail });
            }
        }
    }

    fn check_unregistered(&mut self) {
        let mut allowed: BTreeSet<PathBuf> = BTreeSet::new();
        for kind in &self.current {
            allowed.insert(self.fixture(*kind, ".json"));
        }
        for kind in &self.legacy {
            allowed.insert(self.fixture(*kind, ".json"));
            allowed.insert(self.fixture(*kind, ".expected.json"));
        }
        let mut found = Vec::new();
        collect_files(self.root, true, &mut found, &mut self.failures);
        found.sort();
        for path in found {
            if !allowed.contains(&path) {
                self.failures
                    .push(GoldenFailure::UnregisteredFixture { path });
            }
        }
    }
}

fn valid_kind(kind: EventKind) -> bool {
    let mut bytes = kind.event_type.bytes();
    kind.schema_version >= 1
        && bytes.next().is_some_and(|b| b.is_ascii_lowercase())
        && bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

fn chain_problem(current: &BTreeSet<u32>, legacy: &BTreeSet<u32>) -> Option<String> {
    let current: Vec<u32> = current.iter().copied().collect();
    let [top] = current.as_slice() else {
        return Some(format!(
            "expected exactly one current version, found {current:?}"
        ));
    };
    if let Some(newer) = legacy.iter().find(|v| *v >= top) {
        return Some(format!(
            "legacy version {newer} is not older than the current version {top}"
        ));
    }
    let registered: BTreeSet<u32> = legacy.iter().copied().chain([*top]).collect();
    let missing: Vec<u32> = (1..*top).filter(|v| !registered.contains(v)).collect();
    if missing.is_empty() {
        None
    } else {
        Some(format!(
            "versions {missing:?} below the current version {top} have no upcaster"
        ))
    }
}

/// Every file and empty subdirectory under `dir`; a missing `dir` holds nothing.
fn collect_files(
    dir: &Path,
    root: bool,
    out: &mut Vec<PathBuf>,
    failures: &mut Vec<GoldenFailure>,
) {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
        Err(e) => {
            failures.push(GoldenFailure::UnregisteredFixture {
                path: dir.join(format!("<unreadable: {e}>")),
            });
            return;
        }
    };
    let mut empty = true;
    for entry in entries.flatten() {
        empty = false;
        let path = entry.path();
        if path.is_dir() {
            collect_files(&path, false, out, failures);
        } else {
            out.push(path);
        }
    }
    if empty && !root {
        out.push(dir.to_owned());
    }
}
