//! `ARCH-projection-locality` (ES-I10, `doc/event-store.md` §2.2 and §3.6): an inline
//! projection reads and writes only rows of the stream its unit appends to. The journal refuses
//! a write outside the unit at run time (`Malformed(ProjectionOutsideUnit)`, contract case
//! PC-05); this rule keeps the code from building one. In the production code (`src/`, outside
//! `#[cfg(test)]` and `#[test]` items) of every control-plane and both-planes crate:
//!
//! 1. **Inside an `impl .. ProjectionSpec for ..` block**, naming `StreamId`, `ProjectionWrite`,
//!    `RowWrite`, `UnitOfWork`, `Journal`, `ProjectionStore`, `ProjectionRebuild` or
//!    `AggregateStore`, by any path, in a type, an expression, a pattern or macro tokens: a spec
//!    sees its own stream's rows and returns suffixes, which the fold keys under that stream, so
//!    it has nothing to name another stream with.
//! 2. **A struct expression `ProjectionWrite { .. }` or `RowWrite { .. }`** anywhere but the
//!    inline fold (`crates/mayfly-control/src/projection/fold.rs`), which keys every write by the
//!    stream being folded, the journal's own module (`crates/mayfly-control/src/journal/`) and the
//!    projection and journal contract suites (`crates/mayfly-testkit/src/contracts/`), which forge
//!    outside writes on purpose to show the store refusing them.
//!
//! What is not seen: a write built through a function another crate exports (there is none:
//! both types are plain structs with public fields), and code a procedural macro generates.
//! Pinned by `should_report_streams_named_in_a_spec_when_its_impl_names_them`,
//! `should_report_projection_writes_built_outside_the_fold_when_code_builds_one`,
//! `should_allow_the_fold_the_journal_contracts_and_tests_when_they_build_writes` and the
//! negative fixtures `projection-spec-names-stream` and `projection-write-outside-fold`.

use proc_macro2::{TokenStream, TokenTree};
use syn::visit::{self, Visit};
use syn::{ExprStruct, ItemFn, ItemImpl, ItemMod, Macro};

use super::super::policy::Policy;
use super::super::report::{Diagnostic, Rule};
use super::{Facts, is_test_only};

/// What a spec's code must not name.
const SPEC_FORBIDDEN: &[&str] = &[
    "StreamId",
    "ProjectionWrite",
    "RowWrite",
    "UnitOfWork",
    "Journal",
    "ProjectionStore",
    "ProjectionRebuild",
    "AggregateStore",
];

/// The structs only the fold builds.
const WRITES: &[&str] = &["ProjectionWrite", "RowWrite"];

/// Where a projection write may be built, by workspace-relative path prefix.
const WRITE_PATHS: &[&str] = &[
    "crates/mayfly-control/src/projection/fold.rs",
    "crates/mayfly-control/src/journal/",
    "crates/mayfly-testkit/src/contracts/",
];

/// The diagnostics of this rule for one package.
pub fn check(facts: &Facts, policy: &Policy) -> Vec<Diagnostic> {
    let selected = policy
        .planes
        .control
        .iter()
        .chain(&policy.planes.both)
        .any(|name| *name == facts.package);
    if !selected {
        return Vec::new();
    }
    let src = if facts.dir.is_empty() {
        "src/".to_owned()
    } else {
        format!("{}/src/", facts.dir)
    };
    let mut out = Vec::new();
    for (path, text) in &facts.sources {
        let Ok(file) = syn::parse_file(text) else {
            continue;
        };
        let test_file = !path.starts_with(&src) || is_test_only(&file.attrs);
        let mut scan = Scan {
            package: &facts.package,
            file: path,
            writes_allowed: WRITE_PATHS.iter().any(|p| path.starts_with(p)),
            spec: 0,
            tests: usize::from(test_file),
            out: &mut out,
        };
        scan.visit_file(&file);
    }
    out
}

struct Scan<'a> {
    package: &'a str,
    file: &'a str,
    /// The file may build projection writes.
    writes_allowed: bool,
    /// Depth of `impl ProjectionSpec` blocks being visited.
    spec: usize,
    /// Depth of test-only scopes being visited (the file itself counts as one).
    tests: usize,
    out: &'a mut Vec<Diagnostic>,
}

impl Scan<'_> {
    fn report(&mut self, line: usize, message: String) {
        if self.tests > 0 {
            return;
        }
        self.out.push(Diagnostic::new(
            Rule::ProjectionLocality,
            message,
            format!("{}:{line}", self.file),
        ));
    }

    fn in_test(&mut self, test: bool, visit: impl FnOnce(&mut Self)) {
        self.tests += usize::from(test);
        visit(self);
        self.tests -= usize::from(test);
    }

    fn named_in_spec(&mut self, name: &str, line: usize) {
        if self.spec > 0 && SPEC_FORBIDDEN.contains(&name) {
            self.report(
                line,
                format!(
                    "{} names `{name}` in a ProjectionSpec impl: a spec sees one stream and returns suffixes, so it cannot address another stream's rows (ES-I10)",
                    self.package
                ),
            );
        }
    }

    fn tokens(&mut self, tokens: TokenStream) {
        for tree in tokens {
            match tree {
                TokenTree::Group(group) => self.tokens(group.stream()),
                TokenTree::Ident(ident) => {
                    let name = ident.to_string();
                    self.named_in_spec(&name, ident.span().start().line);
                }
                TokenTree::Punct(_) | TokenTree::Literal(_) => {}
            }
        }
    }
}

