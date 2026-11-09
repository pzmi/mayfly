//! `ARCH-exhaustive-transition` (foundation.md sections 3.3 and 6.2, plan section 6 rule 8):
//! every pair of a state machine's states and stimuli is a defined transition or a defined
//! refusal, so no match over them falls through a catch-all. A new state or stimulus then fails
//! to compile until every function that reads it says what it does with it, instead of being
//! swallowed by `_ => ...`.
//!
//! In the production code of every `[purity] pure` crate:
//!
//! 1. inside a function named `decide`, `evolve` or `step`, or starting with `decide_`,
//!    `evolve_` or `step_` (closures inside included), no `match` arm is a catch-all;
//! 2. in any function, no `match` whose arms name a variant of an enum declared in a pure
//!    crate (`Phase::Busy`, `Self::Issued` inside `impl Jit`) has a catch-all arm; and
//! 3. in any function, no `matches!`, `if let`, `while let` or `let .. else` tests a pattern
//!    that names such a variant: each is a match with an implicit catch-all. Write the
//!    exhaustive `match` instead.
//!
//! A catch-all is the wildcard `_`, a bare binding (`other =>`, `_x =>`), or `..` among a
//! tuple pattern's elements, also behind a guard or as an or-pattern's alternative. Wildcards
//! inside a variant's payload (`Some(_)`, `Phase::Backoff { .. }`) are allowed: they ignore
//! data, not states or stimuli. Enums of other crates (`Option`, `Result`) are not selected by
//! rules 2 and 3. Code inside macro definitions is not read (the macro that generates event
//! sets uses if-chains); `matches!` invocations are. Test-only items (`#[cfg(test)]`,
//! `#[test]`, a `#[cfg(test)] mod name;` file) are not checked.
//!
//! Pinned by `should_report_catch_all_arms_when_a_transition_function_matches`,
//! `should_ignore_other_functions_and_payload_wildcards_when_matching`,
//! `should_report_implicit_catch_alls_when_a_domain_enum_is_tested` and the negative fixtures
//! `wildcard-transition-arm`, `helper-wildcard-on-domain-enum` and `matches-on-domain-enum`.

use std::collections::BTreeSet;
use std::path::Path;

use syn::parse::{ParseStream, Parser};
use syn::visit::{self, Visit};
use syn::{
    Expr, ExprLet, ExprMacro, ExprMatch, ImplItemFn, Item, ItemFn, ItemImpl, ItemMod, Local, Pat,
    StmtMacro, Token, Type,
};

use super::super::policy::Policy;
use super::super::report::{Diagnostic, Rule};
use super::{Context, Facts, is_test_only};

/// The names (and, followed by `_`, the prefixes) of transition functions.
pub const TRANSITION_FNS: &[&str] = &["decide", "evolve", "step"];

/// The enums declared in the production code of the pure crates, by name.
fn domain_enums(every: &[Facts], policy: &Policy) -> BTreeSet<String> {
    every
        .iter()
        .filter(|facts| policy.purity.pure.contains(&facts.package))
        .flat_map(|facts| &facts.types)
        .filter(|decl| decl.is_enum && decl.site.context == Context::Production)
        .map(|decl| decl.name.clone())
        .collect()
}

/// The diagnostics of this rule over every package.
pub fn check(every: &[Facts], policy: &Policy) -> Vec<Diagnostic> {
    let enums = domain_enums(every, policy);
    every
        .iter()
        .flat_map(|facts| check_package(facts, policy, &enums))
        .collect()
}

fn check_package(facts: &Facts, policy: &Policy, enums: &BTreeSet<String>) -> Vec<Diagnostic> {
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
            enums,
            transition: Vec::new(),
            self_types: Vec::new(),
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

/// The paths a pattern names: `Phase::Busy`, `Jit::Issued(..)`, through nested patterns.
fn pattern_paths<'p>(pat: &'p Pat, out: &mut Vec<&'p syn::Path>) {
    match pat {
        Pat::Path(path) => out.push(&path.path),
        Pat::TupleStruct(tuple) => {
            out.push(&tuple.path);
            tuple.elems.iter().for_each(|e| pattern_paths(e, out));
        }
        Pat::Struct(strukt) => {
            out.push(&strukt.path);
            strukt
                .fields
                .iter()
                .for_each(|field| pattern_paths(&field.pat, out));
        }
        Pat::Ident(binding) => {
            if let Some((_, sub)) = &binding.subpat {
                pattern_paths(sub, out);
            }
        }
        Pat::Or(or) => or.cases.iter().for_each(|case| pattern_paths(case, out)),
        Pat::Guard(guarded) => pattern_paths(&guarded.pat, out),
        Pat::Paren(paren) => pattern_paths(&paren.pat, out),
        Pat::Reference(reference) => pattern_paths(&reference.pat, out),
        Pat::Tuple(tuple) => tuple.elems.iter().for_each(|e| pattern_paths(e, out)),
        Pat::Slice(slice) => slice.elems.iter().for_each(|e| pattern_paths(e, out)),
        _ => {}
    }
}

