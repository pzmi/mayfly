//! `ARCH-hidden-seam` (ported from site-preview): composition's `#[doc(hidden)]` items are
//! seams for the conformance drivers (a fault injected into the runtime, a resource the driver
//! observes), public only because the drivers live in another crate. No other crate may name
//! them: the CLI must not reach a failure no user can cause, and a test elsewhere must go
//! through the public API. Composition's own code and tests define and pin them.
//!
//! Items are matched by name: a method call is not type-resolved, so a call or a
//! path whose last segment is a hidden item's name counts. Composition's hidden names are
//! long and specific; a clash is resolved by renaming, never by an exemption.

use std::collections::BTreeSet;

use super::super::report::{Diagnostic, Rule};
use super::Facts;

/// The crate whose hidden items are seams.
pub const DEFINER: &str = "mayfly-composition";

/// The crates allowed to name them, besides the definer.
pub const USERS: &[&str] = &["mayfly-conformance"];

/// One diagnostic per place outside [`DEFINER`] and [`USERS`] that names a hidden item of
/// [`DEFINER`], test code included.
pub fn check(every: &[Facts]) -> Vec<Diagnostic> {
    let hidden: BTreeSet<&str> = every
        .iter()
        .filter(|facts| facts.package == DEFINER)
        .flat_map(|facts| facts.hidden.iter().map(|item| item.name.as_str()))
        .collect();
    if hidden.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for facts in every {
        if facts.package == DEFINER || USERS.contains(&facts.package.as_str()) {
            continue;
        }
        let calls = facts
            .method_calls
            .iter()
            .map(|call| (&call.site, call.name.as_str()));
        let paths = facts.references.iter().filter_map(|reference| {
            reference
                .written
                .rsplit("::")
                .next()
                .map(|last| (&reference.site, last))
        });
        for (site, name) in calls.chain(paths) {
            if hidden.contains(name) {
                out.push(Diagnostic::new(
                    Rule::HiddenSeam,
                    format!(
                        "{} names `{name}`, a #[doc(hidden)] seam of {DEFINER} that only {} may use: go through the public API",
                        facts.package,
                        USERS.join(", ")
                    ),
                    site.at(),
                ));
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::super::testing::{facts_for, rendered};
    use super::*;

    const COMPOSITION: &str = "pub struct Server;\nimpl Server {\n    #[doc(hidden)]\n    pub fn start_with_seam() -> Self { Server }\n    #[cfg_attr(all(), doc(hidden))]\n    pub const fn owns_seam(&self) -> bool { false }\n    pub fn start() -> Self { Server }\n}\n";

    fn every(package: &str, text: &str) -> Vec<Facts> {
        vec![
            facts_for(DEFINER, &[("src/lib.rs", COMPOSITION)]),
            facts_for(package, &[("src/lib.rs", text)]),
        ]
    }

    #[test]
    fn should_report_hidden_seam_when_another_crate_names_it() {
        // given
        let cli = "pub fn run() -> bool {\n    mayfly_composition::Server::start_with_seam().owns_seam()\n}\n";

        // when
        let got = rendered(&check(&every("mayfly-cli", cli)));

        // then
        assert_eq!(got.len(), 2, "{got:?}");
        assert!(
            got.iter()
                .all(|line| line.starts_with("ARCH-hidden-seam: mayfly-cli names `"))
        );
        assert!(
            got[0].contains("`owns_seam`") || got[1].contains("`owns_seam`"),
            "{got:?}"
        );
        assert!(
            got.iter().any(|line| line.contains("`start_with_seam`")),
            "{got:?}"
        );
        assert!(
            got.iter()
                .all(|line| line.ends_with("(at crates/mayfly-cli/src/lib.rs:2)"))
        );
    }

    #[test]
    fn should_pass_when_only_conformance_or_public_api_names_it() {
        // given
        let conformance = "pub fn drive() -> bool {\n    mayfly_composition::Server::start_with_seam().owns_seam()\n}\n";
        let cli = "pub fn run() { let _ = mayfly_composition::Server::start(); }\n";

        // when
        let from_conformance = check(&every("mayfly-conformance", conformance));
        let from_cli = check(&every("mayfly-cli", cli));

        // then
        assert_eq!(from_conformance, Vec::new());
        assert_eq!(from_cli, Vec::new());
    }
}
