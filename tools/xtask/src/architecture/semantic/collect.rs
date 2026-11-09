//! Walks the units of one Cargo target twice: first to build the module scopes (`use`,
//! `extern crate`, items, child modules, also inside macro token streams), then to record
//! every path reference with its resolution, and every `unsafe` site.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use syn::visit::{self, Visit};
use syn::{Attribute, Item, Meta};

use super::flow;
use super::paths::{self, Scopes, TokenScan, Written, scan_tokens, use_leaves};
use super::{
    AttributeUse, Context, Facts, HiddenItem, MethodCall, MutParam, RefKind, Reference, Site,
    TestFn, Unit, effective_metas, include_path, is_doc_hidden, is_include, is_test_attribute,
    is_test_only, non_literal, relative,
};

/// Collects the facts of one target into `facts`.
pub fn collect_target(
    root: &Path,
    units: &[Unit],
    renames: BTreeMap<String, String>,
    facts: &mut Facts,
) {
    let mut scopes = Scopes::new(renames);
    for unit in units {
        let mut binder = Binder {
            scopes: &mut scopes,
            module: unit.module.clone(),
        };
        for attr in &unit.attrs {
            binder.visit_attribute(attr);
        }
        for item in &unit.items {
            binder.visit_item(item);
        }
    }
    for unit in units {
        if !unit.test {
            facts.modules.insert(unit.module.clone());
        }
        let mut collector = Collector {
            root,
            scopes: &scopes,
            module: unit.module.clone(),
            file: unit.file.clone(),
            rel: relative(root, &unit.file),
            test: unit.test,
            facts: &mut *facts,
            depth: 0,
            impl_generics: Vec::new(),
        };
        for attr in &unit.attrs {
            collector.visit_attribute(attr);
        }
        for item in &unit.items {
            collector.visit_item(item);
        }
    }
}

/// The `cfg` predicate names that select a platform.
const PLATFORM_CFGS: &[&str] = &[
    "target_os",
    "target_family",
    "target_vendor",
    "target_env",
    "unix",
    "windows",
];

/// The predicate of a `cfg!` (also `std::cfg!`, `core::cfg!`) that names a platform.
fn platform_cfg(mac: &syn::Macro) -> Option<String> {
    let names: Vec<String> = mac
        .path
        .segments
        .iter()
        .map(|s| paths::name(&s.ident))
        .collect();
    let is_cfg = match names.as_slice() {
        [cfg] => cfg == "cfg",
        [krate, cfg] => matches!(krate.as_str(), "std" | "core") && cfg == "cfg",
        _ => false,
    };
    (is_cfg && names_platform(&mac.tokens)).then(|| mac.tokens.to_string())
}

/// Whether `tokens`, at any depth, hold a platform predicate name.
fn names_platform(tokens: &proc_macro2::TokenStream) -> bool {
    tokens.clone().into_iter().any(|tree| match tree {
        proc_macro2::TokenTree::Ident(ident) => PLATFORM_CFGS.contains(&ident.to_string().as_str()),
        proc_macro2::TokenTree::Group(group) => names_platform(&group.stream()),
        _ => false,
    })
}

/// Macros whose `#[test]` functions are checked from their tokens.
pub const TEST_MACROS: &[&str] = &["proptest"];

/// Pass one: the scopes.
struct Binder<'a> {
    scopes: &'a mut Scopes,
    module: Vec<String>,
}

impl Binder<'_> {
    fn bind_tokens(&mut self, scan: &TokenScan) {
        for item in &scan.uses {
            self.scopes.add_use(&self.module, item);
        }
        for item in &scan.extern_crates {
            self.scopes.add_extern_crate(&self.module, item);
        }
    }
}

