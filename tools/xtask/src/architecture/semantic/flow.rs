//! Syntax-level flow facts for the command, query and observability rules:
//! `&mut` parameter types, method calls written inside macro tokens, and instrumentation
//! values that reach a decision.
//!
//! Nothing here is type-checked. Instrumentation is found from what the code names: a path
//! into `tracing`, `tracing_core` or `log`, or through a module, crate or field named
//! `observability` or `metrics`. A local bound from such a value (`let n = metrics::depth();`,
//! also through further `let`s and assignments, in any order inside one function body) is
//! instrumentation too. A decision is a branch condition (`if`, `while`, a `match` scrutinee
//! or guard), a `let .. else` initializer, or a returned value (`return`, the tail of a
//! function that returns something). Only the value-producing parts of a block count, so a
//! traced statement inside a branch (`{ tracing::info!(..); Outcome::Done }`) is not a
//! decision; the value it ends with is.

use std::collections::BTreeSet;

use proc_macro2::{Delimiter, TokenStream, TokenTree};
use syn::visit::{self, Visit};

use super::paths::{self, Scopes, Written, scan_tokens};

/// Crates whose items are instrumentation.
pub const INSTRUMENTATION_CRATES: &[&str] = &["tracing", "tracing_core", "log"];

/// A path, module or field with one of these segments is instrumentation.
pub const INSTRUMENTATION_MODULES: &[&str] = &["observability", "metrics"];

/// Whether a resolved path or a module it passes through is instrumentation.
pub fn is_instrumentation(candidate: &[String]) -> bool {
    candidate
        .first()
        .is_some_and(|c| INSTRUMENTATION_CRATES.contains(&c.as_str()))
        || candidate
            .iter()
            .any(|s| INSTRUMENTATION_MODULES.contains(&s.as_str()))
}

/// Method calls written in a token stream (`.name(` or `.name::<`), at every depth.
pub fn token_method_calls(tokens: &TokenStream) -> Vec<(String, usize)> {
    let mut out = Vec::new();
    method_calls_into(tokens, &mut out);
    out
}

fn method_calls_into(tokens: &TokenStream, out: &mut Vec<(String, usize)>) {
    let trees: Vec<TokenTree> = tokens.clone().into_iter().collect();
    for (i, tree) in trees.iter().enumerate() {
        if let TokenTree::Group(group) = tree {
            method_calls_into(&group.stream(), out);
            continue;
        }
        let is_dot =
            |t: Option<&TokenTree>| matches!(t, Some(TokenTree::Punct(p)) if p.as_char() == '.');
        if !is_dot(Some(tree)) || i.checked_sub(1).is_some_and(|p| is_dot(trees.get(p))) {
            continue;
        }
        let Some(TokenTree::Ident(ident)) = trees.get(i + 1) else {
            continue;
        };
        let called = match trees.get(i + 2) {
            Some(TokenTree::Group(g)) => g.delimiter() == Delimiter::Parenthesis,
            Some(TokenTree::Punct(p)) => p.as_char() == ':',
            _ => false,
        };
        if called {
            out.push((paths::name(ident), ident.span().start().line));
        }
    }
}

/// The paths inside every `&mut` of `ty` (nested ones included); `None` when `ty` holds no
/// `&mut`.
pub fn mut_reference_paths(ty: &syn::Type) -> Option<Vec<syn::Path>> {
    #[derive(Default)]
    struct MutRefs {
        found: bool,
        paths: Vec<syn::Path>,
    }
    #[derive(Default)]
    struct AllPaths(Vec<syn::Path>);
    impl<'ast> Visit<'ast> for AllPaths {
        fn visit_path(&mut self, path: &'ast syn::Path) {
            self.0.push(path.clone());
            visit::visit_path(self, path);
        }
    }
    impl<'ast> Visit<'ast> for MutRefs {
        fn visit_type_reference(&mut self, reference: &'ast syn::TypeReference) {
            if reference.mutability.is_some() {
                self.found = true;
                let mut all = AllPaths::default();
                all.visit_type(&reference.elem);
                self.paths.extend(all.0);
            }
            visit::visit_type_reference(self, reference);
        }
    }
    let mut refs = MutRefs::default();
    refs.visit_type(ty);
    refs.found.then_some(refs.paths)
}

