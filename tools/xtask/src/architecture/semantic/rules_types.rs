//! Rules over declarations rather than paths (foundation.md section 3.3): secrets never
//! serialise or print (`ARCH-secret-serialize`, plan section 13, D13), and every control-plane
//! aggregate is registered where the harnesses walk (`ARCH-aggregate-registered`).
//!
//! Both read production code of every workspace package at once: a type that holds a
//! `Secret` may be declared in one crate and serialised in another.
//!
//! A holder is known by its item path (its crate, module and name), never by its simple name:
//! a field's type is resolved through `use` items, globs, re-exports and aliases of the
//! workspace crates ([`super::TypeRef`]), so a `World` holding a secret in one crate says
//! nothing about a `World` in another. The wrapper itself is the one name matched as written:
//! a path whose last segment is `Secret`, however it resolves, holds one (over-approximation
//! on purpose, so a re-implemented or renamed import of the wrapper still counts).
//!
//! What makes a type a holder (pinned by
//! `should_report_serialize_when_a_holder_is_reached_through_alias_generic_or_dyn`): a field,
//! a generic argument, a `dyn`/`impl` trait's arguments, a generic parameter's bound or
//! default, naming the wrapper or another holder; a type alias whose target or generics do.
//! The set is built from production declarations only: a test type never seeds it and never
//! joins it, so it never makes a production type a holder (pinned by
//! `should_not_report_serialize_when_a_same_named_type_elsewhere_or_a_test_type_holds_a_secret`).
//! Test code is not checked either: it cannot serialise a secret while the wrapper implements
//! no `Serialize`, which `mayfly-domain`'s `tests/secret.rs` asserts at compile time.

use std::collections::BTreeSet;

use super::super::report::{Diagnostic, Rule};
use super::{Context, Facts, Site, TypeRef};

/// The redacting wrapper's name (plan section 13).
const SECRET: &str = "Secret";

/// Traits that would write a secret out: serde's `Serialize`, and the formatting traits other
/// than the one redacting impl beside the type.
const SERIALIZE: &str = "Serialize";
const FORMAT: &[&str] = &["Debug", "Display"];

/// The crate whose `impl Aggregate for X` items must each be visited by `ControlAggregates`,
/// and the trait's name.
pub const AGGREGATES: (&str, &str) = ("mayfly-control-domain", "Aggregate");

/// The diagnostics of these rules over every package.
pub fn check(every: &[Facts]) -> Vec<Diagnostic> {
    let mut out = secrets(every);
    out.extend(aggregates(every));
    out
}

/// The item paths of every production type or alias that is, or transitively holds, a
/// `Secret`.
fn secret_holders(every: &[Facts]) -> BTreeSet<Vec<String>> {
    let production = |site: &Site| site.context == Context::Production;
    let declared: Vec<(&Vec<String>, &[TypeRef])> = every
        .iter()
        .flat_map(|f| {
            f.types
                .iter()
                .filter(|t| production(&t.site))
                .map(|t| (&t.path, t.refs.as_slice()))
                .chain(
                    f.aliases
                        .iter()
                        .filter(|a| production(&a.site))
                        .map(|a| (&a.path, a.refs.as_slice())),
                )
        })
        .collect();
    let mut holders: BTreeSet<Vec<String>> = every
        .iter()
        .flat_map(|f| &f.types)
        .filter(|t| production(&t.site) && t.name == SECRET)
        .map(|t| t.path.clone())
        .collect();
    loop {
        let before = holders.len();
        for &(path, refs) in &declared {
            if !holders.contains(path) && refs.iter().any(|r| holds(r, &holders)) {
                holders.insert(path.clone());
            }
        }
        if holders.len() == before {
            return holders;
        }
    }
}

/// Whether `type_ref` names the wrapper (by its last segment, written or resolved) or one of
/// `holders` (by item path). A path the scopes cannot resolve (a lone name no `use`, item or
/// glob binds, as a macro-generated import would leave it) fails closed: it holds when its
/// name is the simple name of any production holder (pinned by
/// `should_report_serialize_when_an_unbound_name_or_a_glob_reaches_a_holder`).
fn holds(type_ref: &TypeRef, holders: &BTreeSet<Vec<String>>) -> bool {
    if type_ref.last == SECRET {
        return true;
    }
    if type_ref.resolved.is_empty() {
        return holders
            .iter()
            .any(|h| h.last().is_some_and(|l| *l == type_ref.last));
    }
    type_ref
        .resolved
        .iter()
        .any(|c| holders.contains(c) || c.last().is_some_and(|l| l == SECRET))
}

