//! The shape of an event payload: every struct field and enum variant its serde form can hold,
//! found by tracing the type's `Deserialize` impl, with no format and no sample (ADR 0018).
//!
//! The golden harness requires every field and variant of a registered kind's shape to appear
//! in at least one committed sample, so renaming an optional field or a rarely used variant
//! breaks a frozen fixture instead of passing silently (ver §4). Tracing replaces a hand-kept
//! list, which could leave out exactly the field nobody sampled.
//!
//! [`trace`] feeds the type a made-up input through its own `Deserializer`: a struct receives
//! every field, an `Option` a value, a sequence one element, a map one entry, and an enum one
//! variant. Every field and variant found becomes an item, and each later run aims at the first
//! item not yet visited, entering the field or variant that leads to it first, so a sibling that
//! ends the run cannot hide it.
//!
//! **Leaves.** A string, number or other leaf is offered sample values, one per run, until it
//! accepts one: a string the empty string, then the text of each validated value type of
//! `mayfly_domain` that events carry (pool and host names, the hex ids, image digests, VM
//! names: their [`mayfly_domain::Sample`]); an integer its type's maximum, then 0 and 1. So an
//! id-keyed map or a tuple that starts with an id is traced past its key or first element. A
//! leaf that refuses every sample, or an integer that refuses its maximum, is an *opaque leaf*
//! ([`Gap::OpaqueLeaf`]): most likely an enum coded as a string or an integer (`try_from`,
//! `into`, `with`, `deserialize_with`), whose variants no sample would then be required to show.
//!
//! **Recursion.** A container is known by its kind, serde name, field or variant names and the
//! Rust type of its visitor, so `Change<Limits>` and `Change<Mode>`, or two types both named
//! `Detail`, are different containers even though serde names them alike. One that appears again
//! inside itself is unrolled once more, every node of that copy must match the original at the
//! same relative path, and the next appearance ends the run. A copy that differs is reported
//! ([`Gap::Recursion`]), never taken for recursion; the field and variant slices are compared by
//! content, not address, because a promoted constant can be duplicated across codegen units.
//!
//! **Gaps.** Whatever cannot be traced is reported in [`Shape::untraceable`] with its [`Gap`],
//! never guessed and never dropped: a type that asks for a self-describing format, a part
//! nothing reaches, an opaque leaf, two different shapes at one path, a recursive copy that
//! differs, nesting past [`MAX_DEPTH`], or a trace that does not settle.
//!
//! **Limitations** (ADR 0018): an enum coded as an integer with a variant at the type's
//! maximum, or as a string with a variant spelled like a sample, still traces as a plain leaf,
//! and so does a catch-all coded enum (one whose conversion accepts any string or integer, an
//! `Other(String)` fallback): it accepts the first sample, so it is not even an opaque leaf. A
//! `#[serde(alias)]` name is one of a struct's fields or an enum's variants like any other, so a
//! sample must show it, and a sample that does cannot re-encode byte for byte. The tracer does
//! not catch these; the architecture rule `ARCH-event-payload-serde` keeps them out of the
//! crate that declares events (no `try_from`, `from`, `into`, `with`, `alias`, `other`, tagged
//! or untagged enums or `flatten`, no hand-written serde impls), and `ARCH-sample-coverage`
//! keeps every validated value type of `mayfly_domain` on the list below.
//!
//! Each behaviour is pinned by a unit test at the end of this file.

use std::any::type_name;
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use mayfly_domain::{
    AttemptId, CommandId, DirectiveId, EventId, HostId, ImageDigest, ObservationId, OwnedVmName,
    PoolName, ProviderInstanceId, Sample, SecretHandleId,
};
use serde::de::value::StrDeserializer;
use serde::de::{
    self, DeserializeOwned, DeserializeSeed, Deserializer, EnumAccess, IntoDeserializer, MapAccess,
    SeqAccess, VariantAccess, Visitor,
};

/// A bound on tracing runs: one run per item and per refused sample value is ample.
const MAX_RUNS: usize = 4096;

/// How many containers deep the tracer follows before reporting [`Gap::TooDeep`].
pub const MAX_DEPTH: usize = 128;

/// What sits at one path of a payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Node {
    /// A struct (or struct variant) with these serialized field names.
    Struct(Vec<&'static str>),
    /// An enum with these serialized variant names.
    Enum(Vec<&'static str>),
    /// A sequence or tuple; its elements sit at `<path>[]`.
    Seq,
    /// A map; its keys sit at `<path>{key}`, its values at `<path>{}`.
    Map,
}

/// One thing a payload can contain that some sample must show ([`Shape::atoms`]).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Atom {
    /// The key at this path is present.
    Field(String),
    /// The enum at the path holds the named variant.
    Variant(String, String),
}

impl fmt::Display for Atom {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Field(path) => write!(f, "field `{path}`"),
            Self::Variant(path, variant) => {
                write!(f, "variant `{}`", variant_path(path, variant))
            }
        }
    }
}

/// Why part of a payload cannot be traced, so its coverage cannot be checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Gap {
    /// The type asks for a self-describing format (`deserialize_any`): serde's internally
    /// tagged, adjacently tagged and untagged enums, `flatten`.
    SelfDescribing,
    /// Nothing reaches it: a tuple element or map key before it refuses every sample, or a
    /// value ends every run first.
    Unreached,
    /// A leaf that refuses every sample value, or an integer that refuses its type's maximum.
    OpaqueLeaf,
    /// Two different nodes at one path (a tuple's elements share `[]`).
    Conflict,
    /// A container that appears inside itself, whose unrolled copy differs from the original.
    Recursion,
    /// Containers nested more than [`MAX_DEPTH`] deep.
    TooDeep,
    /// The trace did not settle within its run budget (reported at the root).
    NotConverged,
}

impl fmt::Display for Gap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::SelfDescribing => {
                "needs a self-describing format (an internally, adjacently or untagged enum, \
                 or flatten)"
            }
            Self::Unreached => {
                "is never reached (an element or key before it refuses every sample value, or \
                 its value ends every run)"
            }
            Self::OpaqueLeaf => {
                "is an opaque leaf (an enum coded as a string or integer? use an externally \
                 tagged enum; a new validated value type implements mayfly_domain::Sample and \
                 joins the tracer's list)"
            }
            Self::Conflict => "holds two different shapes (a tuple's elements share one path)",
            Self::Recursion => "repeats a container whose unrolled copy differs from it",
            Self::TooDeep => "is nested deeper than the tracer follows",
            Self::NotConverged => "did not settle within the tracer's run budget",
        })
    }
}

/// The traced shape of one payload type.
///
/// Paths: a field is `parent.field` (`field` at the root), a variant's content
/// `parent::variant`, a sequence element `parent[]`, a map key `parent{key}` and a map value
/// `parent{}`. `Option` and newtypes are transparent, as in serde's JSON form.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Shape {
    /// The node at each path the trace reached.
    pub nodes: BTreeMap<String, Node>,
    /// The paths that could not be traced, each with the first reason found (`""` is the root).
    pub untraceable: BTreeMap<String, Gap>,
}