/// The name a parameter pattern binds, for a diagnostic, and the line of the pattern.
pub fn pattern_name(pat: &syn::Pat) -> Option<(String, usize)> {
    match pat {
        syn::Pat::Ident(p) => Some((paths::name(&p.ident), p.ident.span().start().line)),
        syn::Pat::Type(p) => pattern_name(&p.pat),
        syn::Pat::Reference(p) => pattern_name(&p.pat),
        _ => None,
    }
}

/// Every place in `block` (a function body with return type `output`) where an
/// instrumentation value reaches a decision: line, position and what was named.
pub fn decisions(
    scopes: &Scopes,
    module: &[String],
    block: &syn::Block,
    output: &syn::ReturnType,
) -> Vec<(usize, &'static str, String)> {
    let mut tainted = BTreeSet::new();
    loop {
        let mut binder = Taint {
            scopes,
            module,
            tainted: &tainted,
            new: BTreeSet::new(),
        };
        binder.visit_block(block);
        let before = tainted.len();
        tainted.extend(binder.new);
        if tainted.len() == before {
            break;
        }
    }
    let mut scan = DecisionScan {
        scopes,
        module,
        tainted: &tainted,
        out: Vec::new(),
    };
    scan.visit_block(block);
    if matches!(output, syn::ReturnType::Type(..))
        && let Some(syn::Stmt::Expr(tail, None)) = block.stmts.last()
    {
        scan.check(tail, "a returned value");
    }
    scan.out.sort();
    scan.out.dedup();
    scan.out
}

/// The parts of `expr` that become its value: the tail of a block, each branch of an `if`
/// or `match`, the inside of parentheses; any other expression is its own value.
fn value_parts(expr: &syn::Expr) -> Vec<&syn::Expr> {
    match expr {
        syn::Expr::Block(b) => block_value(&b.block),
        syn::Expr::Unsafe(b) => block_value(&b.block),
        syn::Expr::If(i) => {
            let mut out = block_value(&i.then_branch);
            if let Some((_, other)) = &i.else_branch {
                out.extend(value_parts(other));
            }
            out
        }
        syn::Expr::Match(m) => m.arms.iter().flat_map(|a| value_parts(&a.body)).collect(),
        syn::Expr::Paren(p) => value_parts(&p.expr),
        syn::Expr::Group(g) => value_parts(&g.expr),
        other => vec![other],
    }
}

fn block_value(block: &syn::Block) -> Vec<&syn::Expr> {
    match block.stmts.last() {
        Some(syn::Stmt::Expr(tail, None)) => value_parts(tail),
        _ => Vec::new(),
    }
}

/// The first instrumentation `expr` names, with its line.
fn find(
    scopes: &Scopes,
    module: &[String],
    tainted: &BTreeSet<String>,
    expr: &syn::Expr,
) -> Option<(usize, String)> {
    let mut finder = Finder {
        scopes,
        module,
        tainted,
        hit: None,
    };
    for part in value_parts(expr) {
        finder.visit_expr(part);
    }
    finder.hit
}

struct Finder<'a> {
    scopes: &'a Scopes,
    module: &'a [String],
    tainted: &'a BTreeSet<String>,
    hit: Option<(usize, String)>,
}

