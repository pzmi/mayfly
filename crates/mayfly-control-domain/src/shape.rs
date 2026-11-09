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
//! refuses the made-up input (an id that validates its string, say) and ends the run cannot hide
//! it. An item nothing can reach (behind a tuple element or a map key that refuses), and a type
//! that asks for a self-describing format (`deserialize_any`: serde's internally tagged,
//! adjacently tagged and untagged enums, `flatten`), are reported in [`Shape::untraceable`]
//! rather than guessed. A recursive type is traced once per container. Each behaviour is pinned
//! by a unit test at the end of this file.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::de::value::StrDeserializer;
use serde::de::{
    self, DeserializeOwned, DeserializeSeed, Deserializer, EnumAccess, IntoDeserializer, MapAccess,
    SeqAccess, VariantAccess, Visitor,
};

/// A bound on tracing runs: one run per enum variant and per refused value is ample.
const MAX_RUNS: usize = 4096;

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

/// The traced shape of one payload type.
///
/// Paths: a field is `parent.field` (`field` at the root), a variant's content
/// `parent::variant`, a sequence element `parent[]`, a map key `parent{key}` and a map value
/// `parent{}`. `Option` and newtypes are transparent, as in serde's JSON form.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Shape {
    /// The node at each path the trace reached.
    pub nodes: BTreeMap<String, Node>,
    /// Paths that need a self-describing format; the root when the trace did not converge.
    pub untraceable: BTreeSet<String>,
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

/// Traces `T`'s `Deserialize` impl into its [`Shape`].
#[must_use]
pub fn trace<T: DeserializeOwned>() -> Shape {
    let state = RefCell::new(State::default());
    let mut converged = false;
    for _ in 0..MAX_RUNS {
        let target = {
            let mut s = state.borrow_mut();
            s.stack.clear();
            s.target = s.next_target();
            s.target.clone()
        };
        // An error only ends this run: what it recorded on the way stays recorded.
        let _ = T::deserialize(Tracer {
            state: &state,
            path: String::new(),
        });
        let mut s = state.borrow_mut();
        if let Some(target) = target
            && !s.visited.contains(&target)
        {
            // A failing sibling that must come first (a tuple element, a map key) hides it.
            s.shape.untraceable.insert(target.clone());
            s.visited.insert(target);
        }
        if s.next_target().is_none() {
            converged = true;
            break;
        }
    }
    let mut shape = state.into_inner().shape;
    if !converged {
        shape.untraceable.insert(String::new());
    }
    shape
}

/// Whether the path `target` runs through `path`.
fn leads(target: &str, path: &str) -> bool {
    target
        .strip_prefix(path)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with(['.', ':', '[', '{']))
}

/// The trace's memory across runs.
///
/// Each field and variant found is an item; each run after the first aims at the first item not
/// yet visited, entering the field or variant that leads to it before anything else, so a
/// sibling that refuses the made-up input (and ends the run) cannot hide it.
#[derive(Default)]
struct State {
    shape: Shape,
    /// Fields (`parent.field`) and variants (`parent::variant`), in the order found.
    items: Vec<String>,
    found: BTreeSet<String>,
    visited: BTreeSet<String>,
    /// The item this run aims at.
    target: Option<String>,
    /// Named containers on the current path: a name seen twice is a recursive type.
    stack: Vec<&'static str>,
}

impl State {
    fn next_target(&self) -> Option<String> {
        self.items
            .iter()
            .find(|item| !self.visited.contains(*item))
            .cloned()
    }

    fn find(&mut self, item: String) {
        if self.found.insert(item.clone()) {
            self.items.push(item);
        }
    }

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
        self.shape.nodes.entry(path.to_owned()).or_insert(node);
    }

    fn toward(&self, path: &str) -> bool {
        self.target.as_deref().is_some_and(|t| leads(t, path))
    }

    fn enter(&mut self, name: &'static str) -> Result<(), TraceError> {
        if !name.is_empty() && self.stack.contains(&name) {
            return Err(TraceError(format!("recursive type {name}")));
        }
        self.stack.push(name);
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
    seed.deserialize(Tracer { state, path })
}

struct Tracer<'s> {
    state: &'s RefCell<State>,
    path: String,
}

impl Tracer<'_> {
    fn record(&self, node: Node) {
        self.state.borrow_mut().record(&self.path, node);
    }
}

macro_rules! leaf {
    ($($method:ident => $visit:ident($($value:expr)?)),+ $(,)?) => {
        $(
            fn $method<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, TraceError> {
                visitor.$visit($($value)?)
            }
        )+
    };
}

