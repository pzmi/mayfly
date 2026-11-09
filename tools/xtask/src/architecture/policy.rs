//! The versioned policy file, `architecture.toml` (foundation.md section 3.2).
//!
//! Parsing is strict: unknown fields fail, and a `schema` other than [`SCHEMA`] fails before
//! any other field is read.

use std::collections::BTreeMap;
use std::fmt;

use serde::Deserialize;
use serde::de::{self, Deserializer};

/// The only schema version this checker reads.
pub const SCHEMA: i64 = 1;

/// The whole policy.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    /// Schema version; must equal [`SCHEMA`].
    pub schema: i64,
    /// The category of every workspace crate.
    pub workspace: Categories,
    /// Allowed workspace-to-workspace edges.
    pub dependencies: Dependencies,
    /// Allowed third-party crates per workspace crate and dependency kind.
    #[serde(default)]
    pub external: External,
    /// Approved exceptions.
    #[serde(default)]
    pub exceptions: Exceptions,
    /// Settings of the semantic rules.
    #[serde(default)]
    pub semantic: Semantic,
    /// The plane of every crate that has one, and each plane's own vocabulary.
    #[serde(default)]
    pub planes: Planes,
    /// The crates the purity, process-authority and independent-oracle rules select.
    #[serde(default)]
    pub purity: Purity,
}

/// A plane of foundation.md section 2.1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Plane {
    /// The control plane: decides, owns the journal.
    Control,
    /// The data plane: acts, stores nothing durable.
    Data,
    /// Where the two planes meet: the kernel, the protocol, the link.
    Shared,
    /// Sees both planes: composition, the CLI, conformance.
    Both,
}

impl Plane {
    /// The key under `[planes]`.
    pub fn key(self) -> &'static str {
        match self {
            Self::Control => "control",
            Self::Data => "data",
            Self::Shared => "shared",
            Self::Both => "both",
        }
    }

    /// The planes a crate of this plane must never reach, by any edge kind or name: the other
    /// plane for a plane crate, both planes for a shared crate.
    pub fn forbidden(self) -> &'static [Plane] {
        match self {
            Self::Control => &[Self::Data],
            Self::Data => &[Self::Control],
            Self::Shared => &[Self::Control, Self::Data],
            Self::Both => &[],
        }
    }
}

/// `[planes]`: plane membership (foundation.md section 2.1). A production crate is in exactly
/// one list once any list is given; test-only crates may be listed too, tools are not.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Planes {
    /// Control-plane crates.
    #[serde(default)]
    pub control: Vec<String>,
    /// Data-plane crates.
    #[serde(default)]
    pub data: Vec<String>,
    /// Crates both planes may reach.
    #[serde(default)]
    pub shared: Vec<String>,
    /// Crates that see both planes.
    #[serde(default)]
    pub both: Vec<String>,
    /// `[planes.vocabulary]`: names that belong to one plane only.
    #[serde(default)]
    pub vocabulary: Vocabulary,
}

impl Planes {
    /// Each plane with its list, in a fixed order.
    pub fn lists(&self) -> [(Plane, &Vec<String>); 4] {
        [
            (Plane::Control, &self.control),
            (Plane::Data, &self.data),
            (Plane::Shared, &self.shared),
            (Plane::Both, &self.both),
        ]
    }

    /// Whether any plane list is given: then every production crate must be in one.
    pub fn declared(&self) -> bool {
        self.lists().iter().any(|(_, list)| !list.is_empty())
    }

    /// The plane of `name`: the first list naming it.
    pub fn of(&self, name: &str) -> Option<Plane> {
        self.lists()
            .into_iter()
            .find(|(_, list)| list.iter().any(|n| n == name))
            .map(|(plane, _)| plane)
    }
}

/// `[planes.vocabulary]`: identifiers that only the named plane may write (a CP-only type
/// such as `Journal`, a DP-only port such as `VmProvider`). A crate of the other plane, or a
/// shared crate, that names one fails `ARCH-plane-vocabulary`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Vocabulary {
    /// Names only the control plane may write.
    #[serde(default)]
    pub control: Vec<String>,
    /// Names only the data plane may write.
    #[serde(default)]
    pub data: Vec<String>,
}

