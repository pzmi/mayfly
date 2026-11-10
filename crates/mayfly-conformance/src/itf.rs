//! A narrow decoder of the Informal Trace Format (ITF) Quint writes with `--mbt` (ver §6.2
//! item 2): the JSON encodings of big integers (`#bigint`), sets (`#set`), maps (`#map`),
//! tuples (`#tup`), records, variants (`{"tag", "value"}`) and unserialisable values
//! (`#unserializable`), plus the two model-based-testing variables `mbt::actionTaken` and
//! `mbt::nondetPicks`.
//!
//! The decoder is deliberately narrow: an integer outside `i128`, an object that is none of the
//! encodings above, a state without the MBT variables or a trace without states is a
//! [`TraceError`], never a value the driver could misread. Pinned by the unit tests below,
//! `should_decode_every_encoding_when_a_state_uses_them` and
//! `should_refuse_a_trace_when_the_mbt_variables_are_missing`.

use std::collections::BTreeMap;
use std::fmt;

use serde_json::Value as Json;

/// The MBT variable naming the action Quint took to reach a state (`init` for the first).
pub const ACTION_TAKEN: &str = "mbt::actionTaken";
/// The MBT variable holding the nondeterministic picks of that action.
pub const NONDET_PICKS: &str = "mbt::nondetPicks";

/// One decoded ITF value.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Value {
    /// `true` / `false`.
    Bool(bool),
    /// A plain JSON integer or `{"#bigint": "..."}`.
    Int(i128),
    /// A string.
    Str(String),
    /// A JSON array: a Quint list.
    List(Vec<Value>),
    /// `{"#set": [...]}`, kept in ITF's order.
    Set(Vec<Value>),
    /// `{"#map": [[k, v], ...]}`.
    Map(Vec<(Value, Value)>),
    /// `{"#tup": [...]}`.
    Tuple(Vec<Value>),
    /// A JSON object of fields: a Quint record.
    Record(BTreeMap<String, Value>),
    /// `{"tag": ..., "value": ...}`: a sum-type variant (`Some`/`None` included).
    Variant {
        /// The constructor's name.
        tag: String,
        /// Its argument (the empty tuple for a nullary one).
        value: Box<Value>,
    },
    /// `{"#unserializable": "..."}`: a value ITF cannot carry (an infinite set, a lambda).
    Unserializable(String),
}

impl Value {
    /// The integer, if this is one.
    #[must_use]
    pub const fn as_int(&self) -> Option<i128> {
        match self {
            Self::Int(n) => Some(*n),
            _ => None,
        }
    }

    /// The boolean, if this is one.
    #[must_use]
    pub const fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// The string, if this is one.
    #[must_use]
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::Str(s) => Some(s),
            _ => None,
        }
    }

    /// The field `name` of a record.
    #[must_use]
    pub fn field(&self, name: &str) -> Option<&Self> {
        match self {
            Self::Record(fields) => fields.get(name),
            _ => None,
        }
    }

    /// The argument of `Some(x)`, `None` for the `None` variant or any other value.
    #[must_use]
    pub fn some(&self) -> Option<&Self> {
        match self {
            Self::Variant { tag, value } if tag == "Some" => Some(value),
            _ => None,
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fn list(f: &mut fmt::Formatter<'_>, items: &[Value]) -> fmt::Result {
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    f.write_str(", ")?;
                }
                write!(f, "{item}")?;
            }
            Ok(())
        }
        match self {
            Self::Bool(b) => write!(f, "{b}"),
            Self::Int(n) => write!(f, "{n}"),
            Self::Str(s) => write!(f, "{s:?}"),
            Self::List(items) => {
                f.write_str("[")?;
                list(f, items)?;
                f.write_str("]")
            }
            Self::Set(items) => {
                f.write_str("Set(")?;
                list(f, items)?;
                f.write_str(")")
            }
            Self::Tuple(items) => {
                f.write_str("(")?;
                list(f, items)?;
                f.write_str(")")
            }
            Self::Map(pairs) => {
                f.write_str("Map(")?;
                for (i, (k, v)) in pairs.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{k} -> {v}")?;
                }
                f.write_str(")")
            }
            Self::Record(fields) => {
                f.write_str("{ ")?;
                for (i, (k, v)) in fields.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{k}: {v}")?;
                }
                f.write_str(" }")
            }
            Self::Variant { tag, value } => write!(f, "{tag}({value})"),
            Self::Unserializable(s) => write!(f, "<unserializable {s}>"),
        }
    }
}

