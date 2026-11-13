//! `ARCH-operator-actor` (RT-03, KER-10F): only the operator socket builds an operator actor.
//!
//! An `Actor::Operator { uid }` is what lets a command through the rules that name an operator
//! (only an operator resets a breaker, forces a drain, withdraws an image or resolves a
//! quarantine), and its uid is what the journal records. The socket builds it from the peer
//! credentials the kernel gives for the connection, so the uid is the person's who asked. Any
//! other production code that names the variant could hand a policy, the start-up use case or a
//! value a client sent an operator's authority.
//!
//! So production code of a production crate names `mayfly_control_domain::Actor::Operator`
//! (by any path or `use` the resolver follows, or written `Actor::Operator`) only in
//! `mayfly-composition`'s [`SOCKET`] file, or in [`DEFINER`], which declares the variant and
//! reads it in `decide`. Patterns are names too: a match on the variant outside the domain is
//! refused as well, which costs nothing today and keeps the rule a plain "names" check. Test
//! code and test-only crates are free.
//!
//! Limits, as with every name-based rule here: a path the resolver cannot follow and that is
//! not written `..Actor::Operator` (a glob import of the enum's variants, `use Actor::*;
//! Operator { .. }`, or a type alias of `Actor` used as `Alias::Operator` across a re-export
//! the resolver does not see) is not caught, and neither is a value built elsewhere and passed
//! in (the socket's caller cannot reach one: `Actor` has no constructor but its variants).
//! Pinned by
//! `should_report_operator_actor_when_a_policy_builds_one` and
//! `should_pass_when_only_the_socket_or_the_domain_names_it`.

use super::super::policy::Policy;
use super::super::report::{Diagnostic, Rule};
use super::{Context, Facts, Reference};

/// The crate that declares `Actor` and decides on it.
pub const DEFINER: &str = "mayfly-control-domain";

/// The one production file outside [`DEFINER`] that may build an operator actor.
pub const SOCKET: (&str, &str) = ("mayfly-composition", "src/operator/server.rs");

fn names_operator(reference: &Reference) -> bool {
    let resolved = reference.resolved.iter().any(|candidate| {
        candidate
            .first()
            .is_some_and(|c| c == "mayfly_control_domain")
            && candidate.ends_with(&["Actor".to_owned(), "Operator".to_owned()])
    });
    let mut segments = reference.written.rsplit("::");
    let written = segments.next() == Some("Operator") && segments.next() == Some("Actor");
    resolved || written
}

/// The diagnostics of this rule for one package.
pub fn check(facts: &Facts, policy: &Policy) -> Vec<Diagnostic> {
    let production = policy
        .workspace
        .production
        .as_ref()
        .is_some_and(|p| p.contains(&facts.package));
    if !production || facts.package == DEFINER {
        return Vec::new();
    }
    let socket = (facts.package == SOCKET.0).then(|| format!("{}/{}", facts.dir, SOCKET.1));
    let mut out: Vec<Diagnostic> = facts
        .references
        .iter()
        .filter(|r| r.site.context == Context::Production && names_operator(r))
        .filter(|r| socket.as_deref() != Some(r.site.file.as_str()))
        .map(|r| {
            Diagnostic::new(
                Rule::OperatorActor,
                format!(
                    "{} names Actor::Operator outside the operator socket ({}/{}): an operator actor is built only from a connection's peer credentials",
                    facts.package, SOCKET.0, SOCKET.1
                ),
                r.site.at(),
            )
        })
        .collect();
    out.sort();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::super::testing::{facts_for, policy, rendered};
    use super::*;

    #[test]
    fn should_report_operator_actor_when_a_policy_builds_one() {
        // given: a control-plane use case that promotes itself to an operator
        let control = "use mayfly_control_domain::Actor;\npub fn act() -> Actor {\n    Actor::Operator { uid: 0 }\n}\n";
        let renamed = "use mayfly_control_domain::Actor as Who;\npub fn act() -> Who {\n    Who::Operator { uid: 0 }\n}\n";

        // when
        let direct = rendered(&check(
            &facts_for("mayfly-control", &[("src/lib.rs", control)]),
            &policy(),
        ));
        let aliased = rendered(&check(
            &facts_for("mayfly-control", &[("src/lib.rs", renamed)]),
            &policy(),
        ));

        // then
        assert_eq!(direct.len(), 1, "{direct:?}");
        assert!(
            direct[0].starts_with("ARCH-operator-actor: mayfly-control names Actor::Operator"),
            "{direct:?}"
        );
        assert!(
            direct[0].ends_with("(at crates/mayfly-control/src/lib.rs:3)"),
            "{direct:?}"
        );
        assert_eq!(aliased.len(), 1, "{aliased:?}");
    }

    #[test]
    fn should_pass_when_only_the_socket_or_the_domain_names_it() {
        // given
        let socket = "pub mod operator { pub mod server; }\n";
        let server = "use mayfly_control_domain::Actor;\npub fn peer(uid: u32) -> Actor {\n    Actor::Operator { uid }\n}\n";
        let domain = "pub enum Actor { Operator { uid: u32 }, Startup }\npub fn uid(a: Actor) -> u32 { match a { Actor::Operator { uid } => uid, Actor::Startup => 0 } }\n";
        let lookalike =
            "pub enum MyActor { Operator }\npub fn f() -> MyActor { MyActor::Operator }\n";
        let tests = "#[cfg(test)]\nmod tests {\n    const OP: mayfly_control_domain::Actor = mayfly_control_domain::Actor::Operator { uid: 1 };\n}\n";

        // when
        let composition = check(
            &facts_for(
                "mayfly-composition",
                &[("src/lib.rs", socket), ("src/operator/server.rs", server)],
            ),
            &policy(),
        );
        let in_domain = check(
            &facts_for("mayfly-control-domain", &[("src/lib.rs", domain)]),
            &policy(),
        );
        let in_tests = check(
            &facts_for("mayfly-control", &[("src/lib.rs", tests)]),
            &policy(),
        );

        let other_enum = check(
            &facts_for("mayfly-control", &[("src/lib.rs", lookalike)]),
            &policy(),
        );

        // then
        assert_eq!(other_enum, Vec::new());
        assert_eq!(composition, Vec::new());
        assert_eq!(in_domain, Vec::new());
        assert_eq!(in_tests, Vec::new());
    }
}
