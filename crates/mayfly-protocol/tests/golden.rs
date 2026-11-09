//! The protocol golden fixtures (ver §4): `tests/fixtures/<message>/v<major>.json`, plus any
//! `v<major>.<slug>.json` (`<slug>` is `[a-z0-9][a-z0-9-]*`).
//!
//! Every message kind has its primary fixture for every major this build speaks; every fixture
//! decodes, sits under its message's directory and major, and re-encodes byte-identically
//! (pretty JSON and a final newline); together the fixtures show every directive kind,
//! observation kind, refusal reason and unknown reason, read from serde's own variant lists so
//! a new variant without a fixture fails. An unregistered file fails. Existing fixtures are
//! frozen: a shape change adds a new fixture beside the old one (ver §4 rule 1).

use std::collections::BTreeSet;
use std::fmt;
use std::path::Path;

use mayfly_protocol::{
    CURRENT, DirectiveKind, Frame, FrameHeader, Message, MessageKind, ObservationKind,
    RefusalReason, SUPPORTED, UnknownReason, VersionRefusal, admit_frame,
};
use mayfly_testkit_shared::fixtures::read_tree;
use serde::de::{self, DeserializeOwned, Deserializer, Visitor};
use serde_json::Value;

fn fixtures() -> Vec<(String, String)> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    read_tree(&root).expect("tests/fixtures is readable")
}

fn encode(frame: &Frame) -> String {
    serde_json::to_string_pretty(frame).expect("a frame encodes") + "\n"
}

// ---- serde's own variant lists ------------------------------------------------------------

/// A deserializer that only records the variant list an enum's `Deserialize` hands it.
struct VariantProbe;

#[derive(Debug)]
struct ProbeError(Option<&'static [&'static str]>);

impl fmt::Display for ProbeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("variant probe")
    }
}

impl std::error::Error for ProbeError {}

impl de::Error for ProbeError {
    fn custom<T: fmt::Display>(_: T) -> Self {
        Self(None)
    }
}

impl<'de> Deserializer<'de> for VariantProbe {
    type Error = ProbeError;

    fn deserialize_any<V: Visitor<'de>>(self, _: V) -> Result<V::Value, ProbeError> {
        Err(ProbeError(None))
    }

    fn deserialize_enum<V: Visitor<'de>>(
        self,
        _: &'static str,
        variants: &'static [&'static str],
        _: V,
    ) -> Result<V::Value, ProbeError> {
        Err(ProbeError(Some(variants)))
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string bytes byte_buf
        option unit unit_struct newtype_struct seq tuple tuple_struct map struct identifier
        ignored_any
    }
}

fn variants_of<T: DeserializeOwned>() -> BTreeSet<&'static str> {
    match T::deserialize(VariantProbe) {
        Err(ProbeError(Some(variants))) => variants.iter().copied().collect(),
        _ => panic!("not an enum"),
    }
}

/// The variant an externally tagged enum value is encoded as.
fn variant(value: &Value) -> String {
    match value {
        Value::String(name) => name.clone(),
        Value::Object(map) if map.len() == 1 => map.keys().next().cloned().unwrap_or_default(),
        other => panic!("not an externally tagged enum: {other}"),
    }
}

// ---- the harness --------------------------------------------------------------------------

/// The variants the fixtures show, per enum.
#[derive(Default)]
struct Seen {
    messages: BTreeSet<String>,
    directives: BTreeSet<String>,
    observations: BTreeSet<String>,
    refusals: BTreeSet<String>,
    unknowns: BTreeSet<String>,
}

impl Seen {
    fn record(&mut self, frame: &Frame) {
        let value = serde_json::to_value(&frame.message).expect("a message encodes");
        self.messages.insert(variant(&value));
        match &frame.message {
            Message::Directive(d) => {
                let kind = serde_json::to_value(&d.kind).expect("a kind encodes");
                self.directives.insert(variant(&kind));
            }
            Message::Observation(o) => {
                let kind = serde_json::to_value(&o.kind).expect("a kind encodes");
                self.observations.insert(variant(&kind));
                match &o.kind {
                    ObservationKind::DirectiveRefused { reason } => {
                        let r = serde_json::to_value(reason).expect("a reason encodes");
                        self.refusals.insert(variant(&r));
                    }
                    ObservationKind::OutcomeUnknown { reason } => {
                        let r = serde_json::to_value(reason).expect("a reason encodes");
                        self.unknowns.insert(variant(&r));
                    }
                    _ => {}
                }
            }
            Message::Hello(_) | Message::Welcome(_) | Message::HandshakeRefused(_) => {}
        }
    }
}