/// `matches!`'s pattern, after the scrutinee and its comma (a guard is ignored).
fn matches_pattern(input: ParseStream<'_>) -> syn::Result<Pat> {
    let _scrutinee: Expr = input.parse()?;
    input.parse::<Token![,]>()?;
    let pat = Pat::parse_multi_with_leading_vert(input)?;
    let _rest: proc_macro2::TokenStream = input.parse()?;
    Ok(pat)
}

struct Scan<'a> {
    package: &'a str,
    file: &'a str,
    /// The enums of the pure crates, by name.
    enums: &'a BTreeSet<String>,
    /// The transition functions being visited, innermost last.
    transition: Vec<String>,
    /// The self type of each `impl` being visited, innermost last (`None` when not a path).
    self_types: Vec<Option<String>>,
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

    /// The domain enum a pattern names a variant of, with the line, if any.
    fn domain_enum(&self, pat: &Pat) -> Option<(String, usize)> {
        let mut paths = Vec::new();
        pattern_paths(pat, &mut paths);
        paths.into_iter().find_map(|path| {
            let segments: Vec<String> = path.segments.iter().map(|s| s.ident.to_string()).collect();
            let line = path.segments.first()?.ident.span().start().line;
            let owner = match segments.as_slice() {
                [.., owner, _] if owner == "Self" => self.self_types.last().cloned().flatten(),
                [.., owner, _] => Some(owner.clone()),
                _ => None,
            }?;
            self.enums.contains(&owner).then_some((owner, line))
        })
    }

    fn implicit(&mut self, what: &str, owner: &str, line: usize) {
        self.out.push(Diagnostic::new(
            Rule::ExhaustiveTransition,
            format!(
                "{} tests a `{owner}` variant with {what}, which matches every other variant implicitly: write an exhaustive `match` (plan section 6 rule 8)",
                self.package
            ),
            format!("{}:{line}", self.file),
        ));
    }

    fn matches_macro(&mut self, mac: &syn::Macro) {
        if mac
            .path
            .segments
            .last()
            .is_none_or(|s| s.ident != "matches")
        {
            return;
        }
        if let Ok(pat) = matches_pattern.parse2(mac.tokens.clone())
            && let Some((owner, line)) = self.domain_enum(&pat)
        {
            self.implicit("`matches!`", &owner, line);
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
            let self_type = match &*item.self_ty {
                Type::Path(path) => path.path.segments.last().map(|s| s.ident.to_string()),
                _ => None,
            };
            self.self_types.push(self_type);
            visit::visit_item_impl(self, item);
            self.self_types.pop();
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

    fn visit_expr_let(&mut self, expr: &'ast ExprLet) {
        if let Some((owner, line)) = self.domain_enum(&expr.pat) {
            self.implicit("`if let`/`while let`", &owner, line);
        }
        visit::visit_expr_let(self, expr);
    }

    fn visit_local(&mut self, local: &'ast Local) {
        let diverges = local
            .init
            .as_ref()
            .is_some_and(|init| init.diverge.is_some());
        if diverges && let Some((owner, line)) = self.domain_enum(&local.pat) {
            self.implicit("`let .. else`", &owner, line);
        }
        visit::visit_local(self, local);
    }

    fn visit_expr_macro(&mut self, expr: &'ast ExprMacro) {
        self.matches_macro(&expr.mac);
        visit::visit_expr_macro(self, expr);
    }

    fn visit_stmt_macro(&mut self, stmt: &'ast StmtMacro) {
        self.matches_macro(&stmt.mac);
        visit::visit_stmt_macro(self, stmt);
    }

    fn visit_expr_match(&mut self, expr: &'ast ExprMatch) {
        let over_enum = expr
            .arms
            .iter()
            .find_map(|arm| self.domain_enum(&arm.pat))
            .map(|(owner, _)| owner);
        for arm in &expr.arms {
            let Some((what, line)) = catch_all(&arm.pat) else {
                continue;
            };
            let message = match (self.transition.last(), &over_enum) {
                (Some(function), _) => format!(
                    "{} matches with {what} in the transition function `{function}`: list every state and stimulus, so a new one is a compile error instead of falling through (plan section 6 rule 8)",
                    self.package
                ),
                (None, Some(owner)) => format!(
                    "{} matches `{owner}` with {what}: list every variant, so a new one is a compile error instead of falling through (plan section 6 rule 8)",
                    self.package
                ),
                (None, None) => continue,
            };
            self.out.push(Diagnostic::new(
                Rule::ExhaustiveTransition,
                message,
                format!("{}:{line}", self.file),
            ));
        }
        visit::visit_expr_match(self, expr);
    }
}

#[cfg(test)]
mod tests {
    use super::super::testing::{facts_for, policy, rendered};
    use super::*;

    fn run(package: &str, files: &[(&str, &str)]) -> Vec<String> {
        rendered(&check(&[facts_for(package, files)], &policy()))
    }

    fn lines(got: &[String]) -> Vec<&str> {
        got.iter()
            .map(|d| d.rsplit(':').next().unwrap_or(""))
            .collect()
    }

    #[test]
    fn should_report_catch_all_arms_when_a_transition_function_matches() {
        // given
        let source = "pub fn decide(s: u8, t: (u8, u8)) -> u8 {\n    match s {\n        0 => 0,\n        _ => 1,\n    };\n    match t {\n        (0, ..) => 0,\n        (1, other) => 1,\n    }\n}\npub struct M;\nimpl M {\n    fn evolve_phase(s: u8) -> u8 {\n        match s {\n            0 | rest => 2,\n        }\n    }\n}\npub fn step_once(s: u8) -> u8 {\n    let f = |x: u8| match x {\n        y if true => 3,\n    };\n    f(s)\n}\n";

        // when
        let got = run("mayfly-data-domain", &[("src/lib.rs", source)]);

        // then
        assert_eq!(lines(&got), ["4)", "7)", "8)", "15)", "21)"], "{got:?}");
        assert!(got[0].starts_with("ARCH-exhaustive-transition"), "{got:?}");
    }

    #[test]
    fn should_ignore_other_functions_and_payload_wildcards_when_matching() {
        // given
        let source = "pub enum S { A(u8), B { n: u8 }, C }\npub fn decide(s: S, o: Option<u8>) -> u8 {\n    match s {\n        S::A(_) | S::B { .. } => 0,\n        S::C => match o {\n            Some(_) | None => 1,\n        },\n    }\n}\npub fn other(o: Option<u8>) -> u8 {\n    if let Some(v) = o { return v; }\n    match o {\n        Some(1) => 0,\n        _ => 1,\n    }\n}\n#[cfg(test)]\nmod tests {\n    fn decide(s: super::S) -> u8 { match s { super::S::C => 0, _ => 1 } }\n}\n#[cfg(test)]\nmod more;\n";
        let more = "pub fn decide(s: u8) -> u8 { match s { _ => 0 } }\n";

        // when
        let pure = run(
            "mayfly-control-domain",
            &[("src/lib.rs", source), ("src/more.rs", more)],
        );
        let impure = run(
            "mayfly-control",
            &[(
                "src/lib.rs",
                "pub enum S { A, B }\npub fn decide(s: S) -> u8 { match s { S::A => 0, _ => 1 } }\n",
            )],
        );

        // then
        assert_eq!(pure, Vec::<String>::new());
        assert_eq!(impure, Vec::<String>::new());
    }

    #[test]
    fn should_report_implicit_catch_alls_when_a_domain_enum_is_tested() {
        // given
        let source = "pub enum Phase { A, B(u8), C }\nimpl Phase {\n    pub fn busy(self) -> bool {\n        matches!(self, Self::A)\n    }\n}\npub fn helper(p: Phase) -> u8 {\n    match p {\n        Phase::A => 0,\n        _ => 1,\n    }\n}\npub fn tested(p: Phase) -> u8 {\n    if let Phase::B(n) = p {\n        return n;\n    }\n    let Phase::C = p else {\n        return 0;\n    };\n    u8::from(matches!(p, Phase::A | Phase::C))\n}\n";

        // when
        let got = run("mayfly-control-domain", &[("src/lib.rs", source)]);

        // then
        assert_eq!(lines(&got), ["4)", "10)", "14)", "17)", "20)"], "{got:?}");
        assert!(got[1].contains("matches `Phase` with `_`"), "{got:?}");
        assert!(got[0].contains("`matches!`"), "{got:?}");
    }
}