impl<'de> Deserializer<'de> for Tracer<'_> {
    type Error = TraceError;

    fn deserialize_any<V: Visitor<'de>>(self, _visitor: V) -> Result<V::Value, TraceError> {
        self.state
            .borrow_mut()
            .shape
            .untraceable
            .insert(self.path.clone());
        Err(TraceError(format!(
            "{}: the type needs a self-describing format",
            self.path
        )))
    }

    leaf! {
        deserialize_bool => visit_bool(false),
        deserialize_i8 => visit_i8(0),
        deserialize_i16 => visit_i16(0),
        deserialize_i32 => visit_i32(0),
        deserialize_i64 => visit_i64(0),
        deserialize_i128 => visit_i128(0),
        deserialize_u8 => visit_u8(0),
        deserialize_u16 => visit_u16(0),
        deserialize_u32 => visit_u32(0),
        deserialize_u64 => visit_u64(0),
        deserialize_u128 => visit_u128(0),
        deserialize_f32 => visit_f32(0.0),
        deserialize_f64 => visit_f64(0.0),
        deserialize_char => visit_char('a'),
        deserialize_str => visit_str(""),
        deserialize_string => visit_str(""),
        deserialize_bytes => visit_bytes(&[]),
        deserialize_byte_buf => visit_bytes(&[]),
        deserialize_unit => visit_unit(),
        deserialize_identifier => visit_str(""),
        deserialize_ignored_any => visit_unit(),
    }

    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, TraceError> {
        visitor.visit_some(self)
    }

    fn deserialize_unit_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, TraceError> {
        visitor.visit_unit()
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        name: &'static str,
        visitor: V,
    ) -> Result<V::Value, TraceError> {
        let state = self.state;
        state.borrow_mut().enter(name)?;
        let result = visitor.visit_newtype_struct(self);
        state.borrow_mut().leave();
        result
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
        visitor.visit_seq(Elements {
            state: self.state,
            path: element_path(&self.path),
            left: len,
        })
    }

    fn deserialize_tuple_struct<V: Visitor<'de>>(
        self,
        name: &'static str,
        len: usize,
        visitor: V,
    ) -> Result<V::Value, TraceError> {
        let state = self.state;
        state.borrow_mut().enter(name)?;
        let result = self.deserialize_tuple(len, visitor);
        state.borrow_mut().leave();
        result
    }

    fn deserialize_map<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, TraceError> {
        self.record(Node::Map);
        visitor.visit_map(Entry {
            state: self.state,
            key: key_path(&self.path),
            value: value_path(&self.path),
            left: true,
        })
    }

    fn deserialize_struct<V: Visitor<'de>>(
        self,
        name: &'static str,
        fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, TraceError> {
        let state = self.state;
        state.borrow_mut().enter(name)?;
        self.record(Node::Struct(fields.to_vec()));
        let (mut ordered, rest): (Vec<_>, Vec<_>) = {
            let s = state.borrow();
            fields
                .iter()
                .map(|f| (*f, field_path(&self.path, f)))
                .partition(|(_, path)| s.toward(path))
        };
        ordered.extend(rest);
        let result = visitor.visit_map(Fields {
            state,
            fields: ordered.into_iter(),
            pending: None,
        });
        state.borrow_mut().leave();
        result
    }

    fn deserialize_enum<V: Visitor<'de>>(
        self,
        name: &'static str,
        variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, TraceError> {
        let state = self.state;
        state.borrow_mut().enter(name)?;
        self.record(Node::Enum(variants.to_vec()));
        let pick = state.borrow_mut().choose(&self.path, variants);
        let result = match pick {
            Some(variant) => visitor.visit_enum(Variant {
                state,
                path: variant_path(&self.path, variant),
                name: variant,
            }),
            None => Err(TraceError(format!(
                "{}: the enum has no variant",
                self.path
            ))),
        };
        state.borrow_mut().leave();
        result
    }
}

struct Elements<'s> {
    state: &'s RefCell<State>,
    path: String,
    left: usize,
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
        self.left -= 1;
        let element = traced(self.state, self.path.clone(), seed);
        if element.is_err() && self.left > 0 {
            // Elements come in order: one that refuses hides the shape of those after it.
            self.state
                .borrow_mut()
                .shape
                .untraceable
                .insert(self.path.clone());
        }
        element.map(Some)
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
        let key = traced(self.state, self.key.clone(), seed);
        if key.is_err() {
            // The value comes only after a key: a key that refuses hides the value's shape.
            self.state
                .borrow_mut()
                .shape
                .untraceable
                .insert(self.value.clone());
        }
        key.map(Some)
    }

    fn next_value_seed<T: DeserializeSeed<'de>>(
        &mut self,
        seed: T,
    ) -> Result<T::Value, TraceError> {
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

    use serde::Deserialize;

    use super::{Atom, Node, trace};

    fn field(path: &str) -> Atom {
        Atom::Field(path.to_owned())
    }

    fn variant(path: &str, name: &str) -> Atom {
        Atom::Variant(path.to_owned(), name.to_owned())
    }

    fn atoms(shape: &super::Shape) -> Vec<Atom> {
        shape.atoms().into_iter().collect()
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
    fn should_trace_past_a_value_when_it_refuses_the_made_up_input() {
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
    struct Tree {
        label: u8,
        children: Vec<Tree>,
    }

    #[test]
    fn should_trace_once_when_type_is_recursive() {
        // given
        let traced = trace::<Tree>;
        // when
        let shape = traced();
        // then
        assert_eq!(atoms(&shape), [field("children"), field("label")]);
        assert!(shape.untraceable.is_empty());
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
        assert_eq!(
            shape.untraceable.into_iter().collect::<Vec<_>>(),
            ["tagged"]
        );
    }

    #[derive(Deserialize, PartialEq)]
    struct Hidden {
        pair: (Name, Nested),
        index: BTreeMap<Name, Nested>,
    }

    #[test]
    fn should_report_untraceable_when_a_refusing_element_or_key_hides_the_rest() {
        // given
        let traced = trace::<Hidden>;
        // when
        let shape = traced();
        // then
        assert_eq!(
            shape.untraceable.into_iter().collect::<Vec<_>>(),
            ["index{}", "pair[]"]
        );
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
                field("marker"),
                field("point"),
                field("point[].depth"),
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
    fn should_trace_once_when_a_newtype_or_tuple_struct_is_recursive() {
        // given
        let traced = trace::<Recursive>;
        // when
        let shape = traced();
        // then
        assert_eq!(atoms(&shape), [field("chain"), field("pair")]);
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
            shape.untraceable.into_iter().collect::<Vec<_>>(),
            ["a", "b"]
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
}