fn secrets(every: &[Facts]) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    let holders = secret_holders(every);
    let defining: BTreeSet<&str> = every
        .iter()
        .flat_map(|f| &f.types)
        .filter(|t| t.name == SECRET)
        .map(|t| t.site.file.as_str())
        .collect();
    for facts in every {
        for decl in facts
            .types
            .iter()
            .filter(|t| t.site.context == Context::Production)
        {
            let held = decl.name == SECRET || holders.contains(&decl.path);
            if held && decl.derives.iter().any(|d| d == SERIALIZE) {
                out.push(Diagnostic::new(
                    Rule::SecretSerialize,
                    format!(
                        "{} derives Serialize for `{}`, which {}: a secret is never serialised; carry a SecretHandle instead (D13)",
                        facts.package,
                        decl.name,
                        holds_text(&decl.name)
                    ),
                    decl.site.at(),
                ));
            }
            if decl.name == SECRET && decl.derives.iter().any(|d| FORMAT.contains(&d.as_str())) {
                out.push(Diagnostic::new(
                    Rule::SecretSerialize,
                    format!(
                        "{} derives a formatting trait for `Secret`: its Debug and Display are hand-written and redact (D13)",
                        facts.package
                    ),
                    decl.site.at(),
                ));
            }
        }
        for imp in facts
            .impls
            .iter()
            .filter(|i| i.site.context == Context::Production)
        {
            let held = imp.self_name == SECRET || imp.self_refs.iter().any(|r| holds(r, &holders));
            if held && imp.trait_name == SERIALIZE {
                out.push(Diagnostic::new(
                    Rule::SecretSerialize,
                    format!(
                        "{} implements Serialize for `{}`, which {}: a secret is never serialised (D13)",
                        facts.package,
                        imp.self_name,
                        holds_text(&imp.self_name)
                    ),
                    imp.site.at(),
                ));
            }
            if imp.self_name == SECRET
                && FORMAT.contains(&imp.trait_name.as_str())
                && !defining.contains(imp.site.file.as_str())
            {
                out.push(Diagnostic::new(
                    Rule::SecretSerialize,
                    format!(
                        "{} implements {} for `Secret` outside the file that defines it: only the redacting impl beside the type may format a secret (D13)",
                        facts.package, imp.trait_name
                    ),
                    imp.site.at(),
                ));
            }
        }
    }
    out
}

fn holds_text(name: &str) -> &'static str {
    if name == SECRET {
        "is the secret wrapper"
    } else {
        "holds a Secret"
    }
}