impl Vocabulary {
    /// The words of `plane`'s own vocabulary.
    pub fn of(&self, plane: Plane) -> &[String] {
        match plane {
            Plane::Control => &self.control,
            Plane::Data => &self.data,
            Plane::Shared | Plane::Both => &[],
        }
    }
}

/// `[purity]`: the crates the semantic rules select by role (foundation.md section 2.3).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Purity {
    /// Crates held to `ARCH-domain-purity`.
    #[serde(default)]
    pub pure: Vec<String>,
    /// Production crates that may name a process API (`ARCH-process-authority`).
    #[serde(default)]
    pub process_authority: Vec<String>,
    /// Independent oracles that reach no workspace crate (`ARCH-independent-oracle`, D11).
    #[serde(default)]
    pub independent: Vec<String>,
}

impl Purity {
    /// Each list with its key, in a fixed order.
    pub fn lists(&self) -> [(&'static str, &Vec<String>); 3] {
        [
            ("pure", &self.pure),
            ("process_authority", &self.process_authority),
            ("independent", &self.independent),
        ]
    }
}

/// `[semantic]`: settings of the semantic rules.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Semantic {
    /// Crates the rules name that this workspace deliberately lacks (fixtures with part of
    /// the topology). Every other crate a rule names must be a workspace member.
    #[serde(default)]
    pub absent: Vec<String>,
}

/// A crate category. Every workspace crate is in exactly one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Category {
    /// Shipped code.
    Production,
    /// Test support, never in a production closure.
    TestOnly,
    /// Repository tooling, never in a production closure.
    ToolOnly,
    /// A fuzz crate: outside the root workspace, built only with the pinned nightly.
    FuzzOnly,
}

impl Category {
    /// The key under `[workspace]`.
    pub fn key(self) -> &'static str {
        match self {
            Self::Production => "production",
            Self::TestOnly => "test_only",
            Self::ToolOnly => "tool_only",
            Self::FuzzOnly => "fuzz_only",
        }
    }
}

/// `[workspace]`: one optional list per category. A list that is present must not be empty.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Categories {
    /// Shipped crates.
    pub production: Option<Vec<String>>,
    /// Test-support crates.
    pub test_only: Option<Vec<String>>,
    /// Tooling crates.
    pub tool_only: Option<Vec<String>>,
    /// Fuzz crates. They are not workspace members: each is the
    /// `fuzz/Cargo.toml` package with its own `[workspace]` table.
    pub fuzz_only: Option<Vec<String>>,
}

impl Categories {
    /// Each category with its list, in a fixed order.
    pub fn lists(&self) -> [(Category, Option<&Vec<String>>); 4] {
        [
            (Category::Production, self.production.as_ref()),
            (Category::TestOnly, self.test_only.as_ref()),
            (Category::ToolOnly, self.tool_only.as_ref()),
            (Category::FuzzOnly, self.fuzz_only.as_ref()),
        ]
    }
}

/// `[dependencies]`: allowed workspace edges by kind.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Dependencies {
    /// `[dependencies.normal]`: crate to the workspace crates it may depend on. A crate
    /// without an entry may depend on none. Dev-dependencies need no table: they may target
    /// a test_only crate or a crate in this list.
    pub normal: BTreeMap<String, Vec<String>>,
    /// `[dependencies.build]`: crate to the workspace crates it may build-depend on.
    #[serde(default)]
    pub build: BTreeMap<String, Vec<String>>,
}

impl Dependencies {
    /// Each kind's table with its key, in a fixed order.
    pub fn tables(&self) -> [(&'static str, &BTreeMap<String, Vec<String>>); 2] {
        [("normal", &self.normal), ("build", &self.build)]
    }
}

/// Third-party crate name to the features it may enable.
pub type Allowed = BTreeMap<String, Features>;

/// Workspace crate to its [`Allowed`] third-party crates.
pub type ExternalTable = BTreeMap<String, Allowed>;

/// `[external.<kind>]`: workspace crate to the third-party crates it may depend on directly.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct External {
    /// Normal dependencies.
    #[serde(default)]
    pub normal: ExternalTable,
    /// Dev-dependencies. A crate listed under `normal` is also allowed as a dev-dependency.
    #[serde(default)]
    pub dev: ExternalTable,
    /// Build-dependencies.
    #[serde(default)]
    pub build: ExternalTable,
}

