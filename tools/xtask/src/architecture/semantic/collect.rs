//! Walks the units of one Cargo target twice: first to build the module scopes (`use`,
//! `extern crate`, items, child modules, also inside macro token streams), then to record
//! every path reference with its resolution, every `unsafe` site, and the declarations the
//! type rules read (statics, structs and enums, trait impls, the aggregate registry).

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use syn::visit::{self, Visit};
use syn::{Attribute, Item, Meta};

use super::flow;
use super::paths::{self, Scopes, TokenScan, Written, scan_tokens, use_leaves};
use super::rules_events;
use super::{
    AttributeUse, Context, Declaration, Facts, HiddenItem, INCLUDE_FAMILY, ImplDecl, IncludeKind,
    IncludeUse, METAVARIABLE_ATTRIBUTE, MacroFacts, MethodCall, MutParam, RefKind, Reference,
    SerdeAttr, Site, TestFn, TypeAlias, TypeDecl, TypeRef, Unit, effective_metas, foreign_message,
    include_path, is_doc_hidden, is_include, is_test_attribute, is_test_only, non_literal,
    outside_package, relative,
};

/// Where a target is: the workspace root, its package's directory, and whether it is the
/// package's library.
pub struct Target<'a> {
    /// The workspace root.
    pub root: &'a Path,
    /// The package directory; an `include!` outside it is refused.
    pub package_dir: &'a Path,
    /// Whether the target is a library.
    pub library: bool,
    /// What a `crate::` path of this target stands for in a declaration's item path: a
    /// library's crate name, or a key no path can spell for any other target.
    pub key: &'a str,
}

/// Collects the facts of one target into `facts`; returns its module scopes, which
/// [`super::follow_reexports`] reads for a library.
pub fn collect_target(
    target: &Target<'_>,
    units: &[Unit],
    renames: BTreeMap<String, String>,
    facts: &mut Facts,
) -> Scopes {
    let root = target.root;
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
            key: target.key,
            scopes: &scopes,
            module: unit.module.clone(),
            file: unit.file.clone(),
            rel: relative(root, &unit.file),
            test: unit.test,
            facts: &mut *facts,
            depth: 0,
            impl_generics: Vec::new(),
            package_dir: target.package_dir,
            library: target.library,
            registry: false,
            self_ty: None,
            defining: None,
        };
        for attr in &unit.attrs {
            collector.visit_attribute(attr);
        }
        for item in &unit.items {
            collector.visit_item(item);
        }
    }
    scopes
}

/// The impl whose `accept` lists the control plane's aggregates (`ARCH-aggregate-registered`).
const REGISTRY_IMPL: (&str, &str) = ("AggregateRegistry", "ControlAggregates");

/// The last segment of a type's path, or empty.
fn type_last(ty: &syn::Type) -> String {
    match ty {
        syn::Type::Path(p) => p
            .path
            .segments
            .last()
            .map(|s| paths::name(&s.ident))
            .unwrap_or_default(),
        syn::Type::Reference(r) => type_last(&r.elem),
        syn::Type::Paren(p) => type_last(&p.elem),
        syn::Type::Group(g) => type_last(&g.elem),
        _ => String::new(),
    }
}

/// Every path inside a syntax node: generic arguments, bounds, defaults, `dyn` and `impl`
/// traits and function pointer types included.
#[derive(Default)]
struct PathsIn(Vec<Written>);

impl<'ast> Visit<'ast> for PathsIn {
    fn visit_path(&mut self, path: &'ast syn::Path) {
        self.0.push(Written::from_syn(path));
        visit::visit_path(self, path);
    }
}

/// The names of the type parameters `generics` declares.
fn type_params(generics: &syn::Generics) -> BTreeSet<String> {
    generics
        .type_params()
        .map(|p| paths::name(&p.ident))
        .collect()
}

/// Every identifier in a type, generic arguments included.
fn type_idents(ty: &syn::Type) -> BTreeSet<String> {
    struct Idents(BTreeSet<String>);
    impl<'ast> Visit<'ast> for Idents {
        fn visit_path_segment(&mut self, segment: &'ast syn::PathSegment) {
            self.0.insert(paths::name(&segment.ident));
            visit::visit_path_segment(self, segment);
        }
    }
    let mut idents = Idents(BTreeSet::new());
    idents.visit_type(ty);
    idents.0
}

