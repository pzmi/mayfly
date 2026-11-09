//! The event golden-fixture harness (ver §4, the "Events" and "Event upcasts" families).
//!
//! Layout, under one root (`crates/mayfly-control-domain/tests/fixtures/events/` for the
//! control plane), one directory per event type:
//!
//! - `<type>/v<n>.json`: the primary sample of every registered kind, current and legacy,
//!   exactly as the codec writes it: pretty JSON (two-space indent) and one trailing newline.
//! - `<type>/v<n>.<slug>.json`: further samples of the same kind, `<slug>` matching
//!   `[a-z0-9][a-z0-9-]*`. A current kind's samples round-trip byte for byte like the primary.
//! - `<type>/v<n>[.<slug>].expected.json`: beside each sample of a **legacy** kind, the current
//!   encoding its upcaster must produce.
//!
//! Each sample must decode, re-encode (current) or upcast (legacy) exactly, and fold through
//! `evolve` from `initial()` without a panic, and need no higher epoch than its aggregate's
//! `SUPPORTED_EPOCH` (`doc/event-store.md` §6.2: a build never writes what it could not decide
//! afterwards; [`GoldenFailure::EpochAboveSupported`]). Together, the samples of a kind must show every
//! field and enum variant of the payload's traced shape
//! ([`mayfly_control_domain::shape`]): an optional field or a variant no sample shows could be
//! renamed without breaking a frozen fixture, so it fails as [`GoldenFailure::Uncovered`]. A
//! payload part the tracer cannot see (an enum coded as a string or integer, a type that needs a
//! self-describing format, a part nothing reaches: [`shape::Gap`]) fails as
//! [`GoldenFailure::Untraceable`], and a registered kind whose set reports no shape at all as
//! [`GoldenFailure::NoShape`], so coverage is never skipped silently. A `#[serde(alias)]` name
//! counts as a field or variant of its own that a sample must show, and a sample that shows it
//! cannot re-encode byte for byte: aliases are refused in event payloads (pinned by
//! `should_refuse_an_alias_when_an_event_declares_one`).
//!
//! [`verify`] reports every failure, not just the first. Each kind of failure is pinned by a test
//! in `tests/harness/golden.rs` (for example `should_fail_when_a_registered_kind_has_no_fixture`,
//! `should_fail_when_a_fixture_is_not_registered`,
//! `should_fail_when_an_optional_field_is_in_no_sample` and
//! `should_fail_when_upcast_lands_on_another_kind`), except
//! [`GoldenFailure::UnreadableEntry`], an I/O error of a directory listing that no test can
//! force portably. Fixtures are never rewritten to get green (ver §4.1): a shape change adds a
//! version.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};