impl<'ast> Visit<'ast> for Scan<'_> {
    fn visit_item_mod(&mut self, module: &'ast ItemMod) {
        self.in_test(is_test_only(&module.attrs), |scan| {
            visit::visit_item_mod(scan, module);
        });
    }

    fn visit_item_fn(&mut self, item: &'ast ItemFn) {
        self.in_test(is_test_only(&item.attrs), |scan| {
            visit::visit_item_fn(scan, item);
        });
    }

    fn visit_item_impl(&mut self, item: &'ast ItemImpl) {
        let spec = item.trait_.as_ref().is_some_and(|(path, _)| {
            path.segments
                .last()
                .is_some_and(|s| s.ident == "ProjectionSpec")
        });
        self.in_test(is_test_only(&item.attrs), |scan| {
            scan.spec += usize::from(spec);
            visit::visit_item_impl(scan, item);
            scan.spec -= usize::from(spec);
        });
    }

    fn visit_path_segment(&mut self, segment: &'ast syn::PathSegment) {
        let name = segment.ident.to_string();
        self.named_in_spec(&name, segment.ident.span().start().line);
        visit::visit_path_segment(self, segment);
    }

    fn visit_expr_struct(&mut self, expr: &'ast ExprStruct) {
        if let Some(last) = expr.path.segments.last() {
            let name = last.ident.to_string();
            if WRITES.contains(&name.as_str()) && !self.writes_allowed {
                self.report(
                    last.ident.span().start().line,
                    format!(
                        "{} builds a `{name}` outside the inline fold: only the fold keys a projection write by the stream it folds (ES-I10)",
                        self.package
                    ),
                );
            }
        }
        visit::visit_expr_struct(self, expr);
    }

    fn visit_macro(&mut self, mac: &'ast Macro) {
        if self.spec > 0 {
            self.tokens(mac.tokens.clone());
        }
        visit::visit_macro(self, mac);
    }
}

#[cfg(test)]
mod tests {
    use super::super::testing::{facts_for, policy, rendered};
    use super::*;

    fn lines(diagnostics: &[Diagnostic]) -> Vec<String> {
        rendered(diagnostics)
            .iter()
            .map(|d| {
                d.rsplit(':')
                    .next()
                    .unwrap_or("")
                    .trim_end_matches(')')
                    .to_owned()
            })
            .collect()
    }

    #[test]
    fn should_report_streams_named_in_a_spec_when_its_impl_names_them() {
        // given
        let source = "pub trait ProjectionSpec { fn apply(); }\npub struct StreamId;\npub struct Spec;\nimpl ProjectionSpec for Spec {\n    fn apply() {\n        let _s: Option<StreamId> = None;\n        let _w = crate::journal::RowWrite::default;\n        assert!(format!(\"{:?}\", StreamId).is_empty());\n    }\n}\npub fn elsewhere(_s: StreamId) {}\n";

        // when
        let got = check(
            &facts_for("mayfly-control", &[("src/lib.rs", source)]),
            &policy(),
        );

        // then
        assert_eq!(lines(&got), ["6", "7", "8"], "{got:?}");
        assert!(
            rendered(&got)[0].starts_with("ARCH-projection-locality"),
            "{got:?}"
        );
    }

    #[test]
    fn should_report_projection_writes_built_outside_the_fold_when_code_builds_one() {
        // given
        let source = "pub struct ProjectionWrite { pub key: u8 }\npub struct RowWrite { pub key: u8 }\npub fn forge() -> (ProjectionWrite, RowWrite) {\n    (ProjectionWrite { key: 1 }, crate::RowWrite { key: 2 })\n}\n";

        // when
        let got = check(
            &facts_for("mayfly-control", &[("src/lib.rs", source)]),
            &policy(),
        );

        // then
        assert_eq!(lines(&got), ["4", "4"], "{got:?}");
    }

    #[test]
    fn should_allow_the_fold_the_journal_contracts_and_tests_when_they_build_writes() {
        // given
        let build = "pub struct RowWrite { pub key: u8 }\npub fn build() -> RowWrite { RowWrite { key: 1 } }\n";
        let tested = "pub struct RowWrite { pub key: u8 }\n#[cfg(test)]\nmod tests {\n    fn forge() -> super::RowWrite { super::RowWrite { key: 1 } }\n}\n";
        let control = facts_for(
            "mayfly-control",
            &[
                ("src/lib.rs", tested),
                ("src/projection/fold.rs", build),
                ("src/journal/unit.rs", build),
                ("tests/store.rs", build),
            ],
        );
        let testkit = facts_for("mayfly-testkit", &[("src/contracts/projections.rs", build)]);
        let data = facts_for("mayfly-data", &[("src/lib.rs", build)]);

        // when
        let got: Vec<Diagnostic> = [control, testkit, data]
            .iter()
            .flat_map(|f| check(f, &policy()))
            .collect();

        // then
        assert!(got.is_empty(), "{got:?}");
    }
}