/// `ARCH-aggregate-registered`: every `impl Aggregate for X` in the control domain's
/// production code is visited in `ControlAggregates::accept`; one that is not would be
/// skipped by the golden and determinism harnesses without a failure.
fn aggregates(every: &[Facts]) -> Vec<Diagnostic> {
    let (package, trait_name) = AGGREGATES;
    let Some(facts) = every.iter().find(|f| f.package == package) else {
        return Vec::new();
    };
    let visited: BTreeSet<&str> = facts.registry_visits.iter().map(String::as_str).collect();
    facts
        .impls
        .iter()
        .filter(|i| i.site.context == Context::Production && i.trait_name == trait_name)
        .filter(|i| !visited.contains(i.self_name.as_str()))
        .map(|i| {
            Diagnostic::new(
                Rule::AggregateRegistered,
                format!(
                    "{package} implements Aggregate for `{0}`, but `ControlAggregates::accept` has no `visitor.visit::<{0}>()`: the golden and determinism harnesses would skip it",
                    i.self_name
                ),
                i.site.at(),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::super::testing::{facts_for, package_with_binary, rendered, workspace};
    use super::*;

    /// The `file:line` of each rendered diagnostic.
    fn locations(got: &[String]) -> Vec<&str> {
        got.iter()
            .filter_map(|d| {
                d.rsplit_once(" (at ")
                    .map(|(_, at)| at.trim_end_matches(')'))
            })
            .collect()
    }

    #[test]
    fn should_not_report_serialize_when_a_same_named_type_elsewhere_or_a_test_type_holds_a_secret()
    {
        // given
        let domain = "pub struct Secret<T>(T);\n#[derive(serde::Serialize)]\npub struct Launch { pub jit: Secret<String> }\nstruct Held { config: Secret<String> }\n#[derive(serde::Serialize)]\npub struct World { pub hosts: u32 }\n#[cfg(test)]\nmod tests {\n    pub struct World { pub key: super::Secret<String> }\n}\n";
        let protocol = "#[derive(serde::Serialize)]\npub struct Launch { pub name: String }\n#[derive(serde::Serialize)]\npub struct World { pub hosts: u32 }\n#[derive(serde::Serialize)]\npub struct Held { pub name: String }\n#[derive(serde::Serialize)]\npub struct Envelope { pub launch: Launch, pub world: World, pub held: Held }\nimpl serde::Serialize for Wrapper<Held> {}\n";

        // when
        let got = rendered(&check(&workspace(&[
            ("mayfly-domain", domain),
            ("mayfly-protocol", protocol),
        ])));

        // then
        assert_eq!(
            locations(&got),
            ["crates/mayfly-domain/src/lib.rs:3"],
            "{got:?}"
        );
        assert!(
            got[0].contains("mayfly-domain derives Serialize for `Launch`"),
            "{got:?}"
        );
    }

    #[test]
    fn should_report_serialize_when_a_holder_is_reached_through_alias_generic_or_dyn() {
        // given
        let domain = "pub struct Secret<T>(T);\npub struct Envelope<T> { pub body: T }\nmod jit {\n    pub struct Jit { config: crate::Secret<String> }\n    pub type JitConfig = Option<crate::Secret<String>>;\n}\npub use jit::{Jit, JitConfig};\npub type Boxed = Box<dyn Fn() -> Jit>;\n";
        let protocol = [
            "use mayfly_domain::{Envelope, Jit};",
            "#[derive(serde::Serialize)]",
            "pub struct ThroughAlias { pub jit: mayfly_domain::JitConfig }",
            "#[derive(serde::Serialize)]",
            "pub struct Generic { pub jit: Envelope<mayfly_domain::Jit> }",
            "#[derive(serde::Serialize)]",
            "pub struct Defaulted<J = mayfly_domain::Jit> { pub jit: Vec<J> }",
            "#[derive(serde::Serialize)]",
            "pub struct Dynamic { pub make: Box<dyn Fn() -> mayfly_domain::Jit> }",
            "#[derive(serde::Serialize)]",
            "pub struct AliasedDyn { pub make: mayfly_domain::Boxed }",
            "type Local = Envelope<mayfly_domain::Jit>;",
            "impl serde::Serialize for Local {}",
            "impl serde::Serialize for Envelope<mayfly_domain::Jit> {}",
            "#[derive(serde::Serialize)]",
            "pub struct Plain<T> { pub body: Envelope<T> }",
            "impl<Jit> serde::Serialize for Envelope<Jit> {}",
        ]
        .join("\n");

        // when
        let got = rendered(&check(&workspace(&[
            ("mayfly-domain", domain),
            ("mayfly-protocol", protocol.as_str()),
        ])));

        // then
        let lines: Vec<&str> = locations(&got)
            .into_iter()
            .map(|at| at.trim_start_matches("crates/mayfly-protocol/src/lib.rs:"))
            .collect();
        assert_eq!(lines, ["3", "5", "7", "9", "11", "13", "14"], "{got:?}");
    }

    #[test]
    fn should_report_serialize_when_an_unbound_name_or_a_glob_reaches_a_holder() {
        // given
        let domain = "pub struct Secret<T>(T);\nmod jit {\n    pub struct Jit { config: crate::Secret<String> }\n}\npub use jit::*;\n#[cfg(test)]\nmod tests {\n    pub struct World { pub key: super::Secret<String> }\n}\n";
        let protocol = [
            "#[derive(serde::Serialize)]",
            "pub struct Unbound { pub jit: Jit }",
            "#[derive(serde::Serialize)]",
            "pub struct ThroughGlob { pub jit: mayfly_domain::Jit }",
            "#[derive(serde::Serialize)]",
            "pub struct UnboundTestName { pub world: World }",
        ]
        .join("\n");

        // when
        let got = rendered(&check(&workspace(&[
            ("mayfly-domain", domain),
            ("mayfly-protocol", protocol.as_str()),
        ])));

        // then
        assert_eq!(
            locations(&got),
            [
                "crates/mayfly-protocol/src/lib.rs:2",
                "crates/mayfly-protocol/src/lib.rs:4"
            ],
            "{got:?}"
        );
    }

    #[test]
    fn should_report_only_the_library_holder_when_a_binary_declares_a_same_named_type() {
        // given
        let lib = "pub struct Secret<T>(T);\npub struct World { pub key: Secret<String> }\n";
        let bin = "#[derive(serde::Serialize)]\npub struct World { pub hosts: u32 }\n#[derive(serde::Serialize)]\npub struct Launch { pub world: World }\n#[derive(serde::Serialize)]\npub struct Real { pub world: mayfly_cli::World }\n";

        // when
        let got = rendered(&check(&[package_with_binary("mayfly-cli", lib, bin)]));

        // then
        assert_eq!(
            locations(&got),
            ["crates/mayfly-cli/src/main.rs:6"],
            "{got:?}"
        );
    }

    #[test]
    fn should_report_serialize_when_a_type_holds_a_secret_through_another() {
        // given
        let source = "pub struct Secret<T>(T);\npub struct Jit { pub config: Secret<String> }\n#[derive(serde::Serialize, Debug)]\npub struct Launch { pub jit: Jit, pub name: String }\n#[derive(serde::Serialize)]\npub struct Plain { pub name: String }\nimpl serde::Serialize for Jit {}\n";

        // when
        let got = rendered(&check(&[facts_for(
            "mayfly-data",
            &[("src/lib.rs", source)],
        )]));

        // then
        let lines: Vec<&str> = got
            .iter()
            .map(|d| d.rsplit(':').next().unwrap_or(""))
            .collect();
        assert_eq!(lines, ["4)", "7)"], "{got:?}");
    }

    #[test]
    fn should_report_formatting_when_secret_derives_debug_or_is_formatted_elsewhere() {
        // given
        let domain = facts_for(
            "mayfly-domain",
            &[(
                "src/lib.rs",
                "#[derive(Clone, Debug)]\npub struct Secret<T>(T);\nimpl<T> std::fmt::Display for Secret<T> {}\n",
            )],
        );
        let other = facts_for(
            "mayfly-data",
            &[("src/lib.rs", "impl std::fmt::Debug for Secret<u8> {}\n")],
        );

        // when
        let got = rendered(&check(&[domain, other]));

        // then
        assert_eq!(got.len(), 2, "{got:?}");
        assert!(got[0].contains("derives a formatting trait"), "{got:?}");
        assert!(
            got[1].contains("outside the file that defines it"),
            "{got:?}"
        );
    }

    #[test]
    fn should_report_aggregate_when_registry_does_not_visit_it() {
        // given
        let source = "pub struct Attempt;\npub struct Host;\nimpl Aggregate for Attempt {}\nimpl Aggregate for Host {}\npub struct ControlAggregates;\nimpl AggregateRegistry for ControlAggregates {\n    fn accept<V: AggregateVisitor>(visitor: &mut V) {\n        visitor.visit::<Attempt>();\n    }\n}\n#[cfg(test)]\nmod tests {\n    struct Fake;\n    impl Aggregate for Fake {}\n}\n";

        // when
        let got = rendered(&check(&[facts_for(
            "mayfly-control-domain",
            &[("src/lib.rs", source)],
        )]));

        // then
        assert_eq!(got.len(), 1, "{got:?}");
        assert!(
            got[0].contains("`Host`") && got[0].ends_with("src/lib.rs:4)"),
            "{got:?}"
        );
    }
}