impl Shape {
    /// Every field and variant the payload can contain.
    #[must_use]
    pub fn atoms(&self) -> BTreeSet<Atom> {
        let mut atoms = BTreeSet::new();
        for (path, node) in &self.nodes {
            match node {
                Node::Struct(fields) => {
                    atoms.extend(fields.iter().map(|f| Atom::Field(field_path(path, f))));
                }
                Node::Enum(variants) => atoms.extend(
                    variants
                        .iter()
                        .map(|v| Atom::Variant(path.clone(), (*v).to_owned())),
                ),
                Node::Seq | Node::Map => {}
            }
        }
        atoms
    }
}

/// `parent.field`, or `field` at the root.
#[must_use]
pub fn field_path(parent: &str, field: &str) -> String {
    if parent.is_empty() {
        field.to_owned()
    } else {
        format!("{parent}.{field}")
    }
}

/// `parent::variant`: where a variant's content sits.
#[must_use]
pub fn variant_path(parent: &str, variant: &str) -> String {
    format!("{parent}::{variant}")
}

/// `parent[]`: where a sequence's elements sit.
#[must_use]
pub fn element_path(parent: &str) -> String {
    format!("{parent}[]")
}

/// `parent{key}`: where a map's keys sit.
#[must_use]
pub fn key_path(parent: &str) -> String {
    format!("{parent}{{key}}")
}

/// `parent{}`: where a map's values sit.
#[must_use]
pub fn value_path(parent: &str) -> String {
    format!("{parent}{{}}")
}

/// The texts a string leaf is offered, in order: the empty string, then one valid text of each
/// validated value type events carry. This list is the tracer's allowlist of value types: a
/// string leaf that refuses all of them is an opaque leaf.
fn texts() -> Vec<String> {
    let mut texts = vec![String::new()];
    for text in [
        String::from(PoolName::sample()),
        String::from(HostId::sample()),
        String::from(AttemptId::sample()),
        String::from(CommandId::sample()),
        String::from(DirectiveId::sample()),
        String::from(EventId::sample()),
        String::from(ObservationId::sample()),
        String::from(ImageDigest::sample()),
        String::from(OwnedVmName::sample()),
        String::from(ProviderInstanceId::sample()),
        String::from(SecretHandleId::sample()),
    ] {
        if !texts.contains(&text) {
            texts.push(text);
        }
    }
    texts
}

/// Traces `T`'s `Deserialize` impl into its [`Shape`].
#[must_use]
pub fn trace<T: DeserializeOwned>() -> Shape {
    let state = RefCell::new(State::new());
    let mut aim = None;
    let mut converged = false;
    for _ in 0..MAX_RUNS {
        state.borrow_mut().begin(aim.as_ref());
        // An error only ends this run: what it recorded on the way stays recorded.
        let result = T::deserialize(Tracer {
            state: &state,
            path: String::new(),
        });
        let mut s = state.borrow_mut();
        if result.is_err() {
            s.failed("");
        }
        s.finish(aim.as_ref());
        aim = s.next_aim();
        if aim.is_none() {
            converged = true;
            break;
        }
    }
    let mut state = state.into_inner();
    state.check_reached();
    state.check_echoes();
    if !converged {
        state.gap("", Gap::NotConverged);
    }
    state.shape
}

/// Whether the path `target` runs through `path`.
fn leads(target: &str, path: &str) -> bool {
    target
        .strip_prefix(path)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with(['.', ':', '[', '{']))
}

/// `path`, inside the unrolled copy rooted at `root`, moved to the same place under `original`.
fn relocate(path: &str, root: &str, original: &str) -> String {
    let rest = path.strip_prefix(root).unwrap_or(path);
    if original.is_empty() {
        rest.strip_prefix('.').unwrap_or(rest).to_owned()
    } else {
        format!("{original}{rest}")
    }
}

/// What a container is, beyond where it sits: two containers with equal identities are taken
/// for the same Rust type.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Identity {
    kind: &'static str,
    name: &'static str,
    /// The field or variant names, compared by content: a container that differs from another
    /// only in them is another type (pinned by
    /// `should_trace_both_when_a_nested_container_differs_only_in_its_field_names`).
    names: &'static [&'static str],
    len: usize,
    /// The visitor's Rust type, which carries a generic type's parameters.
    visitor: &'static str,
}

impl Identity {
    fn of<V>(
        kind: &'static str,
        name: &'static str,
        names: &'static [&'static str],
        len: usize,
    ) -> Self {
        Self {
            kind,
            name,
            names,
            len,
            visitor: type_name::<V>(),
        }
    }
}

/// What sits at one path, as the recursion check compares it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Sig {
    Container(Identity),
    /// A sequence, map or leaf, by family (`seq`, `map`, `text`, `integer`, …).
    Plain(&'static str),
}

/// One named container on the current path.
struct Frame {
    identity: Identity,
    path: String,
    /// For the unrolled copy of a container already on the path: the original's path.
    original: Option<String>,
}

/// What a run aims at.
#[derive(Clone)]
enum Aim {
    /// A field or variant not yet visited.
    Item(String),
    /// A leaf that refused a sample and has another to try.
    Retry(String),
}

/// The trace's memory across runs.
struct State {
    shape: Shape,
    /// Fields (`parent.field`) and variants (`parent::variant`), in the order found.
    items: Vec<String>,
    found: BTreeSet<String>,
    visited: BTreeSet<String>,
    /// The path this run aims at.
    target: Option<String>,
    stack: Vec<Frame>,
    /// What the trace saw at each path outside unrolled copies.
    sigs: BTreeMap<String, BTreeSet<Sig>>,
    /// What each unrolled copy saw: its root, the original path it must match, and the sig.
    echoes: Vec<(String, String, Sig)>,
    /// The sample each leaf (path and family) is offered next.
    probes: BTreeMap<(String, &'static str), usize>,
    /// Leaves that refused a sample and have another to try.
    retries: BTreeSet<String>,
    /// Whether this run queued a retry.
    queued: bool,
    /// Whether this run reached the leaf it retries.
    reached: bool,
    /// Outside unrolled copies: each sequence's element path and its longest length, the
    /// element indices some run reached, the value paths of maps, and those some run reached.
    /// An element or value no run reaches is reported when the trace ends, not when one run
    /// misses it: a retried sample or another variant may reach it in a later run.
    lens: BTreeMap<String, usize>,
    elements: BTreeSet<(String, usize)>,
    maps: BTreeSet<String>,
    values: BTreeSet<String>,
    /// The last leaf that accepted its sample, with its probe: if the value that leaf belongs to
    /// then fails (a `try_from` conversion runs after the leaf's visitor), the leaf refused.
    last_leaf: Option<Probe>,
    texts: Vec<String>,
}

/// One leaf's answer to one sample.
struct Probe {
    path: String,
    family: &'static str,
    index: usize,
    count: usize,
    /// Refusing the first sample (an integer's maximum) already makes the leaf opaque.
    strict: bool,
}

impl State {
    fn new() -> Self {
        Self {
            shape: Shape::default(),
            items: Vec::new(),
            found: BTreeSet::new(),
            visited: BTreeSet::new(),
            target: None,
            stack: Vec::new(),
            sigs: BTreeMap::new(),
            echoes: Vec::new(),
            probes: BTreeMap::new(),
            retries: BTreeSet::new(),
            queued: false,
            reached: false,
            lens: BTreeMap::new(),
            elements: BTreeSet::new(),
            maps: BTreeSet::new(),
            values: BTreeSet::new(),
            last_leaf: None,
            texts: texts(),
        }
    }