/// The last segment of every trait in a `derive`, also through `cfg_attr`.
fn derives(attrs: &[Attribute]) -> Vec<String> {
    let mut out = Vec::new();
    for attr in attrs {
        for meta in effective_metas(&attr.meta) {
            if let Meta::List(list) = &meta
                && list.path.is_ident("derive")
                && let Ok(paths) = list.parse_args_with(
                    syn::punctuated::Punctuated::<syn::Path, syn::Token![,]>::parse_terminated,
                )
            {
                out.extend(
                    paths
                        .iter()
                        .filter_map(|p| p.segments.last().map(|s| paths::name(&s.ident))),
                );
            }
        }
    }
    out
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
        if let Item::Type(alias) = item
            && let syn::Type::Path(target) = alias.ty.as_ref()
            && target.qself.is_none()
        {
            self.scopes.add_alias(
                &self.module,
                paths::name(&alias.ident),
                Written::from_syn(&target.path),
            );
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
    /// The target's key, for item paths ([`Target::key`]).
    key: &'a str,
    scopes: &'a Scopes,
    module: Vec<String>,
    file: PathBuf,
    rel: String,
    test: bool,
    facts: &'a mut Facts,
    depth: usize,
    /// Type parameters of the enclosing `impl` or `trait`.
    impl_generics: Vec<String>,
    /// The package directory: an `include!` outside it is refused.
    package_dir: &'a Path,
    /// Whether the target is a library.
    library: bool,
    /// Inside `impl AggregateRegistry for ControlAggregates`.
    registry: bool,
    /// The self type of the enclosing `impl`, when it is a plain path: what `Self` names.
    self_ty: Option<Written>,
    /// The name of the `macro_rules!` item being visited.
    defining: Option<String>,
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
        let (mut resolved, mut through) =
            self.scopes
                .resolve_through(&self.module, path, kind == RefKind::Macro);
        // `Self::now()` in `impl Trait for SystemTime` names `SystemTime::now`: the impl's
        // self type continued by the rest (pinned by
        // `should_report_domain_when_clock_or_fs_is_reached_through_alias_self_or_path`).
        if let Some(self_ty) = &self.self_ty
            && !path.absolute
            && path.segments.first().is_some_and(|s| s == "Self")
        {
            let mut segments = self_ty.segments.clone();
            segments.extend_from_slice(&path.segments[1..]);
            let substituted = Written {
                absolute: self_ty.absolute,
                segments,
            };
            let (more, more_through) =
                self.scopes
                    .resolve_through(&self.module, &substituted, false);
            for candidate in more {
                if !resolved.contains(&candidate) {
                    resolved.push(candidate);
                }
            }
            through.extend(more_through);
        }
        self.push(path, (resolved, through), line, kind, (open_tail, false));
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
            library: self.library,
        });
    }

    /// A path with a qualified self type (`<std::time::SystemTime>::now`,
    /// `<T as Trait>::f`): the self type's candidates continued by the segments after the
    /// trait, so the call is checked like `std::time::SystemTime::now`.
    fn qualified(&mut self, qself: Option<&syn::QSelf>, path: &syn::Path) {
        let Some(qself) = qself else {
            return;
        };
        let syn::Type::Path(base) = qself.ty.as_ref() else {
            return;
        };
        if base.qself.is_some() {
            return;
        }
        let written = Written::from_syn(&base.path);
        let tail: Vec<String> = path
            .segments
            .iter()
            .skip(qself.position)
            .map(|s| paths::name(&s.ident))
            .collect();
        if tail.is_empty() {
            return;
        }
        let (resolved, through) = self.scopes.resolve_through(&self.module, &written, false);
        let resolved: Vec<Vec<String>> = resolved
            .into_iter()
            .map(|mut candidate| {
                candidate.extend(tail.iter().cloned());
                candidate
            })
            .collect();
        let mut shown = written.clone();
        shown.segments.extend(tail);
        self.push(
            &shown,
            (resolved, through),
            first_line(&base.path),
            RefKind::Path,
            (false, false),
        );
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
        for (line, text) in &scan.open_attributes {
            let site = self.site(*line);
            self.facts.attributes.push(AttributeUse {
                site,
                name: METAVARIABLE_ATTRIBUTE.to_owned(),
                lints: vec![text.clone()],
            });
        }
        for line in &scan.macro_items {
            let site = self.site(*line);
            self.facts
                .nightly
                .push((site, "a macro 2.0 item (`macro name`)".to_owned()));
        }
        for (name, line) in flow::token_method_calls(tokens) {
            self.method_call(name, line);
        }
        for (name, line, variable) in token_macros(tokens, ENV_MACROS, first_literal) {
            let site = self.site(line);
            self.facts.env_macros.push((site, name, variable));
        }
        for (name, line, literal) in token_includes(tokens) {
            self.include_use(
                &name,
                &name,
                line,
                literal.as_deref(),
                IncludeKind::InTokens,
            );
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
            if name == "serde" {
                let site = self.site(line);
                self.facts.serde_attrs.push(SerdeAttr {
                    site,
                    keys: rules_events::serde_keys(&meta),
                });
                continue;
            }
            if name == "feature" {
                let site = self.site(line);
                let shown = match &meta {
                    Meta::List(list) => format!("#![feature({})]", list.tokens),
                    _ => "#![feature]".to_owned(),
                };
                self.facts.nightly.push((site, format!("`{shown}`")));
                continue;
            }
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

    /// Records a struct or enum with its derives, field identifiers and the paths its fields
    /// and generics name.
    fn type_decl(
        &mut self,
        attrs: &[Attribute],
        ident: &syn::Ident,
        (fields, refs): (BTreeSet<String>, Vec<TypeRef>),
        is_enum: bool,
    ) {
        let site = self.site(ident.span().start().line);
        self.facts.types.push(TypeDecl {
            site,
            name: paths::name(ident),
            path: self.item_path(ident),
            derives: derives(attrs),
            fields,
            refs,
            try_from: rules_events::container_try_from(attrs),
            is_enum,
            converted: rules_events::converted(attrs),
        });
    }

    /// The item path of `ident` declared in this module: the target's key, the module and
    /// the name, as a [`TypeRef`] candidate into this target spells it.
    fn item_path(&self, ident: &syn::Ident) -> Vec<String> {
        let mut path = vec![self.key.to_owned()];
        path.extend(self.module.iter().cloned());
        path.push(paths::name(ident));
        path
    }

    /// Each of `paths` with its candidates from this module, a `crate::` candidate keyed by
    /// the target. A lone name in `generics` is a type parameter of the item, not an item.
    fn type_refs(&self, paths: Vec<Written>, generics: &BTreeSet<String>) -> Vec<TypeRef> {
        let parameter = |p: &Written| match p.segments.as_slice() {
            [only] => !p.absolute && generics.contains(only),
            _ => false,
        };
        paths
            .into_iter()
            .filter(|p| !parameter(p))
            .filter_map(|p| {
                let last = p.segments.last()?.clone();
                let resolved = self
                    .scopes
                    .resolve(&self.module, &p)
                    .into_iter()
                    .map(|mut candidate| {
                        if let Some(first) = candidate.first_mut()
                            && *first == "crate"
                        {
                            self.key.clone_into(first);
                        }
                        candidate
                    })
                    .collect();
                Some(TypeRef { last, resolved })
            })
            .collect()
    }

    /// The identifiers and the [`TypeRef`]s of `types` and `generics`.
    fn declared_types<'t>(
        &self,
        types: impl Iterator<Item = &'t syn::Type>,
        generics: &syn::Generics,
    ) -> (BTreeSet<String>, Vec<TypeRef>) {
        let mut idents = BTreeSet::new();
        let mut found = PathsIn::default();
        for ty in types {
            idents.extend(type_idents(ty));
            found.visit_type(ty);
        }
        found.visit_generics(generics);
        (idents, self.type_refs(found.0, &type_params(generics)))
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

    /// Records an include-family macro (`include`, `include_str`, `include_bytes`) written as
    /// `written` at `line`, with the file its literal path names when that lies outside the
    /// package.
    fn include_use(
        &mut self,
        name: &str,
        written: &str,
        line: usize,
        literal: Option<&str>,
        kind: IncludeKind,
    ) {
        let outside = literal.and_then(|path| {
            let file = self.file.parent().unwrap_or(Path::new("")).join(path);
            outside_package(&file, self.package_dir)
                .then(|| foreign_message(&format!("`{name}!`"), self.root, &file))
        });
        let site = self.site(line);
        self.facts.includes.push(IncludeUse {
            site,
            name: name.to_owned(),
            written: written.to_owned(),
            kind,
            outside,
            literal: literal.is_some(),
        });
    }

    /// Records the declaration of `ident` as a `kind` item.
    fn declaration(&mut self, kind: &'static str, ident: &syn::Ident) {
        let site = self.site(ident.span().start().line);
        self.facts.declarations.push(Declaration {
            site,
            kind,
            name: paths::name(ident),
        });
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
        if outside_package(&file, self.package_dir) {
            self.facts
                .foreign
                .push((at, foreign_message("an `include!`", self.root, &file)));
            return;
        }
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

/// The include-family macros that read a file as data rather than compile it.
const DATA_INCLUDES: &[&str] = &["include_str", "include_bytes"];

/// The macros that read the build environment.
const ENV_MACROS: &[&str] = &["env", "option_env"];

/// Every include-family invocation (`include!`, `include_str!`, `include_bytes!`, under any
/// path) in a token stream, at any depth: its name, line and literal path, if it has one.
fn token_includes(tokens: &proc_macro2::TokenStream) -> Vec<(String, usize, Option<String>)> {
    token_macros(tokens, INCLUDE_FAMILY, include_path)
}

/// The first argument of a macro when it is a string literal (`env!("X", "message")`).
fn first_literal(tokens: &proc_macro2::TokenStream) -> Option<String> {
    let first: proc_macro2::TokenStream = tokens
        .clone()
        .into_iter()
        .take_while(|t| !matches!(t, proc_macro2::TokenTree::Punct(p) if p.as_char() == ','))
        .collect();
    include_path(&first)
}

/// Every invocation of a macro named in `names` (under any path) in a token stream, at any
/// depth: its name, line and what `argument` reads from its arguments.
fn token_macros(
    tokens: &proc_macro2::TokenStream,
    names: &[&str],
    argument: fn(&proc_macro2::TokenStream) -> Option<String>,
) -> Vec<(String, usize, Option<String>)> {
    let mut out = Vec::new();
    let trees: Vec<proc_macro2::TokenTree> = tokens.clone().into_iter().collect();
    for (i, tree) in trees.iter().enumerate() {
        match tree {
            proc_macro2::TokenTree::Group(group) => {
                out.extend(token_macros(&group.stream(), names, argument));
            }
            proc_macro2::TokenTree::Ident(ident)
                if names.contains(&paths::name(ident).as_str())
                    && matches!(trees.get(i + 1), Some(proc_macro2::TokenTree::Punct(p)) if p.as_char() == '!') =>
            {
                let value = match trees.get(i + 2) {
                    Some(proc_macro2::TokenTree::Group(group)) => argument(&group.stream()),
                    _ => None,
                };
                out.push((paths::name(ident), ident.span().start().line, value));
            }
            _ => {}
        }
    }
    out
}

/// The line of `macro` when `tokens` (an item `syn` could not parse) is a macro 2.0 item,
/// with or without a visibility: `macro m(..) {..}`, `pub(crate) macro m {..}`.
fn macro_item_line(tokens: &proc_macro2::TokenStream) -> Option<usize> {
    let trees: Vec<proc_macro2::TokenTree> = tokens.clone().into_iter().collect();
    let mut i = 0;
    if matches!(trees.first(), Some(proc_macro2::TokenTree::Ident(i)) if i == "pub") {
        i = 1;
        if matches!(trees.get(1), Some(proc_macro2::TokenTree::Group(g)) if g.delimiter() == proc_macro2::Delimiter::Parenthesis)
        {
            i = 2;
        }
    }
    match trees.get(i) {
        Some(proc_macro2::TokenTree::Ident(ident)) if ident == "macro" => {
            Some(ident.span().start().line)
        }
        _ => None,
    }
}

/// The kind and name of an item that declares one.
fn declared(item: &Item) -> Option<(&'static str, &syn::Ident)> {
    Some(match item {
        Item::Const(i) => ("const", &i.ident),
        Item::Enum(i) => ("enum", &i.ident),
        Item::Fn(i) => ("fn", &i.sig.ident),
        Item::Mod(i) => ("mod", &i.ident),
        Item::Static(i) => ("static", &i.ident),
        Item::Struct(i) => ("struct", &i.ident),
        Item::Trait(i) => ("trait", &i.ident),
        Item::TraitAlias(i) => ("trait", &i.ident),
        Item::Type(i) => ("type", &i.ident),
        Item::Union(i) => ("union", &i.ident),
        Item::Macro(i) => ("macro", i.ident.as_ref()?),
        _ => return None,
    })
}

/// The line of a `#[macro_export]` among `attrs`, also through `cfg_attr`.
fn macro_export_line(attrs: &[Attribute]) -> Option<usize> {
    attrs.iter().find_map(|attr| {
        effective_metas(&attr.meta)
            .iter()
            .any(|meta| meta.path().is_ident("macro_export"))
            .then(|| attr.pound_token.span.start().line)
    })
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
        self.scoped(attrs, |this| {
            if let Some((kind, ident)) = declared(item) {
                this.declaration(kind, ident);
            }
            if let Item::Verbatim(tokens) = item
                && let Some(line) = macro_item_line(tokens)
            {
                let site = this.site(line);
                this.facts
                    .nightly
                    .push((site, "a macro 2.0 item (`macro name`)".to_owned()));
            }
            if let Item::Macro(mac) = item
                && mac.ident.is_some()
                && let Some(line) = macro_export_line(&mac.attrs)
            {
                let site = this.site(line);
                this.facts.macro_exports.push(site);
            }
            let defining = match item {
                Item::Macro(mac) => mac.ident.as_ref().map(paths::name),
                _ => None,
            };
            let outer = std::mem::replace(&mut this.defining, defining);
            visit::visit_item(this, item);
            this.defining = outer;
        });
    }

    fn visit_impl_item(&mut self, item: &'ast syn::ImplItem) {
        let attrs: &[Attribute] = match item {
            syn::ImplItem::Const(i) => &i.attrs,
            syn::ImplItem::Fn(i) => &i.attrs,
            syn::ImplItem::Type(i) => &i.attrs,
            syn::ImplItem::Macro(i) => &i.attrs,
            _ => &[],
        };
        let declared = match item {
            syn::ImplItem::Const(i) => Some(("const", &i.ident)),
            syn::ImplItem::Fn(i) => Some(("fn", &i.sig.ident)),
            syn::ImplItem::Type(i) => Some(("type", &i.ident)),
            _ => None,
        };
        self.scoped(attrs, |this| {
            if let Some((kind, ident)) = declared {
                this.declaration(kind, ident);
            }
            visit::visit_impl_item(this, item);
        });
    }

    fn visit_trait_item(&mut self, item: &'ast syn::TraitItem) {
        let attrs: &[Attribute] = match item {
            syn::TraitItem::Const(i) => &i.attrs,
            syn::TraitItem::Fn(i) => &i.attrs,
            syn::TraitItem::Type(i) => &i.attrs,
            syn::TraitItem::Macro(i) => &i.attrs,
            _ => &[],
        };
        let declared = match item {
            syn::TraitItem::Const(i) => Some(("const", &i.ident)),
            syn::TraitItem::Fn(i) => Some(("fn", &i.sig.ident)),
            syn::TraitItem::Type(i) => Some(("type", &i.ident)),
            _ => None,
        };
        self.scoped(attrs, |this| {
            if let Some((kind, ident)) = declared {
                this.declaration(kind, ident);
            }
            visit::visit_trait_item(this, item);
        });
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
        if let Some(last) = mac.path.segments.last()
            && ENV_MACROS.contains(&paths::name(&last.ident).as_str())
        {
            let site = self.site(line);
            self.facts.env_macros.push((
                site,
                paths::name(&last.ident),
                first_literal(&mac.tokens),
            ));
        }
        if is_include(&mac.path) {
            self.include(mac, line);
        } else if let Some(last) = mac.path.segments.last()
            && DATA_INCLUDES.contains(&paths::name(&last.ident).as_str())
        {
            let name = paths::name(&last.ident);
            let literal = include_path(&mac.tokens);
            let written = Written::from_syn(&mac.path).render();
            self.include_use(
                &name,
                &written,
                line,
                literal.as_deref(),
                IncludeKind::Direct,
            );
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
        let defines = self.defining.take();
        let site = self.site(line);
        let found = rules_events::scan_macro(&mac.tokens, defines.is_some());
        let at = |line: usize| self.site(line);
        let impls = found
            .impls
            .into_iter()
            .map(|(line, trait_name, self_name)| super::TokenImpl {
                site: at(line),
                trait_name,
                self_name,
            })
            .collect();
        let coded = found
            .coded
            .into_iter()
            .map(|(line, name, try_from, is_enum)| super::CodedType {
                site: at(line),
                name,
                try_from,
                is_enum,
            })
            .collect();
        self.facts.macros.push(MacroFacts {
            site,
            name: mac
                .path
                .segments
                .last()
                .map(|s| paths::name(&s.ident))
                .unwrap_or_default(),
            defines,
            last_ident: found.last_ident,
            impls,
            coded,
        });
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
        let fields = self.declared_types(item.fields.iter().map(|f| &f.ty), &item.generics);
        self.type_decl(&item.attrs, &item.ident, fields, false);
        visit::visit_item_struct(self, item);
    }

    fn visit_item_enum(&mut self, item: &'ast syn::ItemEnum) {
        self.hidden(&item.attrs, &item.ident);
        let fields = self.declared_types(
            item.variants
                .iter()
                .flat_map(|v| v.fields.iter())
                .map(|f| &f.ty),
            &item.generics,
        );
        self.type_decl(&item.attrs, &item.ident, fields, true);
        visit::visit_item_enum(self, item);
    }

    fn visit_expr_path(&mut self, expr: &'ast syn::ExprPath) {
        self.qualified(expr.qself.as_ref(), &expr.path);
        visit::visit_expr_path(self, expr);
    }

    fn visit_type_path(&mut self, ty: &'ast syn::TypePath) {
        self.qualified(ty.qself.as_ref(), &ty.path);
        visit::visit_type_path(self, ty);
    }

    fn visit_item_const(&mut self, item: &'ast syn::ItemConst) {
        self.hidden(&item.attrs, &item.ident);
        visit::visit_item_const(self, item);
    }

    fn visit_item_type(&mut self, item: &'ast syn::ItemType) {
        self.hidden(&item.attrs, &item.ident);
        let (_, refs) = self.declared_types(std::iter::once(item.ty.as_ref()), &item.generics);
        let site = self.site(item.ident.span().start().line);
        self.facts.aliases.push(TypeAlias {
            site,
            path: self.item_path(&item.ident),
            refs,
        });
        visit::visit_item_type(self, item);
    }

    fn visit_item_static(&mut self, item: &'ast syn::ItemStatic) {
        self.hidden(&item.attrs, &item.ident);
        let site = self.site(item.ident.span().start().line);
        let mutable = matches!(item.mutability, syn::StaticMutability::Mut(_));
        self.facts.statics.push((site, mutable));
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
        if self.registry && expr.method == "visit" {
            for argument in expr.turbofish.iter().flat_map(|t| &t.args) {
                if let syn::GenericArgument::Type(ty) = argument {
                    self.facts.registry_visits.push(type_last(ty));
                }
            }
        }
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
        let mut registry = false;
        if let Some((path, _)) = &item.trait_
            && let Some(last) = path.segments.last()
        {
            let trait_name = paths::name(&last.ident);
            let self_name = type_last(&item.self_ty);
            registry = (trait_name.as_str(), self_name.as_str()) == REGISTRY_IMPL;
            let site = self.site(last.ident.span().start().line);
            let mut found = PathsIn::default();
            found.visit_type(&item.self_ty);
            let self_refs = self.type_refs(found.0, &type_params(&item.generics));
            self.facts.impls.push(ImplDecl {
                site,
                trait_name,
                self_name,
                self_refs,
            });
        }
        let self_ty = match item.self_ty.as_ref() {
            syn::Type::Path(p) if p.qself.is_none() => Some(Written::from_syn(&p.path)),
            _ => None,
        };
        let outer_self = std::mem::replace(&mut self.self_ty, self_ty);
        let outer_registry = std::mem::replace(&mut self.registry, registry);
        let outer = std::mem::replace(
            &mut self.impl_generics,
            item.generics
                .type_params()
                .map(|p| paths::name(&p.ident))
                .collect(),
        );
        visit::visit_item_impl(self, item);
        self.impl_generics = outer;
        self.registry = outer_registry;
        self.self_ty = outer_self;
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