/// `<message>/v<major>.json` or `<message>/v<major>.<slug>.json`: the message and the major.
fn layout(path: &str) -> Option<(MessageKind, u16)> {
    let (dir, file) = path.split_once('/')?;
    let kind = MessageKind::ALL.into_iter().find(|k| k.name() == dir)?;
    let stem = file.strip_prefix('v')?.strip_suffix(".json")?;
    let (major, slug) = match stem.split_once('.') {
        Some((major, slug)) => (major, Some(slug)),
        None => (stem, None),
    };
    let slug_ok = slug.is_none_or(|s| {
        s.bytes()
            .next()
            .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
            && s.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    });
    let major_ok = !major.is_empty()
        && major.bytes().all(|b| b.is_ascii_digit())
        && !(major.len() > 1 && major.starts_with('0'));
    if !slug_ok || !major_ok {
        return None;
    }
    Some((kind, major.parse().ok()?))
}

/// Every disagreement between the fixtures and the protocol.
fn check(files: &[(String, String)]) -> Vec<String> {
    let mut problems = Vec::new();
    let mut seen = Seen::default();
    let mut primaries = BTreeSet::new();
    for (path, text) in files {
        let Some((kind, major)) = layout(path) else {
            problems.push(format!(
                "{path}: not a fixture of the layout <message>/v<major>[.<slug>].json"
            ));
            continue;
        };
        let frame: Frame = match serde_json::from_str(text) {
            Ok(frame) => frame,
            Err(e) => {
                problems.push(format!("{path}: does not decode: {e}"));
                continue;
            }
        };
        if frame.message.kind() != kind {
            problems.push(format!(
                "{path}: holds message `{}`",
                frame.message.kind().name()
            ));
        }
        if frame.protocol_version.major != major {
            problems.push(format!(
                "{path}: is written in major {}",
                frame.protocol_version.major
            ));
        }
        if admit_frame(None, frame.protocol_version, SUPPORTED).is_err() {
            problems.push(format!("{path}: is of a major this build does not speak"));
        }
        if &encode(&frame) != text {
            problems.push(format!("{path}: does not re-encode byte-identically"));
        }
        if path.ends_with(&format!("/v{major}.json")) {
            primaries.insert((kind, major));
        }
        seen.record(&frame);
    }
    for kind in MessageKind::ALL {
        for version in SUPPORTED {
            if !primaries.contains(&(kind, version.major)) {
                problems.push(format!(
                    "{}/v{}.json is missing",
                    kind.name(),
                    version.major
                ));
            }
        }
    }
    let coverage = [
        ("message", variants_of::<Message>(), &seen.messages),
        (
            "directive kind",
            variants_of::<DirectiveKind>(),
            &seen.directives,
        ),
        (
            "observation kind",
            variants_of::<ObservationKind>(),
            &seen.observations,
        ),
        (
            "refusal reason",
            variants_of::<RefusalReason>(),
            &seen.refusals,
        ),
        (
            "unknown reason",
            variants_of::<UnknownReason>(),
            &seen.unknowns,
        ),
    ];
    for (what, all, shown) in coverage {
        for name in all {
            if !shown.contains(name) {
                problems.push(format!("no fixture shows the {what} `{name}`"));
            }
        }
    }
    problems
}

// ---- tests --------------------------------------------------------------------------------

#[test]
fn should_agree_with_the_protocol_when_every_fixture_is_checked() {
    // given
    let files = fixtures();

    // when
    let problems = check(&files);

    // then
    assert!(problems.is_empty(), "{problems:#?}");
    assert!(files.len() > MessageKind::ALL.len());
}