    fn begin(&mut self, aim: Option<&Aim>) {
        self.stack.clear();
        self.target = aim.map(|aim| match aim {
            Aim::Item(path) | Aim::Retry(path) => path.clone(),
        });
        self.queued = false;
        self.reached = false;
        self.last_leaf = None;
    }

    /// After a run: an item it aimed at but did not reach is unreachable, unless a leaf on the
    /// way has another sample to try first.
    fn finish(&mut self, aim: Option<&Aim>) {
        match aim {
            Some(Aim::Item(item)) if !self.visited.contains(item) && !self.queued => {
                self.gap(item, Gap::Unreached);
                self.visited.insert(item.clone());
            }
            Some(Aim::Retry(path)) if !self.reached => self.gap(path, Gap::Unreached),
            _ => {}
        }
    }

    /// Retries first, so a refused leaf gets its next sample before anything is given up.
    fn next_aim(&mut self) -> Option<Aim> {
        if let Some(path) = self.retries.pop_first() {
            return Some(Aim::Retry(path));
        }
        self.items
            .iter()
            .find(|item| !self.visited.contains(*item))
            .cloned()
            .map(Aim::Item)
    }

    fn gap(&mut self, path: &str, gap: Gap) {
        self.shape.untraceable.entry(path.to_owned()).or_insert(gap);
    }

    fn find(&mut self, item: String) {
        if self.found.insert(item.clone()) {
            self.items.push(item);
        }
    }

    /// The outermost unrolled copy on the current path: its root and the original's path.
    fn echo(&self) -> Option<(String, String)> {
        self.stack.iter().find_map(|frame| {
            frame
                .original
                .as_ref()
                .map(|original| (frame.path.clone(), original.clone()))
        })
    }

    fn note(&mut self, path: &str, sig: Sig) {
        match self.echo() {
            Some((root, original)) => {
                let at = relocate(path, &root, &original);
                self.echoes.push((root, at, sig));
            }
            None => {
                self.sigs.entry(path.to_owned()).or_default().insert(sig);
            }
        }
    }

    /// A sequence element or map value that no run reached hides its shape.
    fn check_reached(&mut self) {
        let mut unreached = Vec::new();
        for (path, len) in &self.lens {
            if (0..*len).any(|i| !self.elements.contains(&(path.clone(), i))) {
                unreached.push(path.clone());
            }
        }
        unreached.extend(self.maps.difference(&self.values).cloned());
        for path in unreached {
            self.gap(&path, Gap::Unreached);
        }
    }

    /// Every node of an unrolled copy must match the original at the same relative path.
    fn check_echoes(&mut self) {
        let echoes = std::mem::take(&mut self.echoes);
        for (root, at, sig) in echoes {
            if !self.sigs.get(&at).is_some_and(|sigs| sigs.contains(&sig)) {
                self.gap(&root, Gap::Recursion);
            }
        }
    }

    /// Finds the node's fields or variants as items and, outside an unrolled copy, records the
    /// node; a different node already at the path is a conflict, not overwritten or dropped.
    fn record(&mut self, path: &str, node: Node) {
        match &node {
            Node::Struct(fields) => {
                for field in fields {
                    self.find(field_path(path, field));
                }
            }
            Node::Enum(variants) => {
                for variant in variants {
                    self.find(variant_path(path, variant));
                }
            }
            Node::Seq | Node::Map => {}
        }
        if self.echo().is_some() {
            return;
        }
        let conflict = match self.shape.nodes.get(path) {
            Some(existing) => *existing != node,
            None => {
                self.shape.nodes.insert(path.to_owned(), node);
                false
            }
        };
        if conflict {
            self.gap(path, Gap::Conflict);
        }
    }

    fn toward(&self, path: &str) -> bool {
        self.target.as_deref().is_some_and(|t| leads(t, path))
    }

    /// Enters a named container: its first appearance on the path is traced, its second is the
    /// unrolled copy checked against the first, its third ends the run.
    fn enter(&mut self, identity: Identity, path: &str) -> Result<(), TraceError> {
        if self.stack.len() >= MAX_DEPTH {
            self.gap(path, Gap::TooDeep);
            return Err(TraceError(format!("{path}: nested too deep")));
        }
        let mut earlier = self.stack.iter().filter(|f| f.identity == identity);
        let original = match (earlier.next(), earlier.next()) {
            (None, _) => None,
            (Some(first), None) => Some(first.path.clone()),
            (Some(_), Some(_)) => {
                return Err(TraceError(format!("{path}: recursion unrolled once")));
            }
        };
        self.stack.push(Frame {
            identity: identity.clone(),
            path: path.to_owned(),
            original,
        });
        self.note(path, Sig::Container(identity));
        Ok(())
    }

    fn leave(&mut self) {
        self.stack.pop();
    }

    /// The variant leading to the target, else one not yet visited, else the first.
    fn choose(&mut self, path: &str, variants: &'static [&'static str]) -> Option<&'static str> {
        let paths = || variants.iter().map(|v| (*v, variant_path(path, v)));
        let pick = paths()
            .find(|(_, p)| self.toward(p))
            .or_else(|| paths().find(|(_, p)| !self.visited.contains(p)))
            .or_else(|| paths().next())?;
        self.visited.insert(pick.1);
        Some(pick.0)
    }

    fn probe(&self, path: &str, family: &'static str) -> usize {
        self.probes
            .get(&(path.to_owned(), family))
            .copied()
            .unwrap_or(0)
    }

    /// Records a leaf's answer to its sample.
    fn probed(&mut self, probe: Probe, accepted: bool) {
        self.note(&probe.path, Sig::Plain(probe.family));
        if self.target.as_deref() == Some(probe.path.as_str()) {
            self.reached = true;
        }
        if accepted {
            self.last_leaf = Some(probe);
        } else {
            self.refused(&probe);
        }
    }

    /// The value at `path` failed: if the last leaf accepted sits at `path` itself, its value
    /// was refused after the leaf's visitor took it (`try_from`, `deserialize_with`).
    fn failed(&mut self, path: &str) {
        if let Some(probe) = self.last_leaf.take()
            && probe.path == path
        {
            self.refused(&probe);
        }
    }

    /// A leaf refused its sample: queue the next one, or report the leaf opaque.
    fn refused(&mut self, probe: &Probe) {
        if probe.strict && probe.index == 0 {
            self.gap(&probe.path, Gap::OpaqueLeaf);
        }
        if probe.index + 1 < probe.count {
            self.probes
                .insert((probe.path.clone(), probe.family), probe.index + 1);
            self.retries.insert(probe.path.clone());
            self.queued = true;
        } else {
            self.gap(&probe.path, Gap::OpaqueLeaf);
        }
    }
}