/// Why a trace is not usable. A trace error fails the replay; it is never a pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TraceError {
    /// Not JSON at all.
    Json(String),
    /// The document lacks a part ITF requires (`states`, a state's `#meta`).
    Shape(String),
    /// A value is none of the encodings this decoder accepts.
    Encoding {
        /// Where, as `states[i].var.path`.
        at: String,
        /// What was found.
        what: String,
    },
    /// A state lacks `mbt::actionTaken` or `mbt::nondetPicks` (the trace was not made with
    /// `--mbt`).
    NoMbt {
        /// The state's index.
        state: usize,
    },
    /// The trace has a number of states the manifest does not allow.
    Length {
        /// States found.
        found: usize,
        /// The bound: `max_steps + 1`.
        most: usize,
        /// Whether the manifest requires exactly `most`.
        exact: bool,
    },
}

impl fmt::Display for TraceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Json(e) => write!(f, "trace is not JSON: {e}"),
            Self::Shape(e) => write!(f, "trace is not ITF: {e}"),
            Self::Encoding { at, what } => write!(f, "unsupported ITF value at {at}: {what}"),
            Self::NoMbt { state } => write!(
                f,
                "state {state} has no {ACTION_TAKEN} or {NONDET_PICKS}: generate the trace with --mbt"
            ),
            Self::Length { found, most, exact } => {
                if *exact {
                    write!(f, "trace has {found} states, the manifest requires {most}")
                } else {
                    write!(
                        f,
                        "trace has {found} states, more than max_steps + 1 = {most}"
                    )
                }
            }
        }
    }
}

impl std::error::Error for TraceError {}

/// One state of a trace: its variables, and how Quint reached it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct State {
    /// The state's position (`#meta.index`).
    pub index: usize,
    /// The model's variables, the MBT ones excluded.
    pub vars: BTreeMap<String, Value>,
    /// `mbt::actionTaken`.
    pub action: String,
    /// `mbt::nondetPicks`: each pick's value, `None` when the action did not pick it.
    pub picks: BTreeMap<String, Option<Value>>,
}

/// A decoded ITF trace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trace {
    /// The states, the initial one first.
    pub states: Vec<State>,
}

/// Decodes `text` as an ITF trace written with `--mbt`.
///
/// # Errors
/// [`TraceError`] when it is not JSON, not ITF, uses an encoding outside this decoder, or has
/// no MBT variables.
pub fn parse(text: &str) -> Result<Trace, TraceError> {
    let doc: Json = serde_json::from_str(text).map_err(|e| TraceError::Json(e.to_string()))?;
    let states = doc
        .get("states")
        .and_then(Json::as_array)
        .ok_or_else(|| TraceError::Shape("no \"states\" array".to_owned()))?;
    if states.is_empty() {
        return Err(TraceError::Shape("no state".to_owned()));
    }
    let decoded = states
        .iter()
        .enumerate()
        .map(|(position, state)| decode_state(position, state))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Trace { states: decoded })
}

fn decode_state(position: usize, state: &Json) -> Result<State, TraceError> {
    let object = state
        .as_object()
        .ok_or_else(|| TraceError::Shape(format!("state {position} is not an object")))?;
    let index = object
        .get("#meta")
        .and_then(|m| m.get("index"))
        .and_then(Json::as_u64)
        .and_then(|i| usize::try_from(i).ok())
        .unwrap_or(position);
    let mut vars = BTreeMap::new();
    let mut action = None;
    let mut picks = None;
    for (name, raw) in object {
        let at = format!("states[{position}].{name}");
        match name.as_str() {
            "#meta" => {}
            ACTION_TAKEN => action = raw.as_str().map(str::to_owned),
            NONDET_PICKS => picks = Some(decode_picks(&at, raw)?),
            _ => {
                vars.insert(name.clone(), decode(&at, raw)?);
            }
        }
    }
    match (action, picks) {
        (Some(action), Some(picks)) => Ok(State {
            index,
            vars,
            action,
            picks,
        }),
        _ => Err(TraceError::NoMbt { state: position }),
    }
}

/// `mbt::nondetPicks` is a record of `Option`s: `Some(v)` when the action picked `v`.
fn decode_picks(at: &str, raw: &Json) -> Result<BTreeMap<String, Option<Value>>, TraceError> {
    let Value::Record(fields) = decode(at, raw)? else {
        return Err(TraceError::Encoding {
            at: at.to_owned(),
            what: "nondetPicks is not a record".to_owned(),
        });
    };
    fields
        .into_iter()
        .map(|(name, value)| match value {
            Value::Variant { tag, value } if tag == "Some" => Ok((name, Some(*value))),
            Value::Variant { tag, .. } if tag == "None" => Ok((name, None)),
            other => Err(TraceError::Encoding {
                at: format!("{at}.{name}"),
                what: format!("a pick that is not an Option: {other}"),
            }),
        })
        .collect()
}