impl<'ast> Visit<'ast> for Binder<'_> {
    fn visit_item(&mut self, item: &'ast Item) {
        let name = match item {
            Item::Const(i) => Some(&i.ident),
            Item::Enum(i) => Some(&i.ident),
            Item::Fn(i) => Some(&i.sig.ident),
            Item::Static(i) => Some(&i.ident),
            Item::Struct(i) => Some(&i.ident),
            Item::Trait(i) => Some(&i.ident),
            Item::TraitAlias(i) => Some(&i.ident),
            Item::Type(i) => Some(&i.ident),
            Item::Union(i) => Some(&i.ident),
            Item::Macro(i) => i.ident.as_ref(),
            _ => None,
        };
        if let Some(name) = name {
            self.scopes.add_item(&self.module, paths::name(name));
        }
        match item {
            Item::Use(u) => self.scopes.add_use(&self.module, u),
            Item::ExternCrate(e) => self.scopes.add_extern_crate(&self.module, e),
            Item::Mod(m) => {
                self.scopes.add_child(&self.module, &paths::name(&m.ident));
                // Out-of-line and inline modules are their own units; an inline module left
                // in place (inside a function body) counts for this module.
                visit::visit_item_mod(self, m);
            }
            other => visit::visit_item(self, other),
        }
    }

    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        let scan = scan_tokens(&mac.tokens);
        self.bind_tokens(&scan);
        visit::visit_macro(self, mac);
    }

    fn visit_attribute(&mut self, attr: &'ast Attribute) {
        if let Meta::List(list) = &attr.meta {
            let scan = scan_tokens(&list.tokens);
            self.bind_tokens(&scan);
        }
        visit::visit_attribute(self, attr);
    }
}

/// Pass two: references and `unsafe` sites.
struct Collector<'a> {
    root: &'a Path,
    scopes: &'a Scopes,
    module: Vec<String>,
    file: PathBuf,
    rel: String,
    test: bool,
    facts: &'a mut Facts,
    depth: usize,
    /// Type parameters of the enclosing `impl` or `trait`.
    impl_generics: Vec<String>,
}