#[derive(Debug)]
struct TraceError(String);

impl fmt::Display for TraceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for TraceError {}

impl de::Error for TraceError {
    fn custom<M: fmt::Display>(msg: M) -> Self {
        Self(msg.to_string())
    }
}

fn traced<'de, T: DeserializeSeed<'de>>(
    state: &RefCell<State>,
    path: String,
    seed: T,
) -> Result<T::Value, TraceError> {
    state.borrow_mut().last_leaf = None;
    let result = seed.deserialize(Tracer {
        state,
        path: path.clone(),
    });
    if result.is_err() {
        state.borrow_mut().failed(&path);
    }
    result
}

struct Tracer<'s> {
    state: &'s RefCell<State>,
    path: String,
}

impl Tracer<'_> {
    fn record(&self, node: Node) {
        self.state.borrow_mut().record(&self.path, node);
    }

    /// Offers a leaf the sample its probe has reached, of `count` in its family.
    fn leaf<T>(
        &self,
        family: &'static str,
        count: usize,
        strict: bool,
        visit: impl FnOnce(usize) -> Result<T, TraceError>,
    ) -> Result<T, TraceError> {
        let index = self.state.borrow().probe(&self.path, family);
        let result = visit(index);
        let probe = Probe {
            path: self.path.clone(),
            family,
            index,
            count,
            strict,
        };
        self.state.borrow_mut().probed(probe, result.is_ok());
        result
    }

    fn text<'de, V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, TraceError> {
        let texts = self.state.borrow().texts.clone();
        self.leaf("text", texts.len(), false, |i| visitor.visit_str(&texts[i]))
    }

    /// Enters a named container for the length of `body`.
    fn within<T>(
        self,
        identity: Identity,
        body: impl FnOnce(Self) -> Result<T, TraceError>,
    ) -> Result<T, TraceError> {
        let state = self.state;
        state.borrow_mut().enter(identity, &self.path)?;
        let result = body(self);
        state.borrow_mut().leave();
        result
    }
}

macro_rules! integers {
    ($($method:ident => $visit:ident: $ty:ty),+ $(,)?) => {
        $(
            fn $method<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, TraceError> {
                let samples: [$ty; 3] = [<$ty>::MAX, 0, 1];
                self.leaf("integer", samples.len(), true, |i| visitor.$visit(samples[i]))
            }
        )+
    };
}

impl<'de> Deserializer<'de> for Tracer<'_> {
    type Error = TraceError;

    fn deserialize_any<V: Visitor<'de>>(self, _visitor: V) -> Result<V::Value, TraceError> {
        self.state.borrow_mut().gap(&self.path, Gap::SelfDescribing);
        Err(TraceError(format!(
            "{}: the type needs a self-describing format",
            self.path
        )))
    }

    integers! {
        deserialize_i8 => visit_i8: i8,
        deserialize_i16 => visit_i16: i16,
        deserialize_i32 => visit_i32: i32,
        deserialize_i64 => visit_i64: i64,
        deserialize_i128 => visit_i128: i128,
        deserialize_u8 => visit_u8: u8,
        deserialize_u16 => visit_u16: u16,
        deserialize_u32 => visit_u32: u32,
        deserialize_u64 => visit_u64: u64,
        deserialize_u128 => visit_u128: u128,
    }

    fn deserialize_bool<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, TraceError> {
        self.leaf("bool", 2, false, |i| visitor.visit_bool(i == 1))
    }

    fn deserialize_f32<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, TraceError> {
        self.leaf("float", 1, false, |_| visitor.visit_f32(0.0))
    }

    fn deserialize_f64<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, TraceError> {
        self.leaf("float", 1, false, |_| visitor.visit_f64(0.0))
    }

    fn deserialize_char<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, TraceError> {
        self.leaf("char", 1, false, |_| visitor.visit_char('a'))
    }

    fn deserialize_str<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, TraceError> {
        self.text(visitor)
    }

    fn deserialize_string<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, TraceError> {
        self.text(visitor)
    }

    fn deserialize_identifier<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, TraceError> {
        self.text(visitor)
    }

    fn deserialize_bytes<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, TraceError> {
        self.leaf("bytes", 1, false, |_| visitor.visit_bytes(&[]))
    }

    fn deserialize_byte_buf<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, TraceError> {
        self.leaf("bytes", 1, false, |_| visitor.visit_bytes(&[]))
    }

    fn deserialize_unit<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, TraceError> {
        self.leaf("unit", 1, false, |_| visitor.visit_unit())
    }

    fn deserialize_ignored_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, TraceError> {
        visitor.visit_unit()
    }

    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, TraceError> {
        visitor.visit_some(self)
    }

    fn deserialize_unit_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, TraceError> {
        self.leaf("unit", 1, false, |_| visitor.visit_unit())
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        name: &'static str,
        visitor: V,
    ) -> Result<V::Value, TraceError> {
        let identity = Identity::of::<V>("newtype", name, &[], 1);
        self.within(identity, |tracer| visitor.visit_newtype_struct(tracer))
    }

    fn deserialize_seq<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, TraceError> {
        self.deserialize_tuple(1, visitor)
    }

    fn deserialize_tuple<V: Visitor<'de>>(
        self,
        len: usize,
        visitor: V,
    ) -> Result<V::Value, TraceError> {
        self.record(Node::Seq);
        let path = element_path(&self.path);
        let echo = {
            let mut s = self.state.borrow_mut();
            s.note(&self.path, Sig::Plain("seq"));
            let echo = s.echo().is_some();
            if !echo {
                let longest = s.lens.entry(path.clone()).or_default();
                *longest = (*longest).max(len);
            }
            echo
        };
        visitor.visit_seq(Elements {
            state: self.state,
            path,
            len,
            left: len,
            echo,
        })
    }

    fn deserialize_tuple_struct<V: Visitor<'de>>(
        self,
        name: &'static str,
        len: usize,
        visitor: V,
    ) -> Result<V::Value, TraceError> {
        let identity = Identity::of::<V>("tuple_struct", name, &[], len);
        self.within(identity, |tracer| tracer.deserialize_tuple(len, visitor))
    }

    fn deserialize_map<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, TraceError> {
        self.record(Node::Map);
        let value = value_path(&self.path);
        let echo = {
            let mut s = self.state.borrow_mut();
            s.note(&self.path, Sig::Plain("map"));
            let echo = s.echo().is_some();
            if !echo {
                s.maps.insert(value.clone());
            }
            echo
        };
        visitor.visit_map(Entry {
            state: self.state,
            key: key_path(&self.path),
            value,
            left: true,
            echo,
        })
    }

    fn deserialize_struct<V: Visitor<'de>>(
        self,
        name: &'static str,
        fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, TraceError> {
        let identity = Identity::of::<V>("struct", name, fields, fields.len());
        self.within(identity, |tracer| {
            tracer.record(Node::Struct(fields.to_vec()));
            let state = tracer.state;
            let (mut ordered, rest): (Vec<_>, Vec<_>) = {
                let s = state.borrow();
                fields
                    .iter()
                    .map(|f| (*f, field_path(&tracer.path, f)))
                    .partition(|(_, path)| s.toward(path))
            };
            ordered.extend(rest);
            visitor.visit_map(Fields {
                state,
                fields: ordered.into_iter(),
                pending: None,
            })
        })
    }

    fn deserialize_enum<V: Visitor<'de>>(
        self,
        name: &'static str,
        variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, TraceError> {
        let identity = Identity::of::<V>("enum", name, variants, variants.len());
        self.within(identity, |tracer| {
            tracer.record(Node::Enum(variants.to_vec()));
            let state = tracer.state;
            let pick = state.borrow_mut().choose(&tracer.path, variants);
            match pick {
                Some(variant) => visitor.visit_enum(Variant {
                    state,
                    path: variant_path(&tracer.path, variant),
                    name: variant,
                }),
                None => Err(TraceError(format!(
                    "{}: the enum has no variant",
                    tracer.path
                ))),
            }
        })
    }
}