impl Finder<'_> {
    fn path(&mut self, path: &Written, line: usize, import: bool) {
        if self.hit.is_some() {
            return;
        }
        if !path.absolute && path.segments.len() == 1 && self.tainted.contains(&path.segments[0]) {
            self.hit = Some((
                line,
                format!("`{}` (bound from instrumentation)", path.render()),
            ));
            return;
        }
        let (resolved, through) = self.scopes.resolve_through(self.module, path, import);
        let named = path
            .segments
            .iter()
            .any(|s| INSTRUMENTATION_MODULES.contains(&s.as_str()));
        if named
            || resolved.iter().any(|c| is_instrumentation(c))
            || through.iter().any(|c| is_instrumentation(c))
        {
            self.hit = Some((line, format!("`{}`", path.render())));
        }
    }
}

impl<'ast> Visit<'ast> for Finder<'_> {
    fn visit_item(&mut self, _: &'ast syn::Item) {}

    fn visit_path(&mut self, path: &'ast syn::Path) {
        let line = path
            .segments
            .first()
            .map_or(0, |s| s.ident.span().start().line);
        self.path(&Written::from_syn(path), line, false);
        visit::visit_path(self, path);
    }

    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        let line = mac
            .path
            .segments
            .first()
            .map_or(0, |s| s.ident.span().start().line);
        self.path(&Written::from_syn(&mac.path), line, true);
        for found in scan_tokens(&mac.tokens).paths {
            if !found.open_head {
                self.path(&found.path, found.line, true);
            }
        }
        visit::visit_macro(self, mac);
    }

    fn visit_expr_field(&mut self, field: &'ast syn::ExprField) {
        if self.hit.is_none()
            && let syn::Member::Named(name) = &field.member
            && INSTRUMENTATION_MODULES.contains(&paths::name(name).as_str())
        {
            self.hit = Some((
                name.span().start().line,
                format!("field `{}`", paths::name(name)),
            ));
        }
        visit::visit_expr_field(self, field);
    }
}

/// One pass of the taint: locals bound or assigned from instrumentation.
struct Taint<'a> {
    scopes: &'a Scopes,
    module: &'a [String],
    tainted: &'a BTreeSet<String>,
    new: BTreeSet<String>,
}

impl<'ast> Visit<'ast> for Taint<'_> {
    fn visit_item(&mut self, _: &'ast syn::Item) {}

    fn visit_local(&mut self, local: &'ast syn::Local) {
        if let Some(init) = &local.init
            && find(self.scopes, self.module, self.tainted, &init.expr).is_some()
        {
            let mut names = Bound::default();
            names.visit_pat(&local.pat);
            self.new.extend(names.0);
        }
        visit::visit_local(self, local);
    }

    fn visit_expr_assign(&mut self, assign: &'ast syn::ExprAssign) {
        if let syn::Expr::Path(left) = &*assign.left
            && let Some(name) = left.path.get_ident()
            && find(self.scopes, self.module, self.tainted, &assign.right).is_some()
        {
            self.new.insert(paths::name(name));
        }
        visit::visit_expr_assign(self, assign);
    }
}

/// The names a pattern binds.
#[derive(Default)]
struct Bound(Vec<String>);

impl<'ast> Visit<'ast> for Bound {
    fn visit_pat_ident(&mut self, pat: &'ast syn::PatIdent) {
        self.0.push(paths::name(&pat.ident));
        visit::visit_pat_ident(self, pat);
    }
}

struct DecisionScan<'a> {
    scopes: &'a Scopes,
    module: &'a [String],
    tainted: &'a BTreeSet<String>,
    out: Vec<(usize, &'static str, String)>,
}

impl DecisionScan<'_> {
    fn check(&mut self, expr: &syn::Expr, position: &'static str) {
        if let Some((line, what)) = find(self.scopes, self.module, self.tainted, expr) {
            self.out.push((line, position, what));
        }
    }
}