impl External {
    /// Each kind's table with its key, in a fixed order.
    pub fn tables(&self) -> [(&'static str, &ExternalTable); 3] {
        [
            ("normal", &self.normal),
            ("dev", &self.dev),
            ("build", &self.build),
        ]
    }
}

/// The features a third-party dependency may enable: `"*"` for any, or a list where
/// `"default"` stands for the dependency's default features.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Features {
    /// Any feature.
    Any,
    /// Only these features and what they imply.
    Only(Vec<String>),
}

impl<'de> Deserialize<'de> for Features {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visit;
        impl<'de> de::Visitor<'de> for Visit {
            type Value = Features;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("\"*\" or a list of feature names")
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<Features, E> {
                if v == "*" {
                    Ok(Features::Any)
                } else {
                    Err(E::invalid_value(de::Unexpected::Str(v), &self))
                }
            }
            fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> Result<Features, A::Error> {
                let mut out = Vec::new();
                while let Some(name) = seq.next_element::<String>()? {
                    out.push(name);
                }
                Ok(Features::Only(out))
            }
        }
        deserializer.deserialize_any(Visit)
    }
}

/// `[exceptions]`: approved departures from the rules, each citing an ADR.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Exceptions {
    /// `[exceptions.sources]`: a package source other than crates.io (without its `?query`
    /// or `#revision`, for example `git+https://host/repo`) to the approval.
    #[serde(default)]
    pub sources: BTreeMap<String, SourceException>,
    /// `[[exceptions.lints]]`: approved lint suppressions (`#[allow]`, `#[expect]`,
    /// `#[coverage(off)]`) and production `todo!`/`unimplemented!`/`dbg!` calls, by file.
    #[serde(default)]
    pub lints: Vec<LintException>,
    /// `[exceptions.vendor]`: a `directory` source, relative to the workspace root, that a
    /// `.cargo/config.toml` may substitute for crates.io (the fixtures' offline stubs), to
    /// the approval. Any other `[source]` replacement fails `ARCH-lint-config`.
    #[serde(default)]
    pub vendor: BTreeMap<String, SourceException>,
}

/// One approved suppression. Every entry must match at least one suppression.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LintException {
    /// The file, relative to the workspace root, for example `crates/x/src/lib.rs`.
    pub path: String,
    /// The lint as written in the attribute (`clippy::too_many_lines`, `dead_code`), or
    /// `coverage` for `#[coverage(off)]`. `todo!`, `unimplemented!` and `dbg!` are the
    /// clippy lints `clippy::todo`, `clippy::unimplemented` and `clippy::dbg_macro`.
    pub lint: String,
    /// The approving ADR's number; `doc/adr/<adr>-*.md` must exist.
    pub adr: String,
}

/// One approved package source.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceException {
    /// The approving ADR's number, for example `0042`; `doc/adr/<adr>-*.md` must exist.
    pub adr: String,
}