#[test]
fn should_refuse_every_fixture_when_its_major_is_unknown() {
    // given
    let files = fixtures();

    for (path, text) in files {
        let mut value: Value = serde_json::from_str(&text).expect("a fixture is JSON");
        value["protocol_version"]["major"] = Value::from(CURRENT.major + 1);
        let rewritten = value.to_string();

        // when
        let header: FrameHeader = serde_json::from_str(&rewritten).expect("any frame has a header");
        let before_session = admit_frame(None, header.protocol_version, SUPPORTED);
        let in_session = admit_frame(Some(CURRENT), header.protocol_version, SUPPORTED);

        // then
        assert_eq!(
            before_session,
            Err(VersionRefusal::MajorNotSupported {
                major: CURRENT.major + 1
            }),
            "{path}"
        );
        assert!(
            matches!(in_session, Err(VersionRefusal::MajorMismatch { .. })),
            "{path}"
        );
    }
}

#[test]
fn should_read_the_header_when_the_body_is_of_an_unknown_major() {
    // given
    let frame =
        r#"{"protocol_version":{"major":9,"minor":0},"message":{"teleport":{"to":"mars"}}}"#;

    // when
    let header: FrameHeader = serde_json::from_str(frame).expect("the header decodes");

    // then
    assert_eq!(
        admit_frame(None, header.protocol_version, SUPPORTED),
        Err(VersionRefusal::MajorNotSupported { major: 9 })
    );
    assert!(serde_json::from_str::<Frame>(frame).is_err());
}

#[test]
fn should_decode_the_same_frame_when_a_newer_minor_adds_a_field() {
    // given
    for (path, text) in fixtures() {
        let original: Frame = serde_json::from_str(&text).expect("a fixture decodes");
        let mut value: Value = serde_json::from_str(&text).expect("a fixture is JSON");
        value["added_by_a_newer_minor"] = Value::from(true);
        if let Some(fields) = value["message"]
            .as_object_mut()
            .and_then(|m| m.values_mut().next())
            .and_then(Value::as_object_mut)
        {
            fields.insert("also_added".to_owned(), Value::from(1));
        }

        // when
        let decoded: Frame = serde_json::from_str(&value.to_string()).expect("still decodes");

        // then
        assert_eq!(decoded, original, "{path}");
    }
}

#[test]
fn should_name_each_disagreement_when_fixtures_are_wrong() {
    // given
    let files = fixtures();
    let without_clone: Vec<(String, String)> = files
        .iter()
        .filter(|(_, text)| !text.contains("\"clone_vm\""))
        .cloned()
        .collect();
    let mut reformatted = files.clone();
    reformatted[0].1 = reformatted[0].1.replace("  ", "    ");
    let mut stray = files.clone();
    stray.push(("hello/latest.json".to_owned(), files[0].1.clone()));
    stray.push(("teleport/v1.json".to_owned(), files[0].1.clone()));
    let mut misplaced = files.clone();
    let observation = files
        .iter()
        .find(|(p, _)| p == "observation/v1.json")
        .expect("observation/v1.json exists")
        .1
        .clone();
    misplaced.push(("welcome/v1.misplaced.json".to_owned(), observation));

    // when
    let missing = check(&without_clone);
    let unstable = check(&reformatted);
    let orphans = check(&stray);
    let wrong_dir = check(&misplaced);

    // then
    assert!(
        missing.contains(&"directive/v1.json is missing".to_owned()),
        "{missing:#?}"
    );
    assert!(missing.contains(&"no fixture shows the directive kind `clone_vm`".to_owned()));
    assert!(
        unstable
            .iter()
            .any(|p| p.ends_with("does not re-encode byte-identically")),
        "{unstable:#?}"
    );
    assert_eq!(orphans.len(), 2, "{orphans:#?}");
    assert_eq!(
        wrong_dir,
        vec!["welcome/v1.misplaced.json: holds message `observation`".to_owned()]
    );
}

#[test]
fn should_carry_only_the_handle_when_a_launch_is_encoded() {
    // given
    let (_, text) = fixtures()
        .into_iter()
        .find(|(p, _)| p == "directive/v1.launch-runner.json")
        .expect("the launch fixture exists");

    // when
    let value: Value = serde_json::from_str(&text).expect("a fixture is JSON");
    let launch = &value["message"]["directive"]["kind"]["launch_runner"]["launch"];

    // then
    assert_eq!(launch.as_object().map(|o| o.len()), Some(1), "{launch}");
    assert!(launch["handle_id"].is_string());
}
