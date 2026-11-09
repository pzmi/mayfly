//! `ARCH-single-append` (foundation.md sections 3.3 and 6.3, `doc/event-store.md` §3.7):
//! `AggregateStore::save` is the only caller of `Journal::append`, so every write the control
//! plane makes goes through the epoch check, the receipt, the derived event ids and the
//! after-commit kick, in that order. A second caller would write units nothing checked.
//!
//! In every crate of the control plane and of `[planes] both` (production and test code), a
//! call is refused when it is:
//!
//! - a method call `.append(x)` with one argument that is neither `&mut ..` (`Vec::append`,
//!   `BTreeMap::append`) nor a literal (`OpenOptions::append(true)`): method calls are not
//!   type-resolved, so the shape of the argument tells a journal append from the collections'
//!   (a unit of work is passed by value);
//! - a path `Journal::append` or `<_ as Journal>::append`, called or not;
//! - the same method call written inside a macro's tokens (`assert!(j.append(u).is_ok())`),
//!   found by scanning for `.` `append` `(..)`.
//!
//! Allowed, and only there:
//!
//! - the bodies of `save` and `settle` (its step 5, split out for length) in an
//!   `impl .. AggregateStore ..` block of `mayfly-control`;
//! - every `impl .. Journal for ..` block (an adapter, the memory journal, a broken or scripted
//!   journal wrapping another);
//! - the journal contract suite, `crates/mayfly-testkit/src/contracts/`, and its own tests,
//!   `crates/mayfly-testkit/tests/journal_contract.rs` (the broken journals it must catch);
//! - the test code of the journal adapters (`mayfly-adapter-*`: their `tests/` and
//!   `#[cfg(test)]` items), which drive their journal below the store as the contract does.
//!
//! A renamed crate or directory makes an exemption lapse, so the rule then fails closed rather
//! than passing unchecked. Pinned by `should_report_appends_outside_the_store_when_code_calls_one`,
//! `should_allow_the_store_journals_and_collections_when_they_append` and the negative fixture
//! `second-append-caller`.

use proc_macro2::{Delimiter, TokenStream, TokenTree};
use syn::visit::{self, Visit};
use syn::{Expr, ExprMethodCall, ExprPath, ImplItemFn, ItemFn, ItemImpl, ItemMod, Macro};

use super::super::policy::Policy;
use super::super::report::{Diagnostic, Rule};
use super::{Facts, is_test_only};

/// The package whose `AggregateStore` may append.
const STORE_PACKAGE: &str = "mayfly-control";
/// The functions of `impl AggregateStore` that may append.
const STORE_FNS: &[&str] = &["save", "settle"];
/// The journal contract suite and its own tests, by path prefix.
const CONTRACT_PATHS: &[&str] = &[
    "crates/mayfly-testkit/src/contracts/",
    "crates/mayfly-testkit/tests/journal_contract.rs",
];
/// The prefix of the adapter crates whose test code may append.
const ADAPTER_PREFIX: &str = "mayfly-adapter-";

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
    let adapter = facts.package.starts_with(ADAPTER_PREFIX);
    let src = if facts.dir.is_empty() {
        "src/".to_owned()
    } else {
        format!("{}/src/", facts.dir)
    };
    let mut out = Vec::new();
    for (path, text) in &facts.sources {
        let contract = CONTRACT_PATHS.iter().any(|p| path.starts_with(p));
        if contract || (adapter && !path.starts_with(&src)) {
            continue;
        }
        let Ok(file) = syn::parse_file(text) else {
            continue;
        };
        let mut scan = Scan {
            package: &facts.package,
            file: path,
            store: false,
            allowed: 0,
            skip_tests: adapter,
            out: &mut out,
        };
        scan.visit_file(&file);
    }
    out
}

struct Scan<'a> {
    package: &'a str,
    file: &'a str,
    /// Inside an `impl .. AggregateStore ..` block of the store package.
    store: bool,
    /// Depth of allowed scopes being visited.
    allowed: usize,
    /// Test-only items are not checked (an adapter's own tests).
    skip_tests: bool,
    out: &'a mut Vec<Diagnostic>,
}