/// Decodes one ITF value.
///
/// # Errors
/// [`TraceError::Encoding`] for a float, `null`, an out-of-range integer or an object of an
/// unknown shape.
pub fn decode(at: &str, raw: &Json) -> Result<Value, TraceError> {
    let bad = |what: String| TraceError::Encoding {
        at: at.to_owned(),
        what,
    };
    match raw {
        Json::Bool(b) => Ok(Value::Bool(*b)),
        Json::String(s) => Ok(Value::Str(s.clone())),
        Json::Number(n) => n
            .as_i64()
            .map(|n| Value::Int(i128::from(n)))
            .ok_or_else(|| bad(format!("a number that is not an integer: {n}"))),
        Json::Null => Err(bad("null".to_owned())),
        Json::Array(items) => items
            .iter()
            .enumerate()
            .map(|(i, item)| decode(&format!("{at}[{i}]"), item))
            .collect::<Result<_, _>>()
            .map(Value::List),
        Json::Object(object) => {
            let single = |key: &str| (object.len() == 1).then(|| object.get(key)).flatten();
            if let Some(big) = single("#bigint") {
                return big
                    .as_str()
                    .and_then(|s| s.parse::<i128>().ok())
                    .map(Value::Int)
                    .ok_or_else(|| bad(format!("a #bigint outside i128: {big}")));
            }
            if let Some(items) = single("#set") {
                return elements(at, items).map(Value::Set);
            }
            if let Some(items) = single("#tup") {
                return elements(at, items).map(Value::Tuple);
            }
            if let Some(pairs) = single("#map") {
                let Json::Array(pairs) = pairs else {
                    return Err(bad("#map is not an array".to_owned()));
                };
                return pairs
                    .iter()
                    .enumerate()
                    .map(|(i, pair)| match pair.as_array().map(Vec::as_slice) {
                        Some([k, v]) => Ok((
                            decode(&format!("{at}#map[{i}].key"), k)?,
                            decode(&format!("{at}#map[{i}].value"), v)?,
                        )),
                        _ => Err(bad(format!("#map entry {i} is not a pair"))),
                    })
                    .collect::<Result<_, _>>()
                    .map(Value::Map);
            }
            if let Some(what) = single("#unserializable") {
                return Ok(Value::Unserializable(what.to_string()));
            }
            if object.len() == 2
                && let (Some(Json::String(tag)), Some(value)) =
                    (object.get("tag"), object.get("value"))
            {
                return Ok(Value::Variant {
                    tag: tag.clone(),
                    value: Box::new(decode(&format!("{at}.{tag}"), value)?),
                });
            }
            if let Some(key) = object.keys().find(|k| k.starts_with('#')) {
                return Err(bad(format!("unknown ITF encoding {key}")));
            }
            object
                .iter()
                .map(|(name, value)| Ok((name.clone(), decode(&format!("{at}.{name}"), value)?)))
                .collect::<Result<_, _>>()
                .map(Value::Record)
        }
    }
}

