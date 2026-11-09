//! `ARCH-exhaustive-transition` (foundation.md sections 3.3 and 6.2, plan section 6 rule 8):
//! every pair of a state machine's states and stimuli is a defined transition or a defined
//! refusal, so a match in a transition function never falls through a catch-all arm. A new
//! state or stimulus then fails to compile until each transition function says what it does
//! with it, instead of being swallowed by `_ => ...`.
//!
//! In the production code of every `[purity] pure` crate, inside a function named `decide`,
//! `evolve` or `step`, or starting with `decide_`, `evolve_` or `step_` (free functions and
//! associated functions alike, closures inside them included), a `match` arm is refused when
//! its pattern is, or has as an alternative or as a direct element of a tuple pattern:
//!
//! - the wildcard `_`;
//! - a bare binding (`other =>`, `_x =>`, also with a guard), which matches everything;
//! - the rest pattern `..` among a tuple's elements (`(State::Idle, ..)`).
//!
//! Wildcards inside a variant's payload (`Some(_)`, `Phase::Backoff { .. }`) are allowed: they
//! ignore data, not states or stimuli. Matches written inside macros are not read (the macro
//! that generates event sets uses if-chains, not match arms). Test-only items (`#[cfg(test)]`,
//! `#[test]`, a `#[cfg(test)] mod name;` file) are not checked.
//!
//! Pinned by `should_report_catch_all_arms_when_a_transition_function_matches` and
//! `should_ignore_other_functions_and_payload_wildcards_when_matching`, and by the negative
//! fixture `wildcard-transition-arm`.

use std::collections::BTreeSet;
use std::path::Path;

use syn::visit::{self, Visit};
use syn::{ExprMatch, ImplItemFn, Item, ItemFn, ItemImpl, ItemMod, Pat};

use super::super::policy::Policy;
use super::super::report::{Diagnostic, Rule};
use super::{Facts, is_test_only};

/// The names (and, followed by `_`, the prefixes) of transition functions.
pub const TRANSITION_FNS: &[&str] = &["decide", "evolve", "step"];

/// The diagnostics of this rule for one package.
pub fn check(facts: &Facts, policy: &Policy) -> Vec<Diagnostic> {
    if !policy.purity.pure.contains(&facts.package) {
        return Vec::new();
    }
    let src = if facts.dir.is_empty() {
        "src/".to_owned()
    } else {
        format!("{}/src/", facts.dir)
    };
    let parsed: Vec<(&String, syn::File)> = facts
        .sources
        .iter()
        .filter(|(path, _)| path.starts_with(&src))
        .filter_map(|(path, text)| syn::parse_file(text).ok().map(|file| (path, file)))
        .collect();
    let mut test_files = BTreeSet::new();
    for (path, file) in &parsed {
        test_module_files(&file.items, &module_dir(path), &mut test_files);
    }
    let mut out = Vec::new();
    for (path, file) in parsed.iter().filter(|(p, _)| !test_files.contains(*p)) {
        let mut scan = Scan {
            package: &facts.package,
            file: path,
            transition: Vec::new(),
            out: &mut out,
        };
        scan.visit_file(file);
    }
    out
}

/// The directory a file's child modules live in: its own for `lib.rs`, `main.rs` and
/// `mod.rs`, else one named after it.
fn module_dir(path: &str) -> String {
    let file = Path::new(path);
    let parent = file
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    match file.file_stem().and_then(|s| s.to_str()) {
        Some("lib" | "main" | "mod") | None => parent,
        Some(stem) => format!("{parent}/{stem}"),
    }
}

/// The files of `#[cfg(test)] mod name;` declarations, through inline modules.
fn test_module_files(items: &[Item], dir: &str, out: &mut BTreeSet<String>) {
    for item in items {
        let Item::Mod(module) = item else {
            continue;
        };
        let name = module.ident.to_string();
        match &module.content {
            None if is_test_only(&module.attrs) => {
                out.insert(format!("{dir}/{name}.rs"));
                out.insert(format!("{dir}/{name}/mod.rs"));
            }
            None => {}
            Some((_, inner)) => test_module_files(inner, &format!("{dir}/{name}"), out),
        }
    }
}

fn is_transition(name: &str) -> bool {
    let name = name.strip_prefix("r#").unwrap_or(name);
    TRANSITION_FNS.iter().any(|t| {
        name == *t
            || name
                .strip_prefix(t)
                .is_some_and(|rest| rest.starts_with('_'))
    })
}

/// A pattern that matches every value where a state or stimulus is expected: what it is
/// and its line.
fn catch_all(pat: &Pat) -> Option<(&'static str, usize)> {
    match pat {
        Pat::Wild(wild) => Some(("`_`", wild.underscore_token.span.start().line)),
        Pat::Ident(binding) => match &binding.subpat {
            Some((_, sub)) => catch_all(sub),
            None => {
                let name = binding.ident.to_string();
                let bare = name.starts_with(|c: char| c.is_ascii_lowercase() || c == '_');
                bare.then(|| ("a bare binding", binding.ident.span().start().line))
            }
        },
        Pat::Or(or) => or.cases.iter().find_map(catch_all),
        Pat::Guard(guarded) => catch_all(&guarded.pat),
        Pat::Paren(paren) => catch_all(&paren.pat),
        Pat::Reference(reference) => catch_all(&reference.pat),
        Pat::Tuple(tuple) => tuple.elems.iter().find_map(|element| match element {
            Pat::Rest(rest) => Some(("`..`", rest.dot2_token.spans[0].start().line)),
            other => catch_all(other),
        }),
        _ => None,
    }
}