struct Elements<'s> {
    state: &'s RefCell<State>,
    path: String,
    len: usize,
    left: usize,
    /// Inside an unrolled copy, whose original keeps the books.
    echo: bool,
}

impl<'de> SeqAccess<'de> for Elements<'_> {
    type Error = TraceError;

    fn next_element_seed<T: DeserializeSeed<'de>>(
        &mut self,
        seed: T,
    ) -> Result<Option<T::Value>, TraceError> {
        if self.left == 0 {
            return Ok(None);
        }
        if !self.echo {
            self.state
                .borrow_mut()
                .elements
                .insert((self.path.clone(), self.len - self.left));
        }
        self.left -= 1;
        traced(self.state, self.path.clone(), seed).map(Some)
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.left)
    }
}

struct Entry<'s> {
    state: &'s RefCell<State>,
    key: String,
    value: String,
    left: bool,
    /// Inside an unrolled copy, whose original keeps the books.
    echo: bool,
}

impl<'de> MapAccess<'de> for Entry<'_> {
    type Error = TraceError;

    fn next_key_seed<K: DeserializeSeed<'de>>(
        &mut self,
        seed: K,
    ) -> Result<Option<K::Value>, TraceError> {
        if !self.left {
            return Ok(None);
        }
        self.left = false;
        traced(self.state, self.key.clone(), seed).map(Some)
    }

    fn next_value_seed<T: DeserializeSeed<'de>>(
        &mut self,
        seed: T,
    ) -> Result<T::Value, TraceError> {
        if !self.echo {
            self.state.borrow_mut().values.insert(self.value.clone());
        }
        traced(self.state, self.value.clone(), seed)
    }
}

struct Fields<'s> {
    state: &'s RefCell<State>,
    fields: std::vec::IntoIter<(&'static str, String)>,
    pending: Option<String>,
}

impl<'de> MapAccess<'de> for Fields<'_> {
    type Error = TraceError;

    fn next_key_seed<K: DeserializeSeed<'de>>(
        &mut self,
        seed: K,
    ) -> Result<Option<K::Value>, TraceError> {
        let Some((name, path)) = self.fields.next() else {
            return Ok(None);
        };
        self.pending = Some(path);
        let key: StrDeserializer<'_, TraceError> = name.into_deserializer();
        seed.deserialize(key).map(Some)
    }

    fn next_value_seed<T: DeserializeSeed<'de>>(
        &mut self,
        seed: T,
    ) -> Result<T::Value, TraceError> {
        let path = self.pending.take().unwrap_or_default();
        self.state.borrow_mut().visited.insert(path.clone());
        traced(self.state, path, seed)
    }
}

struct Variant<'s> {
    state: &'s RefCell<State>,
    path: String,
    name: &'static str,
}

impl<'de> EnumAccess<'de> for Variant<'_> {
    type Error = TraceError;
    type Variant = Self;

    fn variant_seed<T: DeserializeSeed<'de>>(
        self,
        seed: T,
    ) -> Result<(T::Value, Self), TraceError> {
        let name: StrDeserializer<'_, TraceError> = self.name.into_deserializer();
        let value = seed.deserialize(name)?;
        Ok((value, self))
    }
}