fn elements(at: &str, raw: &Json) -> Result<Vec<Value>, TraceError> {
    let Json::Array(items) = raw else {
        return Err(TraceError::Encoding {
            at: at.to_owned(),
            what: "a collection that is not an array".to_owned(),
        });
    };
    items
        .iter()
        .enumerate()
        .map(|(i, item)| decode(&format!("{at}[{i}]"), item))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decoded(json: &str) -> Value {
        decode(
            "t",
            &serde_json::from_str(json).unwrap_or_else(|e| panic!("{e}")),
        )
        .unwrap_or_else(|e| panic!("{e}"))
    }

    #[test]
    fn should_decode_every_encoding_when_a_state_uses_them() {
        // given
        let json = r##"{
            "big": {"#bigint": "-170141183460469231731687303715884105728"},
            "plain": 7,
            "set": {"#set": [{"#bigint": "1"}, {"#bigint": "2"}]},
            "map": {"#map": [["a", true]]},
            "tup": {"#tup": [{"#bigint": "1"}, "x"]},
            "some": {"tag": "Some", "value": {"#bigint": "3"}},
            "none": {"tag": "None", "value": {"#tup": []}},
            "list": [false],
            "lambda": {"#unserializable": "fn"}
        }"##;

        // when
        let value = decoded(json);

        // then
        assert_eq!(value.field("big").and_then(Value::as_int), Some(i128::MIN));
        assert_eq!(value.field("plain").and_then(Value::as_int), Some(7));
        assert_eq!(
            value.field("set"),
            Some(&Value::Set(vec![Value::Int(1), Value::Int(2)]))
        );
        assert_eq!(
            value.field("map"),
            Some(&Value::Map(vec![(
                Value::Str("a".to_owned()),
                Value::Bool(true)
            )]))
        );
        assert_eq!(
            value.field("tup"),
            Some(&Value::Tuple(vec![
                Value::Int(1),
                Value::Str("x".to_owned())
            ]))
        );
        assert_eq!(
            value.field("some").and_then(Value::some),
            Some(&Value::Int(3))
        );
        assert_eq!(value.field("none").and_then(Value::some), None);
        assert_eq!(
            value.field("list"),
            Some(&Value::List(vec![Value::Bool(false)]))
        );
        assert!(matches!(
            value.field("lambda"),
            Some(Value::Unserializable(..))
        ));
    }

    #[test]
    fn should_refuse_a_value_when_its_encoding_is_unknown_or_out_of_range() {
        // given
        let inputs = [
            r##"{"#bigint": "170141183460469231731687303715884105728"}"##,
            r##"{"#float": "1.5"}"##,
            "1.5",
            "null",
            r##"{"#map": [[1]]}"##,
        ];

        // when
        let results: Vec<_> = inputs
            .iter()
            .map(|json| {
                decode(
                    "t",
                    &serde_json::from_str(json).unwrap_or_else(|e| panic!("{e}")),
                )
            })
            .collect();

        // then
        assert!(
            results
                .iter()
                .all(|r| matches!(r, Err(TraceError::Encoding { .. }))),
            "{results:?}"
        );
    }

    #[test]
    fn should_read_action_and_picks_when_a_state_has_mbt_variables() {
        // given
        let text = r##"{"states": [{"#meta": {"index": 0}, "x": 1,
            "mbt::actionTaken": "init",
            "mbt::nondetPicks": {"n": {"tag": "Some", "value": 2}, "m": {"tag": "None", "value": {"#tup": []}}}}]}"##;

        // when
        let trace = parse(text).unwrap_or_else(|e| panic!("{e}"));

        // then
        let state = &trace.states[0];
        assert_eq!(state.action, "init");
        assert_eq!(state.picks.get("n"), Some(&Some(Value::Int(2))));
        assert_eq!(state.picks.get("m"), Some(&None));
        assert_eq!(state.vars.keys().collect::<Vec<_>>(), ["x"]);
    }

    #[test]
    fn should_refuse_a_trace_when_the_mbt_variables_are_missing() {
        // given
        let without = r##"{"states": [{"#meta": {"index": 0}, "x": 1}]}"##;
        let empty = r#"{"states": []}"#;

        // when
        let results = (parse(without), parse(empty), parse("[1"));

        // then
        assert_eq!(results.0, Err(TraceError::NoMbt { state: 0 }));
        assert!(matches!(results.1, Err(TraceError::Shape(..))));
        assert!(matches!(results.2, Err(TraceError::Json(..))));
    }

    #[test]
    fn should_render_values_and_errors_when_displayed() {
        // given
        let mut fields = BTreeMap::new();
        fields.insert("x".to_owned(), Value::Int(1));
        fields.insert("y".to_owned(), Value::Bool(false));
        let value = Value::List(vec![
            Value::Set(vec![Value::Int(1), Value::Int(2)]),
            Value::Tuple(vec![Value::Str("s".to_owned()), Value::Int(3)]),
            Value::Map(vec![
                (Value::Int(1), Value::Int(2)),
                (Value::Int(3), Value::Int(4)),
            ]),
            Value::Record(fields),
            Value::Variant {
                tag: "Some".to_owned(),
                value: Box::new(Value::Int(5)),
            },
            Value::Unserializable("f".to_owned()),
        ]);
        let errors = [
            TraceError::Json("j".to_owned()),
            TraceError::Shape("s".to_owned()),
            TraceError::Encoding {
                at: "a".to_owned(),
                what: "w".to_owned(),
            },
            TraceError::NoMbt { state: 2 },
            TraceError::Length {
                found: 5,
                most: 3,
                exact: true,
            },
            TraceError::Length {
                found: 5,
                most: 3,
                exact: false,
            },
        ];

        // when
        let shown = value.to_string();
        let messages: Vec<String> = errors.iter().map(ToString::to_string).collect();

        // then
        assert_eq!(
            shown,
            "[Set(1, 2), (\"s\", 3), Map(1 -> 2, 3 -> 4), { x: 1, y: false }, Some(5), <unserializable f>]"
        );
        assert_eq!(value.as_int(), None);
        assert_eq!(value.as_bool(), None);
        assert_eq!(value.as_str(), None);
        assert_eq!(value.field("x"), None);
        assert_eq!(Value::Str("t".to_owned()).as_str(), Some("t"));
        assert_eq!(Value::Bool(true).as_bool(), Some(true));
        assert!(messages.iter().all(|m| !m.is_empty()));
        assert!(messages[3].contains("--mbt"));
        assert!(messages[4].contains("requires 3"));
        assert!(messages[5].contains("max_steps + 1 = 3"));
    }
}