struct Scan<'a> {
    package: &'a str,
    file: &'a str,
    /// The transition functions being visited, innermost last.
    transition: Vec<String>,
    out: &'a mut Vec<Diagnostic>,
}

impl Scan<'_> {
    fn within(&mut self, name: &syn::Ident, visit: impl FnOnce(&mut Self)) {
        let name = name.to_string();
        if is_transition(&name) {
            self.transition.push(name);
            visit(self);
            self.transition.pop();
        } else {
            visit(self);
        }
    }
}

impl<'ast> Visit<'ast> for Scan<'_> {
    fn visit_item_mod(&mut self, module: &'ast ItemMod) {
        if !is_test_only(&module.attrs) {
            visit::visit_item_mod(self, module);
        }
    }

    fn visit_item_impl(&mut self, item: &'ast ItemImpl) {
        if !is_test_only(&item.attrs) {
            visit::visit_item_impl(self, item);
        }
    }

    fn visit_item_fn(&mut self, item: &'ast ItemFn) {
        if !is_test_only(&item.attrs) {
            self.within(&item.sig.ident, |scan| visit::visit_item_fn(scan, item));
        }
    }

    fn visit_impl_item_fn(&mut self, item: &'ast ImplItemFn) {
        if !is_test_only(&item.attrs) {
            self.within(&item.sig.ident, |scan| {
                visit::visit_impl_item_fn(scan, item);
            });
        }
    }

    fn visit_expr_match(&mut self, expr: &'ast ExprMatch) {
        if let Some(function) = self.transition.last() {
            for arm in &expr.arms {
                if let Some((what, line)) = catch_all(&arm.pat) {
                    self.out.push(Diagnostic::new(
                        Rule::ExhaustiveTransition,
                        format!(
                            "{} matches with {what} in the transition function `{function}`: list every state and stimulus, so a new one is a compile error instead of falling through (plan section 6 rule 8)",
                            self.package
                        ),
                        format!("{}:{line}", self.file),
                    ));
                }
            }
        }
        visit::visit_expr_match(self, expr);
    }
}

#[cfg(test)]
mod tests {
    use super::super::testing::{facts_for, policy, rendered};
    use super::*;

    #[test]
    fn should_report_catch_all_arms_when_a_transition_function_matches() {
        // given
        let source = "pub enum S { A, B }\npub fn decide(s: S, t: (S, S)) -> u8 {\n    match s {\n        S::A => 0,\n        _ => 1,\n    };\n    match t {\n        (S::A, ..) => 0,\n        (S::B, other) => 1,\n    }\n}\npub struct M;\nimpl M {\n    fn evolve_phase(s: S) -> u8 {\n        match s {\n            S::A | rest => 2,\n        }\n    }\n}\npub fn step_once(s: S) -> u8 {\n    let f = |x: S| match x {\n        y if true => 3,\n    };\n    f(s)\n}\n";

        // when
        let got = rendered(&check(
            &facts_for("mayfly-data-domain", &[("src/lib.rs", source)]),
            &policy(),
        ));

        // then
        let lines: Vec<&str> = got
            .iter()
            .map(|d| d.rsplit(':').next().unwrap_or(""))
            .collect();
        assert_eq!(lines, ["5)", "8)", "9)", "16)", "22)"], "{got:?}");
        assert!(got[0].starts_with("ARCH-exhaustive-transition"), "{got:?}");
    }

    #[test]
    fn should_ignore_other_functions_and_payload_wildcards_when_matching() {
        // given
        let source = "pub enum S { A(u8), B { n: u8 }, C }\npub fn decide(s: S, o: Option<u8>) -> u8 {\n    match s {\n        S::A(_) | S::B { .. } => 0,\n        S::C => match o {\n            Some(_) | None => 1,\n        },\n    }\n}\npub fn other(s: S) -> u8 {\n    match s {\n        S::A(_) => 0,\n        _ => 1,\n    }\n}\n#[cfg(test)]\nmod tests {\n    fn decide(s: u8) -> u8 { match s { _ => 0 } }\n}\n#[cfg(test)]\nmod more;\n";
        let more = "pub fn decide(s: u8) -> u8 { match s { _ => 0 } }\n";

        // when
        let pure = check(
            &facts_for(
                "mayfly-control-domain",
                &[("src/lib.rs", source), ("src/more.rs", more)],
            ),
            &policy(),
        );
        let impure = check(
            &facts_for(
                "mayfly-control",
                &[(
                    "src/lib.rs",
                    "pub fn decide(s: u8) -> u8 { match s { _ => 0 } }\n",
                )],
            ),
            &policy(),
        );

        // then
        assert_eq!(rendered(&pure), Vec::<String>::new());
        assert_eq!(rendered(&impure), Vec::<String>::new());
    }
}