impl Scan<'_> {
    fn report(&mut self, line: usize, what: &str) {
        if self.allowed > 0 {
            return;
        }
        self.out.push(Diagnostic::new(
            Rule::SingleAppend,
            format!(
                "{} calls {what} outside AggregateStore::save: only the store writes the journal, so every unit goes through its epoch check, receipt and derived ids (foundation.md section 6.3)",
                self.package
            ),
            format!("{}:{line}", self.file),
        ));
    }

    fn allow(&mut self, visit: impl FnOnce(&mut Self)) {
        self.allowed += 1;
        visit(self);
        self.allowed -= 1;
    }
}

fn last_segment(path: &syn::Path) -> String {
    path.segments
        .last()
        .map(|s| s.ident.to_string())
        .unwrap_or_default()
}

fn names_journal(path: &syn::Path) -> bool {
    path.segments.iter().any(|s| s.ident == "Journal")
}

/// Whether `arg` is what a journal append takes: not `&mut ..` and not a literal.
fn unit_like(arg: &Expr) -> bool {
    !matches!(
        arg,
        Expr::Reference(r) if r.mutability.is_some()
    ) && !matches!(arg, Expr::Lit(_))
}

/// `. append ( .. )` in macro tokens, with the same argument test as [`unit_like`] (a leading
/// `& mut` or a single literal is a collection's or a file's append).
fn appends_in_tokens(tokens: TokenStream, lines: &mut Vec<usize>) {
    let trees: Vec<TokenTree> = tokens.into_iter().collect();
    for (i, tree) in trees.iter().enumerate() {
        if let TokenTree::Group(group) = tree {
            appends_in_tokens(group.stream(), lines);
        }
        let TokenTree::Ident(ident) = tree else {
            continue;
        };
        let dotted = i > 0 && matches!(&trees[i - 1], TokenTree::Punct(p) if p.as_char() == '.');
        let call = match trees.get(i + 1) {
            Some(TokenTree::Group(g)) if g.delimiter() == Delimiter::Parenthesis => Some(g),
            _ => None,
        };
        let (true, Some(call)) = (dotted && ident == "append", call) else {
            continue;
        };
        let args: Vec<TokenTree> = call.stream().into_iter().collect();
        let by_mut = matches!(
            (args.first(), args.get(1)),
            (Some(TokenTree::Punct(p)), Some(TokenTree::Ident(m))) if p.as_char() == '&' && m == "mut"
        );
        let literal = args.len() == 1 && matches!(args[0], TokenTree::Literal(_));
        if !args.is_empty() && !by_mut && !literal {
            lines.push(ident.span().start().line);
        }
    }
}

impl<'ast> Visit<'ast> for Scan<'_> {
    fn visit_item_mod(&mut self, module: &'ast ItemMod) {
        if !(self.skip_tests && is_test_only(&module.attrs)) {
            visit::visit_item_mod(self, module);
        }
    }

    fn visit_item_fn(&mut self, item: &'ast ItemFn) {
        if !(self.skip_tests && is_test_only(&item.attrs)) {
            visit::visit_item_fn(self, item);
        }
    }

    fn visit_item_impl(&mut self, item: &'ast ItemImpl) {
        if self.skip_tests && is_test_only(&item.attrs) {
            return;
        }
        let journal_impl = item
            .trait_
            .as_ref()
            .is_some_and(|(path, _)| last_segment(path) == "Journal");
        if journal_impl {
            self.allow(|scan| visit::visit_item_impl(scan, item));
            return;
        }
        let store = item.trait_.is_none()
            && self.package == STORE_PACKAGE
            && matches!(&*item.self_ty, syn::Type::Path(p) if last_segment(&p.path) == "AggregateStore");
        let outer = self.store;
        self.store = store;
        visit::visit_item_impl(self, item);
        self.store = outer;
    }

    fn visit_impl_item_fn(&mut self, item: &'ast ImplItemFn) {
        if self.skip_tests && is_test_only(&item.attrs) {
            return;
        }
        if self.store && STORE_FNS.iter().any(|f| item.sig.ident == f) {
            self.allow(|scan| visit::visit_impl_item_fn(scan, item));
        } else {
            visit::visit_impl_item_fn(self, item);
        }
    }

    fn visit_expr_method_call(&mut self, call: &'ast ExprMethodCall) {
        if call.method == "append" && call.args.len() == 1 && call.args.iter().all(unit_like) {
            self.report(call.method.span().start().line, "`.append(..)`");
        }
        visit::visit_expr_method_call(self, call);
    }

    fn visit_expr_path(&mut self, expr: &'ast ExprPath) {
        let qualified = expr
            .qself
            .as_ref()
            .is_some_and(|_| names_journal(&expr.path));
        let named = expr.path.segments.len() >= 2 && names_journal(&expr.path);
        if last_segment(&expr.path) == "append" && (qualified || named) {
            let line = expr
                .path
                .segments
                .last()
                .map_or(0, |s| s.ident.span().start().line);
            self.report(line, "`Journal::append`");
        }
        visit::visit_expr_path(self, expr);
    }

    fn visit_macro(&mut self, mac: &'ast Macro) {
        let mut lines = Vec::new();
        appends_in_tokens(mac.tokens.clone(), &mut lines);
        for line in lines {
            self.report(line, "`.append(..)` in a macro's tokens");
        }
        visit::visit_macro(self, mac);
    }
}