impl Collector<'_> {
    fn site(&self, line: usize) -> Site {
        Site {
            file: self.rel.clone(),
            line,
            context: if self.test {
                Context::Test
            } else {
                Context::Production
            },
        }
    }

    fn reference(&mut self, path: &Written, line: usize, kind: RefKind, open_tail: bool) {
        // An unbound macro name is a prelude macro (`todo!`), not a local value.
        let resolved = self
            .scopes
            .resolve_through(&self.module, path, kind == RefKind::Macro);
        self.push(path, resolved, line, kind, (open_tail, false));
    }

    fn push(
        &mut self,
        path: &Written,
        (resolved, through): (Vec<Vec<String>>, Vec<Vec<String>>),
        line: usize,
        kind: RefKind,
        (open_tail, open_head): (bool, bool),
    ) {
        if resolved.is_empty() {
            return;
        }
        let site = self.site(line);
        self.facts.references.push(Reference {
            site,
            written: path.render(),
            resolved,
            open_tail,
            open_head,
            kind,
            module: self.module.clone(),
            through,
        });
    }

    fn use_item(&mut self, item: &syn::ItemUse) {
        for leaf in use_leaves(item) {
            let resolved = self.scopes.resolve_through(&self.module, &leaf.path, true);
            self.push(
                &leaf.path,
                resolved,
                leaf.line,
                RefKind::Path,
                (false, false),
            );
        }
    }

    fn extern_crate(&mut self, item: &syn::ItemExternCrate) {
        let path = Written {
            absolute: true,
            segments: vec![paths::name(&item.ident)],
        };
        let resolved = self.scopes.resolve_through(&self.module, &path, true);
        self.push(
            &path,
            resolved,
            item.ident.span().start().line,
            RefKind::Path,
            (false, false),
        );
    }

    fn unsafe_site(&mut self, line: usize, what: &'static str) {
        let site = self.site(line);
        self.facts.unsafe_sites.push((site, what));
    }

    /// Records what a token stream names: paths, macro calls, `use` leaves, `unsafe`,
    /// attributes and tests. Tests are checkable only in a known test macro (`proptest!`).
    fn tokens(&mut self, tokens: &proc_macro2::TokenStream, test_macro: bool) {
        let scan = scan_tokens(tokens);
        for found in &scan.paths {
            let kind = if found.is_macro {
                RefKind::Macro
            } else {
                RefKind::Path
            };
            if found.open_head {
                // `$m::fs::read`: no resolution; the rules match the tail.
                let resolved = (vec![found.path.segments.clone()], Vec::new());
                self.push(&found.path, resolved, found.line, kind, (false, true));
            } else if found.path.segments.len() == 1 && kind == RefKind::Path {
                // A lone name in a token stream may be a crate handed to a macro.
                let resolved = self.scopes.resolve_through(&self.module, &found.path, true);
                self.push(
                    &found.path,
                    resolved,
                    found.line,
                    kind,
                    (found.open_tail, false),
                );
            } else {
                self.reference(&found.path, found.line, kind, found.open_tail);
            }
        }
        for item in &scan.uses {
            self.use_item(item);
        }
        for item in &scan.extern_crates {
            self.extern_crate(item);
        }
        for line in scan.unsafe_lines {
            self.unsafe_site(line, "`unsafe` in a macro");
        }
        for (meta, line) in &scan.attributes {
            self.attribute(meta, *line);
        }
        for (name, line) in flow::token_method_calls(tokens) {
            self.method_call(name, line);
        }
        for test in scan.test_fns {
            let site = self.site(test.line);
            self.facts.tests.push(TestFn {
                site,
                name: test.name,
                body: if test_macro { test.body } else { None },
            });
        }
    }

    /// Records `meta` when it, or a `cfg_attr` branch of it, is `allow`, `expect`,
    /// `coverage` or `ignore`.
    fn attribute(&mut self, meta: &Meta, line: usize) {
        for meta in effective_metas(meta) {
            let Some(name) = meta.path().get_ident().map(paths::name) else {
                continue;
            };
            let lints = match (name.as_str(), &meta) {
                ("allow" | "expect", Meta::List(list)) => list
                    .parse_args_with(
                        syn::punctuated::Punctuated::<Meta, syn::Token![,]>::parse_terminated,
                    )
                    .map(|args| {
                        args.iter()
                            .filter_map(|m| match m {
                                Meta::Path(p) => Some(Written::from_syn(p).render()),
                                _ => None,
                            })
                            .collect()
                    })
                    .unwrap_or_else(|_| vec![list.tokens.to_string()]),
                ("coverage", Meta::List(list)) if list.tokens.to_string().contains("off") => {
                    vec!["coverage".to_owned()]
                }
                ("ignore", _) => Vec::new(),
                _ => continue,
            };
            let site = self.site(line);
            self.facts
                .attributes
                .push(AttributeUse { site, name, lints });
        }
    }

    /// Records `ident` as a hidden item when `attrs` hold `#[doc(hidden)]`.
    fn hidden(&mut self, attrs: &[Attribute], ident: &syn::Ident) {
        if is_doc_hidden(attrs) {
            let site = self.site(ident.span().start().line);
            self.facts.hidden.push(HiddenItem {
                site,
                name: paths::name(ident),
            });
        }
    }

    fn method_call(&mut self, name: String, line: usize) {
        let site = self.site(line);
        self.facts.method_calls.push(MethodCall {
            site,
            module: self.module.clone(),
            name,
        });
    }

    /// Records every `&mut` parameter of `sig`, with the candidates of the paths in its type.
    fn signature_mut_params(&mut self, sig: &syn::Signature) {
        let mut generics: Vec<String> = self.impl_generics.clone();
        generics.extend(sig.generics.type_params().map(|p| paths::name(&p.ident)));
        for input in &sig.inputs {
            let (paths, receiver, line, written) = match input {
                syn::FnArg::Receiver(r) => {
                    let paths = match &r.kind {
                        syn::ReceiverKind::Reference(_, _, Some(_)) => Some(Vec::new()),
                        syn::ReceiverKind::Typed(_, ty) => flow::mut_reference_paths(ty),
                        _ => None,
                    };
                    (
                        paths,
                        true,
                        r.self_token.span.start().line,
                        "self".to_owned(),
                    )
                }
                syn::FnArg::Typed(t) => {
                    let (name, line) = flow::pattern_name(&t.pat)
                        .unwrap_or_else(|| ("_".to_owned(), sig.ident.span().start().line));
                    (flow::mut_reference_paths(&t.ty), false, line, name)
                }
            };
            let Some(paths) = paths else {
                continue;
            };
            let generic = paths.iter().any(|p| {
                p.leading_colon.is_none()
                    && p.segments.len() == 1
                    && (p.segments[0].ident == "Self"
                        || generics.contains(&paths::name(&p.segments[0].ident)))
            });
            let resolved: Vec<Vec<String>> = paths
                .iter()
                .flat_map(|p| self.scopes.resolve(&self.module, &Written::from_syn(p)))
                .collect();
            let site = self.site(line);
            self.facts.mut_params.push(MutParam {
                site,
                module: self.module.clone(),
                function: paths::name(&sig.ident),
                written,
                receiver,
                generic: generic || receiver,
                resolved,
            });
        }
    }

    /// Records instrumentation that reaches a decision in `block`.
    fn decisions(&mut self, block: &syn::Block, output: &syn::ReturnType) {
        for (line, position, instrumentation) in
            flow::decisions(self.scopes, &self.module, block, output)
        {
            let site = self.site(line);
            self.facts.decisions.push(super::Decision {
                site,
                module: self.module.clone(),
                position,
                instrumentation,
            });
        }
    }

    /// Runs `f` with the test context switched on when `attrs` make the item test-only.
    fn scoped(&mut self, attrs: &[Attribute], f: impl FnOnce(&mut Self)) {
        let outer = self.test;
        self.test = outer || is_test_only(attrs);
        f(self);
        self.test = outer;
    }

    /// Visits the file an `include!` in expression or statement position names.
    fn include(&mut self, mac: &syn::Macro, line: usize) {
        let at = format!("{}:{line}", self.rel);
        let Some(path) = include_path(&mac.tokens) else {
            self.facts.problems.push((at, non_literal("include!")));
            return;
        };
        let file = self.file.parent().unwrap_or(Path::new("")).join(path);
        let rel = relative(self.root, &file);
        if self.depth > super::MAX_NESTING {
            self.facts
                .problems
                .push((at, format!("`{rel}` is included recursively")));
            return;
        }
        let parsed = fs::read_to_string(&file)
            .map_err(|e| format!("cannot read `{rel}`: {e}"))
            .and_then(|text| {
                syn::parse_file(&text)
                    .map(Included::Items)
                    .or_else(|_| syn::parse_str::<syn::Expr>(&text).map(Included::Expr))
                    .map(|included| (included, text))
                    .map_err(|e| format!("`{rel}` does not parse: {e}"))
            });
        match parsed {
            Ok((included, text)) => {
                self.facts.sources.insert(rel.clone(), text);
                let (outer_file, outer_rel) = (
                    std::mem::replace(&mut self.file, file),
                    std::mem::replace(&mut self.rel, rel),
                );
                self.depth += 1;
                match &included {
                    Included::Items(file) => {
                        for item in &file.items {
                            self.visit_item(item);
                        }
                    }
                    Included::Expr(expr) => self.visit_expr(expr),
                }
                self.depth -= 1;
                self.file = outer_file;
                self.rel = outer_rel;
            }
            Err(problem) => self.facts.problems.push((at, problem)),
        }
    }
}