/// Parses policy text: the schema version first, then every field strictly.
pub fn parse(text: &str) -> Result<Policy, String> {
    let table: toml::Table = text.parse().map_err(|e| format!("not valid TOML: {e}"))?;
    match table.get("schema") {
        Some(toml::Value::Integer(SCHEMA)) => {}
        Some(other) => {
            return Err(format!(
                "unsupported schema version {other}; this checker reads schema = {SCHEMA}"
            ));
        }
        None => return Err(format!("missing `schema = {SCHEMA}`")),
    }
    toml::from_str(text).map_err(|e| e.message().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str =
        "schema = 1\n[workspace]\nproduction = [\"a\"]\n[dependencies.normal]\na = []\n";

    #[test]
    fn should_parse_policy_when_text_is_minimal() {
        // given
        let text = MINIMAL;

        // when
        let policy = parse(text).unwrap();

        // then
        assert_eq!(policy.workspace.production, Some(vec!["a".to_owned()]));
        assert_eq!(policy.workspace.test_only, None);
        assert_eq!(policy.external, External::default());
        assert_eq!(policy.planes, Planes::default());
        assert_eq!(policy.purity, Purity::default());
    }

    #[test]
    fn should_find_plane_and_vocabulary_when_planes_table_is_given() {
        // given
        let text = format!(
            "{MINIMAL}[planes]\ncontrol = [\"c\"]\ndata = [\"d\"]\nshared = [\"a\"]\n[planes.vocabulary]\ncontrol = [\"Journal\"]\n[purity]\npure = [\"a\"]\n"
        );

        // when
        let policy = parse(&text).unwrap();

        // then
        assert!(policy.planes.declared());
        assert_eq!(policy.planes.of("d"), Some(Plane::Data));
        assert_eq!(policy.planes.of("x"), None);
        assert_eq!(policy.planes.vocabulary.of(Plane::Control), ["Journal"]);
        assert_eq!(policy.purity.pure, ["a"]);
        assert_eq!(Plane::Shared.forbidden(), [Plane::Control, Plane::Data]);
    }

    #[test]
    fn should_reject_planes_when_a_key_is_unknown() {
        // given
        let text = format!("{MINIMAL}[planes]\ncontrl = [\"a\"]\n");

        // when
        let err = parse(&text).unwrap_err();

        // then
        assert!(err.contains("unknown field `contrl`"), "{err}");
    }

    #[test]
    fn should_reject_policy_when_schema_version_differs() {
        // given
        let text = MINIMAL.replace("schema = 1", "schema = 2");

        // when
        let err = parse(&text).unwrap_err();

        // then
        assert!(err.contains("unsupported schema version 2"), "{err}");
    }

    #[test]
    fn should_reject_policy_when_schema_is_missing() {
        // given
        let text = MINIMAL.replace("schema = 1\n", "");

        // when
        let err = parse(&text).unwrap_err();

        // then
        assert!(err.contains("missing `schema = 1`"), "{err}");
    }

    #[test]
    fn should_reject_policy_when_a_field_is_unknown() {
        // given
        let text = MINIMAL.replace("production", "prod");

        // when
        let err = parse(&text).unwrap_err();

        // then
        assert!(err.contains("unknown field `prod`"), "{err}");
    }

    #[test]
    fn should_parse_feature_lists_when_external_tables_are_present() {
        // given
        let text = format!(
            "{MINIMAL}[external.normal]\na = {{ tokio = [\"sync\"], log = \"*\" }}\n[external.dev]\na = {{ tempfile = [\"default\"] }}\n"
        );

        // when
        let policy = parse(&text).unwrap();

        // then
        let a = &policy.external.normal["a"];
        assert_eq!(a["tokio"], Features::Only(vec!["sync".to_owned()]));
        assert_eq!(a["log"], Features::Any);
        assert_eq!(
            policy.external.dev["a"]["tempfile"],
            Features::Only(vec!["default".to_owned()])
        );
    }

    #[test]
    fn should_reject_features_when_value_is_another_string() {
        // given
        let text = format!("{MINIMAL}[external.normal]\na = {{ tokio = \"sync\" }}\n");

        // when
        let err = parse(&text).unwrap_err();

        // then
        assert!(err.contains("\"*\" or a list of feature names"), "{err}");
    }

    #[test]
    fn should_parse_lint_exceptions_when_entries_are_complete() {
        // given
        let text = format!(
            "{MINIMAL}[[exceptions.lints]]\npath = \"crates/a/src/lib.rs\"\nlint = \"dead_code\"\nadr = \"0042\"\n"
        );

        // when
        let policy = parse(&text).unwrap();

        // then
        assert_eq!(
            policy.exceptions.lints,
            [LintException {
                path: "crates/a/src/lib.rs".to_owned(),
                lint: "dead_code".to_owned(),
                adr: "0042".to_owned(),
            }]
        );
    }

    #[test]
    fn should_reject_exceptions_when_table_is_not_empty() {
        // given
        let text = format!("{MINIMAL}[exceptions]\nanything = 1\n");

        // when
        let err = parse(&text).unwrap_err();

        // then
        assert!(err.contains("unknown field `anything`"), "{err}");
    }
}