impl<'de> VariantAccess<'de> for Variant<'_> {
    type Error = TraceError;

    fn unit_variant(self) -> Result<(), TraceError> {
        Ok(())
    }

    fn newtype_variant_seed<T: DeserializeSeed<'de>>(
        self,
        seed: T,
    ) -> Result<T::Value, TraceError> {
        traced(self.state, self.path, seed)
    }

    fn tuple_variant<V: Visitor<'de>>(
        self,
        len: usize,
        visitor: V,
    ) -> Result<V::Value, TraceError> {
        Tracer {
            state: self.state,
            path: self.path,
        }
        .deserialize_tuple(len, visitor)
    }

    fn struct_variant<V: Visitor<'de>>(
        self,
        fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, TraceError> {
        Tracer {
            state: self.state,
            path: self.path,
        }
        .deserialize_struct("", fields, visitor)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use mayfly_domain::{
        AttemptId, CommandId, Deadline, DirectiveId, Epoch, EventId, Generation, HostId,
        ImageDigest, Millis, ObservationId, OwnedVmName, PoolName, Resources, Slot,
    };
    use serde::Deserialize;
    use serde::de::{DeserializeSeed, Deserializer, MapAccess, Visitor};

    use super::{Atom, Gap, MAX_DEPTH, Node, Shape, trace};

    fn field(path: &str) -> Atom {
        Atom::Field(path.to_owned())
    }

    fn variant(path: &str, name: &str) -> Atom {
        Atom::Variant(path.to_owned(), name.to_owned())
    }

    fn atoms(shape: &Shape) -> Vec<Atom> {
        shape.atoms().into_iter().collect()
    }

    fn gaps(shape: Shape) -> Vec<(String, Gap)> {
        shape.untraceable.into_iter().collect()
    }

    fn gap(path: &str, gap: Gap) -> (String, Gap) {
        (path.to_owned(), gap)
    }

    // `PartialEq` is derived so the fields count as read; only `Deserialize` is traced.
    #[derive(Deserialize, PartialEq)]
    struct Plain {
        count: u32,
        #[serde(default, skip_serializing_if = "Option::is_none", rename = "remark")]
        note: Option<String>,
    }

    #[test]
    fn should_list_every_field_when_struct_has_optional_ones() {
        // given
        let traced = trace::<Plain>;
        // when
        let shape = traced();
        // then
        assert_eq!(shape.nodes[""], Node::Struct(vec!["count", "remark"]));
        assert_eq!(atoms(&shape), [field("count"), field("remark")]);
        assert!(shape.untraceable.is_empty());
    }

    #[derive(Deserialize, PartialEq)]
    #[serde(rename_all = "snake_case")]
    enum Reason {
        Wind,
        Hand { by: u32 },
        Code(u16),
        Pair(u8, Option<Nested>),
    }

    #[derive(Deserialize, PartialEq)]
    struct Nested {
        depth: u8,
    }

    #[derive(Deserialize, PartialEq)]
    struct Closed {
        reason: Option<Reason>,
    }

    #[test]
    fn should_reach_every_variant_when_enum_is_nested() {
        // given
        let traced = trace::<Closed>;
        // when
        let shape = traced();
        // then
        assert_eq!(
            atoms(&shape),
            [
                field("reason"),
                field("reason::hand.by"),
                field("reason::pair[].depth"),
                variant("reason", "code"),
                variant("reason", "hand"),
                variant("reason", "pair"),
                variant("reason", "wind"),
            ]
        );
        assert!(shape.untraceable.is_empty());
    }

    #[derive(Deserialize, PartialEq)]
    struct Collections {
        items: Vec<Nested>,
        labels: BTreeMap<String, Nested>,
    }

    #[test]
    fn should_trace_elements_and_values_when_payload_has_collections() {
        // given
        let traced = trace::<Collections>;
        // when
        let shape = traced();
        // then
        assert_eq!(shape.nodes["items"], Node::Seq);
        assert_eq!(shape.nodes["labels"], Node::Map);
        assert_eq!(
            atoms(&shape),
            [
                field("items"),
                field("items[].depth"),
                field("labels"),
                field("labels{}.depth"),
            ]
        );
    }

    /// A string that refuses to be empty, as validated ids do.
    #[derive(Deserialize, PartialEq, Eq, PartialOrd, Ord)]
    #[serde(try_from = "String")]
    struct Name(String);

    impl TryFrom<String> for Name {
        type Error = &'static str;
        fn try_from(value: String) -> Result<Self, Self::Error> {
            if value.is_empty() {
                Err("empty name")
            } else {
                Ok(Self(value))
            }
        }
    }

    #[derive(Deserialize, PartialEq)]
    struct Validated {
        name: Name,
        after: Option<Reason>,
    }

    #[test]
    fn should_trace_past_a_value_when_it_refuses_the_first_sample() {
        // given
        let traced = trace::<Validated>;
        // when
        let shape = traced();
        // then
        assert!(atoms(&shape).contains(&field("name")));
        assert!(atoms(&shape).contains(&variant("after", "pair")));
        assert!(atoms(&shape).contains(&field("after::pair[].depth")));
        assert!(shape.untraceable.is_empty());
    }

    #[derive(Deserialize, PartialEq)]
    #[serde(rename_all = "snake_case")]
    enum Detail {
        Boot,
        Halt { code: u8 },
    }

    #[derive(Deserialize, PartialEq)]
    #[serde(rename_all = "snake_case")]
    enum Pin {
        Pinned(Generation, Detail),
    }

    #[derive(Deserialize, PartialEq)]
    struct Keyed {
        hosts: BTreeMap<HostId, Nested>,
        vms: BTreeMap<OwnedVmName, Nested>,
        pin: Pin,
        first: (AttemptId, Nested),
    }

    #[test]
    fn should_trace_past_validated_keys_and_leading_elements_when_a_sample_value_fits() {
        // given
        let traced = trace::<Keyed>;
        // when
        let shape = traced();
        // then
        assert!(shape.untraceable.is_empty(), "{:?}", shape.untraceable);
        assert_eq!(
            atoms(&shape),
            [
                field("first"),
                field("first[].depth"),
                field("hosts"),
                field("hosts{}.depth"),
                field("pin"),
                field("pin::pinned[]::halt.code"),
                field("vms"),
                field("vms{}.depth"),
                variant("pin", "pinned"),
                variant("pin::pinned[]", "boot"),
                variant("pin::pinned[]", "halt"),
            ]
        );
    }

    #[derive(Deserialize, PartialEq)]
    struct ValueTypes {
        pool: PoolName,
        host: HostId,
        attempt: AttemptId,
        command: CommandId,
        directive: DirectiveId,
        event: EventId,
        observation: ObservationId,
        digest: ImageDigest,
        vm: OwnedVmName,
        generation: Generation,
        epoch: Epoch,
        at: Millis,
        deadline: Deadline,
        slot: Slot,
        resources: Resources,
    }

    #[test]
    fn should_trace_value_types_as_leaves_when_they_are_on_the_list() {
        // given
        let traced = trace::<ValueTypes>;
        // when
        let shape = traced();
        // then
        assert!(shape.untraceable.is_empty(), "{:?}", shape.untraceable);
        assert!(atoms(&shape).contains(&field("resources.disk_bytes")));
    }

    /// An enum carried as a string: serde sees only the string.
    #[derive(Deserialize, PartialEq)]
    #[serde(try_from = "String")]
    enum Mode {
        Strict,
        Loose,
    }

    impl TryFrom<String> for Mode {
        type Error = &'static str;
        fn try_from(value: String) -> Result<Self, Self::Error> {
            match value.as_str() {
                "strict" => Ok(Self::Strict),
                "loose" => Ok(Self::Loose),
                _ => Err("unknown mode"),
            }
        }
    }

    /// An enum carried as an integer.
    #[derive(Deserialize, PartialEq)]
    #[serde(try_from = "u8")]
    enum Level {
        Low,
        High,
    }

    impl TryFrom<u8> for Level {
        type Error = &'static str;
        fn try_from(value: u8) -> Result<Self, Self::Error> {
            match value {
                0 => Ok(Self::Low),
                1 => Ok(Self::High),
                _ => Err("unknown level"),
            }
        }
    }

    #[derive(Deserialize, PartialEq)]
    struct Coded {
        mode: Mode,
        level: Level,
        after: Option<Reason>,
    }

    #[test]
    fn should_report_opaque_leaves_when_enums_are_coded_as_strings_or_integers() {
        // given
        let traced = trace::<Coded>;
        // when
        let shape = traced();
        // then
        assert!(atoms(&shape).contains(&variant("after", "wind")));
        assert_eq!(
            gaps(shape),
            [gap("level", Gap::OpaqueLeaf), gap("mode", Gap::OpaqueLeaf)]
        );
    }

    /// A string that refuses every value.
    #[derive(Deserialize, PartialEq, Eq, PartialOrd, Ord)]
    #[serde(try_from = "String")]
    struct Stubborn(String);

    impl TryFrom<String> for Stubborn {
        type Error = &'static str;
        fn try_from(_value: String) -> Result<Self, Self::Error> {
            Err("never")
        }
    }

    #[derive(Deserialize, PartialEq)]
    struct Hidden {
        pair: (Stubborn, Nested),
        index: BTreeMap<Stubborn, Nested>,
    }

    #[test]
    fn should_report_untraceable_when_a_refusing_element_or_key_hides_the_rest() {
        // given
        let traced = trace::<Hidden>;
        // when
        let shape = traced();
        // then
        assert_eq!(
            gaps(shape),
            [
                gap("index{key}", Gap::OpaqueLeaf),
                gap("index{}", Gap::Unreached),
                gap("pair[]", Gap::OpaqueLeaf),
            ]
        );
    }

    #[derive(Deserialize, PartialEq)]
    struct Tree {
        label: u8,
        children: Vec<Tree>,
    }

    #[test]
    fn should_stop_after_one_matching_unrolling_when_type_is_recursive() {
        // given
        let traced = trace::<Tree>;
        // when
        let shape = traced();
        // then
        assert_eq!(atoms(&shape), [field("children"), field("label")]);
        assert!(shape.untraceable.is_empty());
    }

    #[derive(Deserialize, PartialEq)]
    #[serde(rename_all = "snake_case")]
    enum Expr {
        Lit(u8),
        Add(Box<Expr>, Box<Expr>),
    }

    #[test]
    fn should_stop_after_one_matching_unrolling_when_an_enum_holds_itself() {
        // given
        let traced = trace::<Expr>;
        // when
        let shape = traced();
        // then
        assert_eq!(atoms(&shape), [variant("", "add"), variant("", "lit")]);
        assert!(shape.untraceable.is_empty(), "{:?}", shape.untraceable);
    }

    #[derive(Deserialize, PartialEq)]
    struct Change<T> {
        after: T,
    }

    #[derive(Deserialize, PartialEq)]
    struct Limits {
        mode: Change<Detail>,
    }

    #[derive(Deserialize, PartialEq)]
    struct Policy {
        limits: Change<Limits>,
    }

    #[test]
    fn should_trace_each_instance_when_a_generic_container_nests_itself() {
        // given
        let traced = trace::<Policy>;
        // when
        let shape = traced();
        // then
        assert!(shape.untraceable.is_empty(), "{:?}", shape.untraceable);
        let atoms = atoms(&shape);
        assert!(atoms.contains(&variant("limits.after.mode.after", "boot")));
        assert!(atoms.contains(&field("limits.after.mode.after::halt.code")));
    }

    mod outer {
        #[derive(serde::Deserialize, PartialEq)]
        pub(super) struct Detail {
            pub(super) inner: super::inner::Detail,
        }
    }

    mod inner {
        #[derive(serde::Deserialize, PartialEq)]
        pub(super) struct Detail {
            pub(super) inner: super::Reason,
        }
    }

    #[test]
    fn should_trace_both_when_two_types_share_a_name_and_fields() {
        // given
        let traced = trace::<outer::Detail>;
        // when
        let shape = traced();
        // then
        assert!(shape.untraceable.is_empty(), "{:?}", shape.untraceable);
        assert!(atoms(&shape).contains(&variant("inner.inner", "wind")));
    }

    /// A struct that holds itself once, with the same name, fields and visitor, and then holds
    /// a number: only the unrolled copy shows it is not recursion.
    #[derive(PartialEq)]
    struct Knot;

    /// `inner` is the field list of the nested struct: the same as the outer one's for
    /// [`Knot`], another for [`Twin`].
    struct KnotSeed {
        depth: u8,
        inner: &'static [&'static str],
    }

    struct KnotVisitor {
        depth: u8,
        inner: &'static [&'static str],
    }

    impl<'de> Visitor<'de> for KnotVisitor {
        type Value = Knot;
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("a knot")
        }
        fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Knot, M::Error> {
            while map.next_key::<String>()?.is_some() {
                if self.depth == 0 {
                    map.next_value_seed(KnotSeed {
                        depth: 1,
                        inner: self.inner,
                    })?;
                } else {
                    map.next_value::<u8>()?;
                }
            }
            Ok(Knot)
        }
    }

    impl<'de> DeserializeSeed<'de> for KnotSeed {
        type Value = Knot;
        fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Knot, D::Error> {
            let fields = if self.depth == 0 {
                &["next"]
            } else {
                self.inner
            };
            let visitor = KnotVisitor {
                depth: self.depth,
                inner: self.inner,
            };
            deserializer.deserialize_struct("Knot", fields, visitor)
        }
    }

    impl<'de> Deserialize<'de> for Knot {
        fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            KnotSeed {
                depth: 0,
                inner: &["next"],
            }
            .deserialize(deserializer)
        }
    }

    #[test]
    fn should_report_untraceable_when_a_repeated_container_differs_from_the_original() {
        // given
        let traced = trace::<Knot>;
        // when
        let shape = traced();
        // then
        assert_eq!(gaps(shape), [gap("next", Gap::Recursion)]);
    }

    /// A [`Knot`] whose nested struct has the same name, length and visitor but another field
    /// list: only the field names tell the two apart, so it is not recursion.
    #[derive(PartialEq)]
    struct Twin;

    impl<'de> Deserialize<'de> for Twin {
        fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            let seed = KnotSeed {
                depth: 0,
                inner: &["last"],
            };
            seed.deserialize(deserializer).map(|Knot| Twin)
        }
    }

    #[test]
    fn should_trace_both_when_a_nested_container_differs_only_in_its_field_names() {
        // given
        let traced = trace::<Twin>;
        // when
        let shape = traced();
        // then
        assert!(shape.untraceable.is_empty(), "{:?}", shape.untraceable);
        assert_eq!(atoms(&shape), [field("next"), field("next.last")]);
    }

    /// A struct nested without end, under a new name at every level.
    #[derive(PartialEq)]
    struct Abyss;

    struct AbyssVisitor(usize);

    impl<'de> Visitor<'de> for AbyssVisitor {
        type Value = Abyss;
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("an abyss")
        }
        fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Abyss, M::Error> {
            while map.next_key::<String>()?.is_some() {
                map.next_value_seed(AbyssVisitor(self.0 + 1))?;
            }
            Ok(Abyss)
        }
    }

    impl<'de> DeserializeSeed<'de> for AbyssVisitor {
        type Value = Abyss;
        fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Abyss, D::Error> {
            let name: &'static str = Box::leak(format!("Level{}", self.0).into_boxed_str());
            deserializer.deserialize_struct(name, &["down"], self)
        }
    }

    impl<'de> Deserialize<'de> for Abyss {
        fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            AbyssVisitor(0).deserialize(deserializer)
        }
    }

    #[test]
    fn should_report_untraceable_when_nesting_exceeds_the_depth_bound() {
        // given
        let traced = trace::<Abyss>;
        // when
        let shape = traced();
        // then
        let deepest = vec!["down"; MAX_DEPTH].join(".");
        assert_eq!(gaps(shape), [gap(&deepest, Gap::TooDeep)]);
    }

    #[derive(Deserialize, PartialEq)]
    #[serde(tag = "kind")]
    enum Tagged {
        Small,
    }

    #[derive(Deserialize, PartialEq)]
    struct WithTagged {
        id: u8,
        tagged: Tagged,
    }

    #[test]
    fn should_report_untraceable_when_type_needs_a_self_describing_format() {
        // given
        let traced = trace::<WithTagged>;
        // when
        let shape = traced();
        // then
        assert_eq!(gaps(shape), [gap("tagged", Gap::SelfDescribing)]);
    }

    #[derive(Deserialize, PartialEq)]
    struct Mixed {
        pair: (Nested, Plain),
    }

    #[test]
    fn should_report_a_conflict_when_two_shapes_share_a_path() {
        // given
        let traced = trace::<Mixed>;
        // when
        let shape = traced();
        // then
        assert_eq!(gaps(shape), [gap("pair[]", Gap::Conflict)]);
    }

    #[derive(Deserialize, PartialEq)]
    struct Marker;

    #[derive(Deserialize, PartialEq)]
    struct Wrapped(Nested);

    #[derive(Deserialize, PartialEq)]
    struct Point(u8, Nested);

    #[derive(Deserialize, PartialEq)]
    struct Containers {
        marker: Marker,
        wrapped: Wrapped,
        point: Point,
        flag: bool,
        ratio: f64,
        letter: char,
        raw: Vec<u8>,
        nothing: (),
    }

    #[test]
    fn should_see_through_unit_newtype_and_tuple_structs_when_they_are_fields() {
        // given
        let traced = trace::<Containers>;
        // when
        let shape = traced();
        // then
        assert_eq!(
            atoms(&shape),
            [
                field("flag"),
                field("letter"),
                field("marker"),
                field("nothing"),
                field("point"),
                field("point[].depth"),
                field("ratio"),
                field("raw"),
                field("wrapped"),
                field("wrapped.depth"),
            ]
        );
        assert!(shape.untraceable.is_empty());
    }

    /// A list that holds itself through a newtype.
    #[derive(Deserialize, PartialEq)]
    struct Chain(Option<Box<Chain>>);

    #[derive(Deserialize, PartialEq)]
    struct Pair(u8, Option<Box<Pair>>);

    #[derive(Deserialize, PartialEq)]
    struct Recursive {
        chain: Chain,
        pair: Pair,
    }

    #[test]
    fn should_stop_after_one_matching_unrolling_when_a_newtype_or_tuple_struct_is_recursive() {
        // given
        let traced = trace::<Recursive>;
        // when
        let shape = traced();
        // then
        assert_eq!(atoms(&shape), [field("chain"), field("pair")]);
        assert!(shape.untraceable.is_empty());
    }

    #[derive(Deserialize, PartialEq)]
    #[serde(rename_all = "snake_case")]
    enum Weather {
        #[serde(alias = "breeze")]
        Wind,
    }

    #[derive(Deserialize, PartialEq)]
    struct Renamed {
        #[serde(alias = "step")]
        rung: u8,
        weather: Weather,
    }

    #[test]
    fn should_list_aliases_as_fields_and_variants_when_a_type_declares_them() {
        // given
        let traced = trace::<Renamed>;
        // when
        let shape = traced();
        // then
        assert_eq!(
            atoms(&shape),
            [
                field("rung"),
                field("step"),
                field("weather"),
                variant("weather", "breeze"),
                variant("weather", "wind"),
            ]
        );
        assert!(shape.untraceable.is_empty());
    }

    #[derive(Deserialize, PartialEq)]
    enum Nothing {}

    #[derive(Deserialize, PartialEq)]
    struct WithNothing {
        before: u8,
        nothing: Option<Nothing>,
    }

    #[test]
    fn should_record_the_enum_when_it_has_no_variant() {
        // given
        let traced = trace::<WithNothing>;
        // when
        let shape = traced();
        // then
        assert_eq!(shape.nodes["nothing"], Node::Enum(vec![]));
        assert_eq!(atoms(&shape), [field("before"), field("nothing")]);
    }

    /// A type that reads one key and then refuses, so none of its fields' values is reached.
    #[derive(PartialEq)]
    struct Impatient;

    impl<'de> Deserialize<'de> for Impatient {
        fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            struct Refusing;
            impl<'de> serde::de::Visitor<'de> for Refusing {
                type Value = Impatient;
                fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                    f.write_str("nothing")
                }
                fn visit_map<M: serde::de::MapAccess<'de>>(
                    self,
                    mut map: M,
                ) -> Result<Impatient, M::Error> {
                    let key: Option<String> = map.next_key()?;
                    Err(serde::de::Error::custom(format!("refused at {key:?}")))
                }
            }
            deserializer.deserialize_struct("Impatient", &["a", "b"], Refusing)
        }
    }

    #[test]
    fn should_report_untraceable_when_a_field_is_found_but_never_reached() {
        // given
        let traced = trace::<Impatient>;
        // when
        let shape = traced();
        // then
        assert_eq!(
            gaps(shape),
            [gap("a", Gap::Unreached), gap("b", Gap::Unreached)]
        );
    }

    #[test]
    fn should_name_variant_paths_when_atoms_are_displayed() {
        // given
        let atoms = [field("reason::hand.by"), variant("reason", "hand")];
        // when
        let shown = atoms.map(|a| a.to_string());
        // then
        assert_eq!(shown, ["field `reason::hand.by`", "variant `reason::hand`"]);
    }

    #[test]
    fn should_say_what_to_change_when_a_gap_is_displayed() {
        // given
        let all = [
            Gap::SelfDescribing,
            Gap::Unreached,
            Gap::OpaqueLeaf,
            Gap::Conflict,
            Gap::Recursion,
            Gap::TooDeep,
            Gap::NotConverged,
        ];
        // when
        let shown = all.map(|g| g.to_string());
        // then
        assert!(shown[0].contains("self-describing"));
        assert!(shown[2].contains("externally tagged enum"));
        assert!(shown[2].contains("mayfly_domain::Sample"));
        assert!(shown.iter().all(|s| !s.is_empty()));
    }
}