enum Included {
    Items(syn::File),
    Expr(syn::Expr),
}

fn first_line(path: &syn::Path) -> usize {
    path.segments
        .first()
        .map_or(0, |s| s.ident.span().start().line)
}

impl<'ast> Visit<'ast> for Collector<'_> {
    fn visit_item(&mut self, item: &'ast Item) {
        let attrs: &[Attribute] = match item {
            Item::Const(i) => &i.attrs,
            Item::Enum(i) => &i.attrs,
            Item::ExternCrate(i) => &i.attrs,
            Item::Fn(i) => &i.attrs,
            Item::ForeignMod(i) => &i.attrs,
            Item::Impl(i) => &i.attrs,
            Item::Macro(i) => &i.attrs,
            Item::Mod(i) => &i.attrs,
            Item::Static(i) => &i.attrs,
            Item::Struct(i) => &i.attrs,
            Item::Trait(i) => &i.attrs,
            Item::TraitAlias(i) => &i.attrs,
            Item::Type(i) => &i.attrs,
            Item::Union(i) => &i.attrs,
            Item::Use(i) => &i.attrs,
            _ => &[],
        };
        self.scoped(attrs, |this| visit::visit_item(this, item));
    }

    fn visit_impl_item(&mut self, item: &'ast syn::ImplItem) {
        let attrs: &[Attribute] = match item {
            syn::ImplItem::Const(i) => &i.attrs,
            syn::ImplItem::Fn(i) => &i.attrs,
            syn::ImplItem::Type(i) => &i.attrs,
            syn::ImplItem::Macro(i) => &i.attrs,
            _ => &[],
        };
        self.scoped(attrs, |this| visit::visit_impl_item(this, item));
    }

    fn visit_trait_item(&mut self, item: &'ast syn::TraitItem) {
        let attrs: &[Attribute] = match item {
            syn::TraitItem::Const(i) => &i.attrs,
            syn::TraitItem::Fn(i) => &i.attrs,
            syn::TraitItem::Type(i) => &i.attrs,
            syn::TraitItem::Macro(i) => &i.attrs,
            _ => &[],
        };
        self.scoped(attrs, |this| visit::visit_trait_item(this, item));
    }

    fn visit_item_use(&mut self, item: &'ast syn::ItemUse) {
        visit::visit_item_use(self, item);
        self.use_item(item);
    }

    fn visit_item_extern_crate(&mut self, item: &'ast syn::ItemExternCrate) {
        visit::visit_item_extern_crate(self, item);
        self.extern_crate(item);
    }

    fn visit_path(&mut self, path: &'ast syn::Path) {
        self.reference(
            &Written::from_syn(path),
            first_line(path),
            RefKind::Path,
            false,
        );
        visit::visit_path(self, path);
    }

    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        let line = first_line(&mac.path);
        self.reference(&Written::from_syn(&mac.path), line, RefKind::Macro, false);
        for segment in &mac.path.segments {
            visit::visit_path_arguments(self, &segment.arguments);
        }
        if is_include(&mac.path) {
            self.include(mac, line);
        }
        if let Some(predicate) = platform_cfg(mac) {
            let site = self.site(line);
            self.facts.platform_cfgs.push((site, predicate));
        }
        let test_macro = mac
            .path
            .segments
            .last()
            .is_some_and(|s| TEST_MACROS.contains(&paths::name(&s.ident).as_str()));
        self.tokens(&mac.tokens, test_macro);
    }

    fn visit_attribute(&mut self, attr: &'ast Attribute) {
        self.attribute(&attr.meta, attr.pound_token.span.start().line);
        if let Meta::List(list) = &attr.meta {
            if list.path.is_ident("unsafe") {
                self.unsafe_site(first_line(&list.path), "an `#[unsafe(..)]` attribute");
            }
            self.tokens(&list.tokens, false);
        }
        visit::visit_attribute(self, attr);
    }

    fn visit_item_fn(&mut self, item: &'ast syn::ItemFn) {
        self.hidden(&item.attrs, &item.sig.ident);
        self.decisions(&item.block, &item.sig.output);
        if item.attrs.iter().any(is_test_attribute) {
            let site = self.site(item.sig.ident.span().start().line);
            let braces = item.block.brace_token.span;
            self.facts.tests.push(TestFn {
                site,
                name: paths::name(&item.sig.ident),
                body: Some((braces.open().start().line, braces.close().end().line)),
            });
        }
        visit::visit_item_fn(self, item);
    }

    fn visit_impl_item_fn(&mut self, item: &'ast syn::ImplItemFn) {
        self.hidden(&item.attrs, &item.sig.ident);
        self.decisions(&item.block, &item.sig.output);
        visit::visit_impl_item_fn(self, item);
    }

    fn visit_item_struct(&mut self, item: &'ast syn::ItemStruct) {
        self.hidden(&item.attrs, &item.ident);
        visit::visit_item_struct(self, item);
    }

    fn visit_item_enum(&mut self, item: &'ast syn::ItemEnum) {
        self.hidden(&item.attrs, &item.ident);
        visit::visit_item_enum(self, item);
    }

    fn visit_item_const(&mut self, item: &'ast syn::ItemConst) {
        self.hidden(&item.attrs, &item.ident);
        visit::visit_item_const(self, item);
    }

    fn visit_item_type(&mut self, item: &'ast syn::ItemType) {
        self.hidden(&item.attrs, &item.ident);
        visit::visit_item_type(self, item);
    }

    fn visit_item_static(&mut self, item: &'ast syn::ItemStatic) {
        self.hidden(&item.attrs, &item.ident);
        visit::visit_item_static(self, item);
    }

    fn visit_trait_item_fn(&mut self, item: &'ast syn::TraitItemFn) {
        if let Some(block) = &item.default {
            self.decisions(block, &item.sig.output);
        }
        visit::visit_trait_item_fn(self, item);
    }

    fn visit_expr_method_call(&mut self, expr: &'ast syn::ExprMethodCall) {
        self.method_call(paths::name(&expr.method), expr.method.span().start().line);
        visit::visit_expr_method_call(self, expr);
    }

    fn visit_expr_unsafe(&mut self, expr: &'ast syn::ExprUnsafe) {
        self.unsafe_site(expr.unsafe_token.span.start().line, "an `unsafe` block");
        visit::visit_expr_unsafe(self, expr);
    }

    fn visit_signature(&mut self, sig: &'ast syn::Signature) {
        if let syn::Safety::Unsafe(token) = &sig.safety {
            self.unsafe_site(token.span.start().line, "an `unsafe fn`");
        }
        self.signature_mut_params(sig);
        visit::visit_signature(self, sig);
    }

    fn visit_item_impl(&mut self, item: &'ast syn::ItemImpl) {
        if let Some(token) = &item.unsafety {
            self.unsafe_site(token.span.start().line, "an `unsafe impl`");
        }
        let outer = std::mem::replace(
            &mut self.impl_generics,
            item.generics
                .type_params()
                .map(|p| paths::name(&p.ident))
                .collect(),
        );
        visit::visit_item_impl(self, item);
        self.impl_generics = outer;
    }

    fn visit_item_trait(&mut self, item: &'ast syn::ItemTrait) {
        self.hidden(&item.attrs, &item.ident);
        if let Some(token) = &item.unsafety {
            self.unsafe_site(token.span.start().line, "an `unsafe trait`");
        }
        let outer = std::mem::replace(
            &mut self.impl_generics,
            item.generics
                .type_params()
                .map(|p| paths::name(&p.ident))
                .collect(),
        );
        visit::visit_item_trait(self, item);
        self.impl_generics = outer;
    }

    fn visit_item_foreign_mod(&mut self, item: &'ast syn::ItemForeignMod) {
        self.unsafe_site(item.abi.extern_token.span.start().line, "an `extern` block");
        visit::visit_item_foreign_mod(self, item);
    }
}