#[cfg(test)]
mod tests {
    use super::super::testing::{facts_for, policy, rendered};
    use super::*;

    #[test]
    fn should_report_appends_outside_the_store_when_code_calls_one() {
        // given
        let source = "pub trait Journal { fn append(&self, u: u8); }\npub fn bypass<J: Journal>(j: &J) {\n    j.append(1 + 1);\n    <J as Journal>::append(j, 2);\n    let f = Journal::append;\n    assert!(matches!(j.append(unit()), ()));\n}\nfn unit() -> u8 { 0 }\npub struct Other;\nimpl Other {\n    pub fn save<J: Journal>(j: &J) { j.append(unit()); }\n}\n";

        // when
        let got = rendered(&check(
            &facts_for("mayfly-control", &[("src/lib.rs", source)]),
            &policy(),
        ));

        // then
        let lines: Vec<&str> = got
            .iter()
            .map(|d| d.rsplit(':').next().unwrap_or(""))
            .collect();
        assert_eq!(lines, ["3)", "4)", "5)", "6)", "11)"], "{got:?}");
        assert!(got[0].starts_with("ARCH-single-append"), "{got:?}");
    }

    #[test]
    fn should_allow_the_store_journals_and_collections_when_they_append() {
        // given
        let source = "pub trait Journal { fn append(&self, u: u8); }\npub struct AggregateStore<J>(J);\nimpl<J: Journal> AggregateStore<J> {\n    pub fn save(&self) { self.0.append(unit()); self.settle(); }\n    fn settle(&self) { self.0.append(unit()); }\n}\nfn unit() -> u8 { 0 }\npub struct Wrapper<J>(J);\nimpl<J: Journal> Journal for Wrapper<J> {\n    fn append(&self, u: u8) { self.0.append(u) }\n}\npub fn collections(mut a: Vec<u8>, mut b: Vec<u8>, o: &mut std::fs::OpenOptions) {\n    a.append(&mut b);\n    o.append(true);\n    assert!(a.is_empty() || { a.append(&mut b); true });\n}\n";
        let adapter = "pub fn f() {}\n#[cfg(test)]\nmod tests {\n    fn t<J: super::Journal>(j: &J, u: u8) { j.append(u); }\n}\npub trait Journal { fn append(&self, u: u8); }\n";

        // when
        let store = check(
            &facts_for("mayfly-control", &[("src/lib.rs", source)]),
            &policy(),
        );
        let elsewhere = check(
            &facts_for("mayfly-control-domain", &[("src/lib.rs", source)]),
            &policy(),
        );
        let adapter_tests = check(
            &facts_for("mayfly-adapter-sqlite", &[("src/lib.rs", adapter)]),
            &policy(),
        );
        let data_plane = check(
            &facts_for(
                "mayfly-data",
                &[("src/lib.rs", "pub fn f(j: &u8) { j.append(1 + 1); }\n")],
            ),
            &policy(),
        );

        // then
        assert_eq!(rendered(&store), Vec::<String>::new());
        assert_eq!(rendered(&adapter_tests), Vec::<String>::new());
        assert_eq!(rendered(&data_plane), Vec::<String>::new());
        assert_eq!(
            rendered(&elsewhere).len(),
            2,
            "save and settle are the store's only in mayfly-control: {elsewhere:?}"
        );
    }
}
