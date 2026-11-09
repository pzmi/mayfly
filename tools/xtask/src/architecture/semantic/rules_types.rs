//! Rules over declarations rather than paths (foundation.md section 3.3): secrets never
//! serialise or print (`ARCH-secret-serialize`, plan section 13, D13), and every control-plane
//! aggregate is registered where the harnesses walk (`ARCH-aggregate-registered`).
//!
//! Both read production code of every workspace package at once: a type that holds a
//! `Secret` may be declared in one crate and serialised in another. Matching is by name
//! (types are not resolved), which over-approximates: a type named `Secret` anywhere counts.

use std::collections::BTreeSet;

use super::super::report::{Diagnostic, Rule};
use super::{Context, Facts};

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

/// The names of every type that is, or transitively holds, a `Secret`.
fn secret_holders(every: &[Facts]) -> BTreeSet<String> {
    let mut holders = BTreeSet::from([SECRET.to_owned()]);
    loop {
        let before = holders.len();
        for decl in every.iter().flat_map(|f| &f.types) {
            if decl.fields.iter().any(|f| holders.contains(f)) {
                holders.insert(decl.name.clone());
            }
        }
        if holders.len() == before {
            return holders;
        }
    }
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
            let holds = holders.contains(&decl.name);
            if holds && decl.derives.iter().any(|d| d == SERIALIZE) {
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
            let holds = imp.self_idents.iter().any(|i| holders.contains(i));
            if holds && imp.trait_name == SERIALIZE {
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
    use super::super::testing::{facts_for, rendered};
    use super::*;

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