#[cfg(test)]
mod tests {
    use super::super::testing::facts_for;
    use super::*;

    fn written_in(facts: &Facts, context: Context) -> Vec<String> {
        facts
            .references
            .iter()
            .filter(|r| r.site.context == context)
            .map(|r| format!("{}={}", r.written, r.resolved[0].join("::")))
            .collect()
    }

    #[test]
    fn should_mark_test_context_when_code_is_cfg_test_or_a_test_function() {
        // given
        let source = "use std::fs;\npub fn f() { let _ = fs::read(\"a\"); }\n#[cfg(test)]\nmod tests {\n    fn helper() { let _ = std::env::args(); }\n}\n#[test]\nfn should_x_when_y() { let _ = std::process::id(); }\n";

        // when
        let facts = facts_for("p", &[("src/lib.rs", source)]);

        // then
        let production = written_in(&facts, Context::Production);
        let test = written_in(&facts, Context::Test);
        assert!(
            production.contains(&"fs::read=std::fs::read".to_owned()),
            "{production:?}"
        );
        assert!(
            test.contains(&"std::env::args=std::env::args".to_owned()),
            "{test:?}"
        );
        assert!(
            test.contains(&"std::process::id=std::process::id".to_owned()),
            "{test:?}"
        );
        assert!(
            !production
                .iter()
                .any(|r| r.contains("env") || r.contains("process"))
        );
    }