use mayfly_control_domain::shape::{self, Atom, Gap, Node, Shape};
use mayfly_control_domain::{
    Aggregate, AggregateRegistry, AggregateVisitor, DecodeError, DeterminismCases, EventKind,
    EventSet,
};
use serde_json::Value;

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
    /// A sample of a legacy kind has no `.expected.json` beside it.
    MissingUpcastExpectation {
        /// The legacy kind.
        kind: EventKind,
        /// Where the expectation was expected.
        path: PathBuf,
    },
    /// A file or empty directory under the root that no registered kind accounts for: an
    /// unknown type or version, a name outside the layout, or an expectation with no sample.
    UnregisteredFixture {
        /// The file.
        path: PathBuf,
    },
    /// A directory under the root (or the root itself) that could not be listed.
    UnreadableDir {
        /// The directory.
        path: PathBuf,
        /// The I/O error.
        error: String,
    },
    /// An entry of a directory listing that could not be read.
    UnreadableEntry {
        /// The directory, or the entry when its path is known.
        path: PathBuf,
        /// The I/O error.
        error: String,
    },
    /// A fixture file that could not be read as UTF-8 text.
    Unreadable {
        /// The file.
        path: PathBuf,
        /// The I/O error.
        error: String,
    },
    /// A sample that does not decode under its kind.
    Undecodable {
        /// The kind.
        kind: EventKind,
        /// The file.
        path: PathBuf,
        /// The decoder's message.
        error: String,
    },
    /// A legacy sample upcast into an event other than the current version of its own type.
    KindMismatch {
        /// The legacy kind.
        kind: EventKind,
        /// The sample.
        path: PathBuf,
        /// The current version of the same type: what the upcast must produce.
        expected: EventKind,
        /// What the upcast produced.
        decoded: EventKind,
    },
    /// Decode then encode did not reproduce the sample (or, for a legacy sample, its
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
    /// `evolve(initial(), event)` panicked for a decoded sample: `evolve` must be total.
    EvolvePanicked {
        /// The aggregate.
        aggregate: &'static str,
        /// The kind.
        kind: EventKind,
        /// The sample.
        path: PathBuf,
        /// The panic message.
        message: String,
    },
    /// A sample's event needs a higher epoch (`DomainEvent::REQUIRED_EPOCH`) than its
    /// aggregate's `SUPPORTED_EPOCH`: the build would refuse to write it (ES-I12).
    EpochAboveSupported {
        /// The aggregate.
        aggregate: &'static str,
        /// The kind.
        kind: EventKind,
        /// The sample.
        path: PathBuf,
        /// The epoch the event needs.
        required: u32,
        /// The aggregate's supported epoch.
        supported: u32,
    },
    /// Fields or variants of the kind's shape that no sample shows.
    Uncovered {
        /// The kind.
        kind: EventKind,
        /// Each missing field or variant, as [`Atom`] displays it.
        missing: Vec<String>,
    },
    /// Parts of the kind's payload type cannot be traced (see
    /// [`mayfly_control_domain::shape`]), so its coverage cannot be checked.
    Untraceable {
        /// The kind.
        kind: EventKind,
        /// Each untraced path (`""` is the root) and why.
        gaps: Vec<(String, Gap)>,
    },
    /// `EventSet::shape` returns `None` for a registered kind, so its coverage cannot be
    /// checked at all (an event set written by hand instead of with `event_set!`).
    NoShape {
        /// The kind.
        kind: EventKind,
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
            Self::UnreadableDir { path, error } => {
                write!(f, "cannot list {}: {error}", path.display())
            }
            Self::UnreadableEntry { path, error } => {
                write!(f, "cannot read an entry of {}: {error}", path.display())
            }
            Self::Unreadable { path, error } => {
                write!(f, "cannot read {}: {error}", path.display())
            }
            Self::Undecodable { kind, path, error } => {
                write!(f, "{kind} does not decode from {}: {error}", path.display())
            }
            Self::KindMismatch {
                kind,
                path,
                expected,
                decoded,
            } => write!(
                f,
                "legacy {kind} ({}) upcasts to {decoded}, not to the current {expected}",
                path.display()
            ),
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
            Self::EvolvePanicked {
                aggregate,
                kind,
                path,
                message,
            } => write!(
                f,
                "{aggregate}: evolve(initial, {kind}) panicked on {}: {message}",
                path.display()
            ),
            Self::EpochAboveSupported {
                aggregate,
                kind,
                path,
                required,
                supported,
            } => write!(
                f,
                "{aggregate}: {kind} ({}) needs epoch {required}, above the supported {supported}; \
                 raise SUPPORTED_EPOCH in a prepare release first",
                path.display()
            ),
            Self::Uncovered { kind, missing } => write!(
                f,
                "{kind}: no sample shows {}; add a v<n>.<slug>.json that does",
                missing.join(", ")
            ),
            Self::Untraceable { kind, gaps } => {
                let gaps: Vec<String> = gaps
                    .iter()
                    .map(|(path, gap)| {
                        let path = if path.is_empty() { "<root>" } else { path };
                        format!("`{path}` {gap}")
                    })
                    .collect();
                write!(
                    f,
                    "{kind}: the payload cannot be traced: {}; events use structs, externally \
                     tagged enums, Option, Vec, maps, plain strings and numbers, and the value \
                     types with a mayfly_domain::Sample (ADR 0018)",
                    gaps.join("; ")
                )
            }
            Self::NoShape { kind } => write!(
                f,
                "{kind}: EventSet::shape has no shape for a registered kind, so its coverage \
                 cannot be checked; declare the set with event_set!"
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
    /// Samples decoded, primary and extra, current and legacy.
    pub samples: usize,
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
        files: BTreeMap::new(),
        claimed: BTreeSet::new(),
        unregistered: Vec::new(),
        aggregates: Vec::new(),
        current: Vec::new(),
        legacy: Vec::new(),
        samples: 0,
        failures: Vec::new(),
    };
    run.scan();
    R::accept(&mut run);
    run.check_registry();
    run.report_unregistered();
    if run.failures.is_empty() {
        Ok(GoldenSummary {
            aggregates: run.aggregates.len(),
            current: run.current.len(),
            legacy: run.legacy.len(),
            samples: run.samples,
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

/// The fields and variants `samples` show, read against `shape`'s paths (pinned through
/// `should_fail_when_a_variant_is_in_no_sample`).
#[must_use]
pub fn observed(shape: &Shape, samples: &[Value]) -> BTreeSet<Atom> {
    let mut seen = BTreeSet::new();
    for sample in samples {
        observe(shape, sample, "", &mut seen);
    }
    seen
}

fn observe(shape: &Shape, value: &Value, path: &str, seen: &mut BTreeSet<Atom>) {
    match (shape.nodes.get(path), value) {
        (Some(Node::Struct(_)), Value::Object(map)) => {
            for (key, inner) in map {
                let field = shape::field_path(path, key);
                seen.insert(Atom::Field(field.clone()));
                observe(shape, inner, &field, seen);
            }
        }
        (Some(Node::Enum(_)), Value::String(variant)) => {
            seen.insert(Atom::Variant(path.to_owned(), variant.clone()));
        }
        (Some(Node::Enum(_)), Value::Object(map)) => {
            for (variant, inner) in map {
                seen.insert(Atom::Variant(path.to_owned(), variant.clone()));
                observe(shape, inner, &shape::variant_path(path, variant), seen);
            }
        }
        (Some(Node::Seq), Value::Array(items)) => {
            for item in items {
                observe(shape, item, &shape::element_path(path), seen);
            }
        }
        (Some(Node::Map), Value::Object(map)) => {
            for (key, inner) in map {
                observe(
                    shape,
                    &Value::String(key.clone()),
                    &shape::key_path(path),
                    seen,
                );
                observe(shape, inner, &shape::value_path(path), seen);
            }
        }
        _ => {}
    }
}

/// One fixture file's role, from `<type>/v<n>[.<slug>][.expected].json`.
struct FixtureName {
    event_type: String,
    version: u32,
    slug: Option<String>,
    expected: bool,
}

fn parse_name(root: &Path, path: &Path) -> Option<FixtureName> {
    let mut parts = path.strip_prefix(root).ok()?.components();
    let event_type = parts.next()?.as_os_str().to_str()?.to_owned();
    let file = parts.next()?.as_os_str().to_str()?;
    if parts.next().is_some() {
        return None;
    }
    let stem = file.strip_suffix(".json")?;
    let (stem, expected) = match stem.strip_suffix(".expected") {
        Some(stem) => (stem, true),
        None => (stem, false),
    };
    let (version, slug) = match stem.split_once('.') {
        Some((version, slug)) => (version, Some(slug)),
        None => (stem, None),
    };
    let digits = version.strip_prefix('v')?;
    let canonical = !digits.is_empty()
        && digits.bytes().all(|b| b.is_ascii_digit())
        && (digits == "0" || !digits.starts_with('0'));
    if !canonical || slug.is_some_and(|s| !valid_slug(s)) {
        return None;
    }
    Some(FixtureName {
        event_type,
        version: digits.parse().ok()?,
        slug: slug.map(str::to_owned),
        expected,
    })
}

fn valid_slug(slug: &str) -> bool {
    let mut bytes = slug.bytes();
    slug != "expected"
        && bytes
            .next()
            .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// The files found for one kind, by slug (`None` for the primary sample).
#[derive(Default, Clone)]
struct KindFiles {
    samples: BTreeMap<Option<String>, PathBuf>,
    expectations: BTreeMap<Option<String>, PathBuf>,
}

#[derive(Clone, Copy)]
enum Role {
    Current,
    /// A legacy kind and the current version of its type, if the set has one.
    Legacy(Option<EventKind>),
}

struct Run<'r> {
    root: &'r Path,
    files: BTreeMap<(String, u32), KindFiles>,
    claimed: BTreeSet<(String, u32)>,
    unregistered: Vec<PathBuf>,
    aggregates: Vec<&'static str>,
    current: Vec<EventKind>,
    legacy: Vec<EventKind>,
    samples: usize,
    failures: Vec<GoldenFailure>,
}

impl AggregateVisitor for Run<'_> {
    fn visit<A: DeterminismCases>(&mut self) {
        self.aggregates.push(A::NAME);
        let current = <A::Event as EventSet>::KINDS;
        for &kind in current {
            self.current.push(kind);
            self.check_kind::<A>(kind, Role::Current);
        }
        for &kind in <A::Event as EventSet>::LEGACY_KINDS {
            self.legacy.push(kind);
            let target = current
                .iter()
                .find(|k| k.event_type == kind.event_type)
                .copied();
            self.check_kind::<A>(kind, Role::Legacy(target));
        }
    }
}

impl Run<'_> {
    fn scan(&mut self) {
        let mut found = Vec::new();
        collect_files(self.root, true, &mut found, &mut self.failures);
        for path in found {
            match parse_name(self.root, &path) {
                Some(name) => {
                    let files = self
                        .files
                        .entry((name.event_type, name.version))
                        .or_default();
                    let by_slug = if name.expected {
                        &mut files.expectations
                    } else {
                        &mut files.samples
                    };
                    by_slug.insert(name.slug, path);
                }
                None => self.unregistered.push(path),
            }
        }
    }

    fn fixture(&self, kind: EventKind, slug: Option<&str>, expected: bool) -> PathBuf {
        let mut name = format!("v{}", kind.schema_version);
        if let Some(slug) = slug {
            name.push('.');
            name.push_str(slug);
        }
        if expected {
            name.push_str(".expected");
        }
        name.push_str(".json");
        self.root.join(kind.event_type).join(name)
    }

    fn read(&mut self, path: &Path) -> Option<String> {
        match fs::read_to_string(path) {
            Ok(text) => Some(text),
            Err(e) => {
                self.failures.push(GoldenFailure::Unreadable {
                    path: path.to_owned(),
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

    /// `evolve` must be total: a recorded event happened, so folding it never panics.
    fn check_evolve<A: Aggregate>(&mut self, kind: EventKind, path: &Path, event: &A::Event) {
        let folded = panic::catch_unwind(AssertUnwindSafe(|| {
            A::evolve(A::initial(), event);
        }));
        if let Err(payload) = folded {
            let message = payload
                .downcast_ref::<&str>()
                .map(|m| (*m).to_owned())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_default();
            self.failures.push(GoldenFailure::EvolvePanicked {
                aggregate: A::NAME,
                kind,
                path: path.to_owned(),
                message,
            });
        }
    }

    fn check_kind<A: Aggregate>(&mut self, kind: EventKind, role: Role) {
        let key = (kind.event_type.to_owned(), kind.schema_version);
        let files = self.files.get(&key).cloned().unwrap_or_default();
        self.claimed.insert(key);
        let mut slugs: BTreeSet<Option<String>> = files.samples.keys().cloned().collect();
        slugs.insert(None);
        match role {
            Role::Current => self
                .unregistered
                .extend(files.expectations.values().cloned()),
            Role::Legacy(_) => slugs.extend(files.expectations.keys().cloned()),
        }
        let mut samples = Vec::new();
        for slug in slugs {
            let expectation = match role {
                Role::Current => None,
                Role::Legacy(_) => match files.expectations.get(&slug) {
                    Some(path) => Some(path),
                    None if slug.is_none() || files.samples.contains_key(&slug) => {
                        self.failures.push(GoldenFailure::MissingUpcastExpectation {
                            kind,
                            path: self.fixture(kind, slug.as_deref(), true),
                        });
                        None
                    }
                    None => None,
                },
            };
            let Some(path) = files.samples.get(&slug) else {
                if slug.is_none() {
                    self.failures.push(GoldenFailure::MissingFixture {
                        kind,
                        path: self.fixture(kind, None, false),
                    });
                } else {
                    self.unregistered.extend(expectation.cloned());
                }
                continue;
            };
            let Some(text) = self.read(path) else {
                continue;
            };
            let Some(event) = self.decoded::<A::Event>(kind, path, &text) else {
                continue;
            };
            self.samples += 1;
            samples.extend(serde_json::from_str::<Value>(&text).ok());
            self.check_evolve::<A>(kind, path, &event);
            if event.required_epoch() > A::SUPPORTED_EPOCH {
                self.failures.push(GoldenFailure::EpochAboveSupported {
                    aggregate: A::NAME,
                    kind,
                    path: path.clone(),
                    required: event.required_epoch(),
                    supported: A::SUPPORTED_EPOCH,
                });
            }
            match role {
                Role::Current => self.compare(kind, path, &text, &event),
                Role::Legacy(target) => {
                    if let Some(expected) = target
                        && event.kind() != expected
                    {
                        self.failures.push(GoldenFailure::KindMismatch {
                            kind,
                            path: path.clone(),
                            expected,
                            decoded: event.kind(),
                        });
                        continue;
                    }
                    if let Some(expectation) = expectation
                        && let Some(expected) = self.read(expectation)
                    {
                        self.compare(kind, expectation, &expected, &event);
                    }
                }
            }
        }
        match <A::Event as EventSet>::shape(kind) {
            None => self.failures.push(GoldenFailure::NoShape { kind }),
            Some(shape) if !samples.is_empty() => self.check_coverage(kind, &shape, &samples),
            Some(_) => {}
        }
    }

    fn check_coverage(&mut self, kind: EventKind, shape: &Shape, samples: &[Value]) {
        if !shape.untraceable.is_empty() {
            self.failures.push(GoldenFailure::Untraceable {
                kind,
                gaps: shape
                    .untraceable
                    .iter()
                    .map(|(path, gap)| (path.clone(), *gap))
                    .collect(),
            });
            return;
        }
        let seen = observed(shape, samples);
        let missing: Vec<String> = shape
            .atoms()
            .difference(&seen)
            .map(ToString::to_string)
            .collect();
        if !missing.is_empty() {
            self.failures
                .push(GoldenFailure::Uncovered { kind, missing });
        }
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

    /// Files no registered kind claimed, in path order, after everything else.
    fn report_unregistered(&mut self) {
        for (key, files) in std::mem::take(&mut self.files) {
            if !self.claimed.contains(&key) {
                self.unregistered.extend(files.samples.into_values());
                self.unregistered.extend(files.expectations.into_values());
            }
        }
        self.unregistered.sort();
        self.unregistered.dedup();
        self.failures.extend(
            self.unregistered
                .drain(..)
                .map(|path| GoldenFailure::UnregisteredFixture { path }),
        );
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

/// Every file and empty subdirectory under `dir`. A missing root holds nothing; any other
/// listing error is its own failure, never a guess.
fn collect_files(
    dir: &Path,
    root: bool,
    out: &mut Vec<PathBuf>,
    failures: &mut Vec<GoldenFailure>,
) {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if root && e.kind() == std::io::ErrorKind::NotFound => return,
        Err(e) => {
            failures.push(GoldenFailure::UnreadableDir {
                path: dir.to_owned(),
                error: e.to_string(),
            });
            return;
        }
    };
    let mut empty = true;
    for entry in entries {
        empty = false;
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                failures.push(GoldenFailure::UnreadableEntry {
                    path: dir.to_owned(),
                    error: e.to_string(),
                });
                continue;
            }
        };
        let path = entry.path();
        match entry.file_type() {
            Ok(kind) if kind.is_dir() => collect_files(&path, false, out, failures),
            Ok(_) => out.push(path),
            Err(e) => failures.push(GoldenFailure::UnreadableEntry {
                path,
                error: e.to_string(),
            }),
        }
    }
    if empty && !root {
        out.push(dir.to_owned());
    }
}