impl<'ast> Visit<'ast> for DecisionScan<'_> {
    fn visit_item(&mut self, _: &'ast syn::Item) {}

    fn visit_expr_if(&mut self, expr: &'ast syn::ExprIf) {
        self.check(&expr.cond, "an if condition");
        visit::visit_expr_if(self, expr);
    }

    fn visit_expr_while(&mut self, expr: &'ast syn::ExprWhile) {
        self.check(&expr.cond, "a while condition");
        visit::visit_expr_while(self, expr);
    }

    fn visit_expr_match(&mut self, expr: &'ast syn::ExprMatch) {
        self.check(&expr.expr, "a match scrutinee");
        visit::visit_expr_match(self, expr);
    }

    fn visit_pat_guard(&mut self, pat: &'ast syn::PatGuard) {
        self.check(&pat.guard, "a match guard");
        visit::visit_pat_guard(self, pat);
    }

    fn visit_expr_return(&mut self, expr: &'ast syn::ExprReturn) {
        if let Some(value) = &expr.expr {
            self.check(value, "a returned value");
        }
        visit::visit_expr_return(self, expr);
    }

    fn visit_local(&mut self, local: &'ast syn::Local) {
        if let Some(init) = &local.init
            && init.diverge.is_some()
        {
            self.check(&init.expr, "a let-else initializer");
        }
        visit::visit_local(self, local);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn decisions_in(source: &str) -> Vec<(usize, &'static str, String)> {
        let file = syn::parse_file(source).unwrap();
        let mut scopes = Scopes::new(BTreeMap::new());
        for item in &file.items {
            match item {
                syn::Item::Use(u) => scopes.add_use(&[], u),
                syn::Item::Mod(m) => scopes.add_child(&[], &m.ident.to_string()),
                _ => {}
            }
        }
        file.items
            .iter()
            .filter_map(|item| match item {
                syn::Item::Fn(f) => Some(decisions(&scopes, &[], &f.block, &f.sig.output)),
                _ => None,
            })
            .flatten()
            .collect()
    }

    #[test]
    fn should_find_instrumentation_when_it_reaches_a_branch_or_return() {
        // given
        let source = "use tracing::enabled as on;\nmod metrics;\nfn a() { if on!(Level::INFO) { x(); } }\nfn b() -> u8 { let d = metrics::depth();\n let e = d + 1;\n match e { _ => 0 } }\nfn c(s: &S) -> bool { s.metrics.ready() }\nfn d() -> u8 { return log::max_level() as u8; }\nfn e() { let Some(x) = metrics::peek() else { return; }; }\n";

        // when
        let got = decisions_in(source);

        // then
        let positions: Vec<(usize, &str)> = got.iter().map(|(l, p, _)| (*l, *p)).collect();
        assert_eq!(
            positions,
            [
                (3, "an if condition"),
                (6, "a match scrutinee"),
                (7, "a returned value"),
                (8, "a returned value"),
                (9, "a let-else initializer"),
            ],
            "{got:?}"
        );
        assert!(
            got[1].2.contains("`e` (bound from instrumentation)"),
            "{got:?}"
        );
    }

    #[test]
    fn should_pass_when_instrumentation_is_a_statement_beside_the_decision() {
        // given
        let source = "fn a(n: u8) -> u8 {\n    tracing::info!(n);\n    let m = { tracing::debug!(n); n + 1 };\n    if m > 2 { tracing::warn!(m); 1 } else { 0 }\n}\nfn b() { tracing::info!(\"x\") }\n";

        // when
        let got = decisions_in(source);

        // then
        assert_eq!(got, Vec::new());
    }

    #[test]
    fn should_find_method_calls_when_written_in_macro_tokens() {
        // given
        let tokens: TokenStream = "format!(\"{}\", state.publish_next(c)); x..y.len(); a.b::<u8>()"
            .parse()
            .unwrap();

        // when
        let got: Vec<String> = token_method_calls(&tokens)
            .into_iter()
            .map(|(n, _)| n)
            .collect();

        // then
        assert_eq!(got, ["publish_next", "len", "b"]);
    }
}