    #[test]
    fn should_record_suppressions_when_given_inner_outer_or_through_cfg_attr() {
        // given
        let source = "#![allow(dead_code)]\n#[cfg_attr(test, expect(clippy::unwrap_used, reason = \"x\"))]\nfn f() {}\n#[cfg_attr(coverage_nightly, coverage(off))]\nfn g() {}\nmacro_rules! m { () => { #[allow(unused)] fn h() {} } }\n";

        // when
        let facts = facts_for("p", &[("src/lib.rs", source)]);

        // then
        let got: Vec<(String, Vec<String>, usize)> = facts
            .attributes
            .iter()
            .map(|a| (a.name.clone(), a.lints.clone(), a.site.line))
            .collect();
        assert_eq!(
            got,
            [
                ("allow".to_owned(), vec!["dead_code".to_owned()], 1),
                (
                    "expect".to_owned(),
                    vec!["clippy::unwrap_used".to_owned()],
                    2
                ),
                ("coverage".to_owned(), vec!["coverage".to_owned()], 4),
                ("allow".to_owned(), vec!["unused".to_owned()], 6),
            ]
        );
    }

    #[test]
    fn should_read_bodies_only_when_tests_are_in_proptest() {
        // given
        let source = "proptest::proptest! {\n    #[test]\n    fn should_a_when_b(n in 0u8..2) {\n        // given\n    }\n}\nmy_tests! {\n    #[test]\n    fn should_c_when_d() {}\n}\n#[rstest]\nfn not_a_test() {}\n";

        // when
        let facts = facts_for("p", &[("src/lib.rs", source)]);

        // then
        let got: Vec<(String, Option<(usize, usize)>)> = facts
            .tests
            .iter()
            .map(|t| (t.name.clone(), t.body))
            .collect();
        assert_eq!(
            got,
            [
                ("should_a_when_b".to_owned(), Some((3, 5))),
                ("should_c_when_d".to_owned(), None)
            ]
        );
    }

    #[test]
    fn should_follow_include_when_it_is_in_expression_position() {
        // given
        let files = [
            (
                "src/lib.rs",
                "pub fn f() -> u8 {\n    include!(\"value.rs\")\n}\n",
            ),
            (
                "src/value.rs",
                "{ let _ = std::net::Ipv4Addr::LOCALHOST; 1 }\n",
            ),
        ];

        // when
        let facts = facts_for("p", &files);

        // then
        assert!(facts.problems.is_empty(), "{:?}", facts.problems);
        let hit = facts
            .references
            .iter()
            .find(|r| r.written == "std::net::Ipv4Addr::LOCALHOST")
            .map(|r| r.site.at());
        assert_eq!(hit.as_deref(), Some("crates/p/src/value.rs:1"));
        assert!(facts.sources.contains_key("crates/p/src/value.rs"));
    }

    #[test]
    fn should_record_unsafe_and_lone_crate_names_when_inside_macro_tokens() {
        // given
        let source = "macro_rules! m { () => { unsafe { } } }\npub fn f() { g!(tokio); }\n";

        // when
        let facts = facts_for("p", &[("src/lib.rs", source)]);

        // then
        assert_eq!(facts.unsafe_sites.len(), 1);
        assert!(
            facts
                .references
                .iter()
                .any(|r| r.written == "tokio" && r.resolved == [vec!["tokio".to_owned()]]),
            "{:?}",
            facts.references
        );
    }

    #[test]
    fn should_record_hidden_items_when_they_are_traits_type_aliases_or_statics() {
        // given: every kind of item `ARCH-hidden-seam` keeps to its users, and
        // a visible one of each
        let source = "#[doc(hidden)]\npub trait HiddenTrait {}\npub trait Shown {}\n#[cfg_attr(all(), doc(hidden))]\npub type HiddenAlias = u8;\npub type ShownAlias = u8;\n#[doc(hidden)]\npub static HIDDEN_STATIC: u8 = 0;\npub static SHOWN: u8 = 0;\n#[doc(hidden)]\npub fn hidden_fn() {}\n";

        // when
        let facts = facts_for("p", &[("src/lib.rs", source)]);

        // then
        let hidden: Vec<String> = facts
            .hidden
            .iter()
            .map(|item| format!("{}@{}", item.name, item.site.at()))
            .collect();
        assert_eq!(
            hidden,
            [
                "HiddenTrait@crates/p/src/lib.rs:2",
                "HiddenAlias@crates/p/src/lib.rs:5",
                "HIDDEN_STATIC@crates/p/src/lib.rs:8",
                "hidden_fn@crates/p/src/lib.rs:11",
            ]
        );
    }

    #[test]
    fn should_load_default_file_when_path_is_only_given_through_cfg_attr() {
        // given
        let files = [
            (
                "src/lib.rs",
                "#[cfg_attr(unix, path = \"u.rs\")]\nmod imp;\n#[cfg_attr(unix, path = \"gone.rs\")]\nmod other;\n",
            ),
            ("src/u.rs", ""),
            (
                "src/imp.rs",
                "pub fn f() { let _ = std::fs::read(\"a\"); }\n",
            ),
            ("src/other.rs", ""),
        ];

        // when
        let facts = facts_for("p", &files);

        // then
        assert!(
            facts
                .references
                .iter()
                .any(|r| r.site.at() == "crates/p/src/imp.rs:1"),
            "{:?}",
            facts.references
        );
        assert_eq!(facts.problems.len(), 1, "{:?}", facts.problems);
        assert!(facts.problems[0].1.contains("`crates/p/src/gone.rs`"));
    }
}
