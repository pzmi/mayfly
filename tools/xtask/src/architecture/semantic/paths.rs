//! Name resolution for the semantic rules: `use` trees, `extern crate` aliases,
//! glob imports and re-exports across the modules of one Cargo target, and the conservative
//! path scanner for macro token streams that `syn` cannot parse as Rust.
//!
//! Resolution over-approximates: a name with several bindings yields every candidate, and a
//! glob import adds a candidate under its prefix. A rule fails when any candidate is
//! forbidden, so an ambiguity can only produce an extra finding, never hide one.

use std::collections::{BTreeMap, BTreeSet};

use proc_macro2::{Delimiter, Spacing, TokenStream, TokenTree};
use syn::{ItemExternCrate, ItemUse, UseTree};

/// An identifier as a name: `r#match` is `match`.
pub fn name(ident: &proc_macro2::Ident) -> String {
    let text = ident.to_string();
    match text.strip_prefix("r#") {
        Some(raw) => raw.to_owned(),
        None => text,
    }
}

/// How deep alias chains are followed before the path is taken as written.
const MAX_DEPTH: usize = 32;

/// A path as written: absolute (`::std::fs`) or relative to the module it appears in.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Written {
    /// Whether it starts with `::`.
    pub absolute: bool,
    /// Its segments, without generic arguments.
    pub segments: Vec<String>,
}

impl Written {
    /// The path as source text, for example `::std::fs`.
    pub fn render(&self) -> String {
        let joined = self.segments.join("::");
        if self.absolute {
            format!("::{joined}")
        } else {
            joined
        }
    }

    /// The path from a `syn` path, without generic arguments.
    pub fn from_syn(path: &syn::Path) -> Self {
        Self {
            absolute: path.leading_colon.is_some(),
            segments: path.segments.iter().map(|s| name(&s.ident)).collect(),
        }
    }
}

/// What one name in a module scope stands for.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Binding {
    /// `use <path> [as name]` or `extern crate`: resolved from the declaring module.
    Import(Written),
    /// A module of this target, by module path.
    Module(Vec<String>),
    /// An item defined in this target; a local path never reaches a forbidden root.
    Item,
}

#[derive(Debug, Default)]
struct Scope {
    bindings: BTreeMap<String, BTreeSet<Binding>>,
    globs: BTreeSet<Written>,
}

/// The scopes of every module of one Cargo target, keyed by module path (the crate root is
/// the empty path). A `use` inside a function body counts for its whole module.
#[derive(Debug, Default)]
pub struct Scopes {
    modules: BTreeMap<Vec<String>, Scope>,
    /// Dependency renames from the manifest: the name in code to the library name.
    renames: BTreeMap<String, String>,
}

impl Scopes {
    /// Scopes for a target whose manifest renames dependencies as in `renames`.
    pub fn new(renames: BTreeMap<String, String>) -> Self {
        Self {
            modules: BTreeMap::new(),
            renames,
        }
    }

    fn scope(&mut self, module: &[String]) -> &mut Scope {
        self.modules.entry(module.to_vec()).or_default()
    }

    fn bind(&mut self, module: &[String], name: String, binding: Binding) {
        self.scope(module)
            .bindings
            .entry(name)
            .or_default()
            .insert(binding);
    }

    /// Records a `use` item declared in `module`.
    pub fn add_use(&mut self, module: &[String], item: &ItemUse) {
        for leaf in use_leaves(item) {
            match leaf.name {
                Some(name) => self.bind(module, name, Binding::Import(leaf.path)),
                None => {
                    self.scope(module).globs.insert(leaf.path);
                }
            }
        }
    }

    /// Records an `extern crate` item declared in `module`.
    pub fn add_extern_crate(&mut self, module: &[String], item: &ItemExternCrate) {
        let crate_name = name(&item.ident);
        let local = item
            .rename
            .as_ref()
            .map_or_else(|| crate_name.clone(), |(_, rename)| name(rename));
        let binding = if crate_name == "self" {
            Binding::Module(Vec::new())
        } else {
            Binding::Import(Written {
                absolute: true,
                segments: vec![crate_name],
            })
        };
        self.bind(module, local, binding);
    }

    /// Records an item named `name` defined in `module`.
    pub fn add_item(&mut self, module: &[String], name: String) {
        self.bind(module, name, Binding::Item);
    }

    /// Records `type name = target;` in `module` as an import of `target` too, so `name::now`
    /// also resolves to `target::now` (`type T = std::time::SystemTime; T::now()`) and another
    /// crate's `other::Alias` follows it ([`super::follow_reexports`]). The item binding stays:
    /// both candidates are kept, so an alias can only add findings. Pinned by
    /// `should_resolve_type_alias_to_its_target_when_alias_is_used`.
    pub fn add_alias(&mut self, module: &[String], name: String, target: Written) {
        self.bind(module, name, Binding::Import(target));
    }

    /// Records the child module `name` of `module`.
    pub fn add_child(&mut self, module: &[String], name: &str) {
        let mut child = module.to_vec();
        child.push(name.to_owned());
        self.scope(&child);
        self.bind(module, name.to_owned(), Binding::Module(child));
    }

    /// Every absolute path `path`, written in `module`, may stand for. A path into this
    /// target starts with `crate`; any other starts with the crate it names. A one-segment
    /// path that no binding or glob explains is a local value and yields nothing.
    pub fn resolve(&self, module: &[String], path: &Written) -> Vec<Vec<String>> {
        self.resolve_through(module, path, false).0
    }

    /// The candidates of [`Scopes::resolve`], plus every module of this target the resolution passed through, as
    /// `crate::..` paths. A re-export keeps only its final target as a candidate, so a rule
    /// about naming a module (`crate::commands`) also looks at the modules on the way: a path
    /// through `crate::commands::X` names that module even when `X` is defined elsewhere.
    /// With `import` set, a lone unbound name is an extern crate, as in a `use` tree
    /// (`use tokio;`) or an `extern crate`.
    pub fn resolve_through(
        &self,
        module: &[String],
        path: &Written,
        import: bool,
    ) -> (Vec<Vec<String>>, Vec<Vec<String>>) {
        let mut walk = Walk::default();
        if !import && path.segments.len() == 1 && !path.absolute {
            self.lookup(module, &path.segments, Fallback::None, &mut walk);
        } else {
            self.resolve_into(module, path, &mut walk);
        }
        (
            walk.out.into_iter().collect(),
            walk.through.into_iter().collect(),
        )
    }

    fn resolve_into(&self, module: &[String], path: &Written, walk: &mut Walk) {
        if path.segments.is_empty() {
            return;
        }
        if path.absolute || walk.depth > MAX_DEPTH {
            walk.out.insert(self.external(&path.segments));
            return;
        }
        self.lookup(module, &path.segments, Fallback::Extern, walk);
    }

    /// Resolves `segments` against the scope of `module`, one level deeper.
    fn lookup(&self, module: &[String], segments: &[String], fallback: Fallback, walk: &mut Walk) {
        walk.depth += 1;
        self.lookup_here(module, segments, fallback, walk);
        walk.depth -= 1;
    }

    fn lookup_here(
        &self,
        module: &[String],
        segments: &[String],
        fallback: Fallback,
        walk: &mut Walk,
    ) {
        let Some((name, rest)) = segments.split_first() else {
            walk.out.insert(local(module, &[]));
            return;
        };
        if walk.depth > MAX_DEPTH {
            walk.out.insert(self.external(segments));
            return;
        }
        match name.as_str() {
            "crate" | "$crate" => return self.lookup(&[], rest, Fallback::Local, walk),
            "self" => return self.lookup(module, rest, Fallback::Local, walk),
            "super" => {
                let parent = &module[..module.len().saturating_sub(1)];
                return self.lookup(parent, rest, Fallback::Local, walk);
            }
            _ => {}
        }
        let scope = self.modules.get(module);
        let bindings = scope.and_then(|s| s.bindings.get(name));
        for binding in bindings.into_iter().flatten() {
            match binding {
                Binding::Import(target) => {
                    let mut full = target.segments.clone();
                    full.extend_from_slice(rest);
                    if !target.absolute && target.segments.first() == Some(name) {
                        // `use tokio::{self};` names the extern crate, not itself.
                        walk.out.insert(self.external(&full));
                    } else {
                        let full = Written {
                            absolute: target.absolute,
                            segments: full,
                        };
                        walk.depth += 1;
                        self.resolve_into(module, &full, walk);
                        walk.depth -= 1;
                    }
                }
                Binding::Module(path) => {
                    walk.through.insert(local(path, &[]));
                    self.lookup(path, rest, Fallback::Local, walk);
                }
                Binding::Item => {
                    walk.out.insert(local(module, segments));
                }
            }
        }
        if bindings.is_some_and(|b| !b.is_empty()) {
            return;
        }
        for glob in scope.into_iter().flat_map(|s| &s.globs) {
            // A glob applies once per chain: its own prefix is not looked up through it.
            let key = (module.to_vec(), glob.clone());
            if walk.globs.contains(&key) {
                continue;
            }
            let mut full = glob.segments.clone();
            full.extend_from_slice(segments);
            let full = Written {
                absolute: glob.absolute,
                segments: full,
            };
            walk.globs.push(key);
            walk.depth += 1;
            self.resolve_into(module, &full, walk);
            walk.depth -= 1;
            walk.globs.pop();
        }
        match fallback {
            Fallback::Extern => {
                walk.out.insert(self.external(segments));
            }
            Fallback::Local => {
                walk.out.insert(local(module, segments));
            }
            Fallback::None => {}
        }
    }

    /// An extern path, with a renamed dependency replaced by its library name.
    fn external(&self, segments: &[String]) -> Vec<String> {
        let mut out = segments.to_vec();
        if let Some(first) = out.first_mut()
            && let Some(name) = self.renames.get(first.as_str())
        {
            first.clone_from(name);
        }
        out
    }
}

/// The state of one resolution: depth, globs being expanded, and the candidates found.
#[derive(Default)]
struct Walk {
    depth: usize,
    globs: Vec<(Vec<String>, Written)>,
    out: BTreeSet<Vec<String>>,
    /// Modules of this target passed through, as `crate::..` paths.
    through: BTreeSet<Vec<String>>,
}

/// What an unbound first segment stands for.
#[derive(Debug, Clone, Copy)]
enum Fallback {
    /// An extern crate (edition 2018 paths).
    Extern,
    /// An item of this target that the scopes do not know.
    Local,
    /// A local value: no candidate.
    None,
}

fn local(module: &[String], rest: &[String]) -> Vec<String> {
    let mut out = vec!["crate".to_owned()];
    out.extend_from_slice(module);
    out.extend_from_slice(rest);
    out
}

/// One leaf of a `use` tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UseLeaf {
    /// The bound name; `None` for a glob.
    pub name: Option<String>,
    /// The imported path (for a glob, its prefix).
    pub path: Written,
    /// The line of the leaf's first segment.
    pub line: usize,
}

/// Every leaf of a `use` item: `use a::{b as c, d::*}` gives `c = a::b` and the glob `a::d`.
pub fn use_leaves(item: &ItemUse) -> Vec<UseLeaf> {
    let mut out = Vec::new();
    let line = item.use_token.span.start().line;
    walk_tree(
        &item.tree,
        &Written {
            absolute: item.leading_colon.is_some(),
            segments: Vec::new(),
        },
        line,
        &mut out,
    );
    out
}

fn walk_tree(tree: &UseTree, prefix: &Written, line: usize, out: &mut Vec<UseLeaf>) {
    let extend = |name: &syn::Ident| {
        let mut path = prefix.clone();
        if name != "self" {
            path.segments.push(self::name(name));
        }
        path
    };
    match tree {
        UseTree::Path(p) => {
            let line = if prefix.segments.is_empty() {
                p.ident.span().start().line
            } else {
                line
            };
            walk_tree(&p.tree, &extend(&p.ident), line, out);
        }
        UseTree::Name(n) => {
            let path = extend(&n.ident);
            let name = path.segments.last().cloned();
            out.push(UseLeaf { name, path, line });
        }
        UseTree::Rename(r) => out.push(UseLeaf {
            name: Some(name(&r.rename)),
            path: extend(&r.ident),
            line,
        }),
        UseTree::Glob(_) => out.push(UseLeaf {
            name: None,
            path: prefix.clone(),
            line,
        }),
        UseTree::Group(g) => {
            for tree in &g.items {
                walk_tree(tree, prefix, line, out);
            }
        }
    }
}

/// A path found in a token stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenPath {
    /// The path as written.
    pub path: Written,
    /// Line of its first token.
    pub line: usize,
    /// It continues with a macro metavariable (`std::$m`), so it may expand to anything
    /// below the written prefix.
    pub open_tail: bool,
    /// It starts with a macro metavariable (`$m::fs::read`): `path` holds the segments
    /// after it, which may follow any crate.
    pub open_head: bool,
    /// It is followed by `!`: a macro invocation.
    pub is_macro: bool,
}

/// What a scan of an unparsed token stream finds.
#[derive(Default)]
pub struct TokenScan {
    /// Every maximal path.
    pub paths: Vec<TokenPath>,
    /// `use` items found in the stream.
    pub uses: Vec<ItemUse>,
    /// `extern crate` items found in the stream.
    pub extern_crates: Vec<ItemExternCrate>,
    /// Lines of `unsafe` keywords.
    pub unsafe_lines: Vec<usize>,
    /// Attributes (`#[..]` and `#![..]`) that parse, with the line of their `#`.
    pub attributes: Vec<(syn::Meta, usize)>,
    /// Functions after a test attribute.
    pub test_fns: Vec<TokenTest>,
    /// Attributes whose name, or the name of a `cfg_attr` branch, is a macro metavariable
    /// (`#[$level(clippy::x)]`), with the line of their `#` and their text: the attribute
    /// the expansion writes cannot be read from the macro (`ARCH-lint-suppression`).
    pub open_attributes: Vec<(usize, String)>,
    /// Macro 2.0 items (`macro name(..) {..}`), by line (`ARCH-nightly-features`).
    pub macro_items: Vec<usize>,
    /// Metavariables bound only as a whole attribute (`#[$meta:meta]`): the caller writes
    /// the `#[..]`, where the scan of its arguments reads it, so `#[$meta]` forwards it.
    forwarded: BTreeSet<String>,
}

/// Scans `tokens` (a macro body or invocation) for paths, `use` and `extern crate` items and
/// `unsafe`, at every group depth.
pub fn scan_tokens(tokens: &TokenStream) -> TokenScan {
    let mut scan = TokenScan {
        forwarded: forwarded_metas(tokens),
        ..TokenScan::default()
    };
    scan_into(tokens, &mut scan);
    scan
}

/// The metavariables of `tokens` whose every binding (`$name:fragment`) is a whole attribute
/// of fragment `meta` (`#[$name:meta]`).
fn forwarded_metas(tokens: &TokenStream) -> BTreeSet<String> {
    fn walk(tokens: &TokenStream, all: &mut BTreeMap<String, (usize, usize)>) {
        let trees: Vec<TokenTree> = tokens.clone().into_iter().collect();
        for (i, tree) in trees.iter().enumerate() {
            match tree {
                TokenTree::Group(group) => {
                    let whole = group.delimiter() == Delimiter::Bracket
                        && i > 0
                        && is_punct(trees.get(i - 1), '#')
                        && binding(&group.stream().into_iter().collect::<Vec<_>>(), 0)
                            .is_some_and(|(_, fragment, end)| fragment == "meta" && end == 4);
                    if whole {
                        let inner: Vec<TokenTree> = group.stream().into_iter().collect();
                        if let Some((name, _, _)) = binding(&inner, 0) {
                            let entry = all.entry(name).or_default();
                            entry.0 += 1;
                            entry.1 += 1;
                        }
                    } else {
                        walk(&group.stream(), all);
                    }
                }
                TokenTree::Punct(p) if p.as_char() == '$' => {
                    if let Some((name, _, _)) = binding(&trees, i) {
                        all.entry(name).or_default().0 += 1;
                    }
                }
                _ => {}
            }
        }
    }
    let mut all = BTreeMap::new();
    walk(tokens, &mut all);
    all.into_iter()
        .filter(|(_, (bound, whole))| bound == whole)
        .map(|(name, _)| name)
        .collect()
}

/// The binding `$name:fragment` at `trees[i]`: its name, fragment and the index after it.
fn binding(trees: &[TokenTree], i: usize) -> Option<(String, String, usize)> {
    match (
        trees.get(i),
        trees.get(i + 1),
        trees.get(i + 2),
        trees.get(i + 3),
    ) {
        (
            Some(TokenTree::Punct(dollar)),
            Some(TokenTree::Ident(var)),
            Some(TokenTree::Punct(colon)),
            Some(TokenTree::Ident(fragment)),
        ) if dollar.as_char() == '$' && colon.as_char() == ':' && !is_path_sep(trees, i + 2) => {
            Some((name(var), name(fragment), i + 4))
        }
        _ => None,
    }
}

/// Whether the attribute `trees` (the inside of `#[..]`) has a metavariable for its name or
/// for the name of a `cfg_attr` branch, other than `$crate` and a whole forwarded `meta`.
fn metavariable_named(trees: &[TokenTree], forwarded: &BTreeSet<String>) -> bool {
    if binding(trees, 0).is_some_and(|(_, _, end)| end == trees.len()) {
        // `#[$name:meta]`: a matcher, not an attribute the expansion writes.
        return false;
    }
    if let [TokenTree::Punct(p), TokenTree::Ident(var)] = trees
        && p.as_char() == '$'
        && forwarded.contains(&name(var))
    {
        return false;
    }
    let head = trees
        .iter()
        .position(|t| {
            matches!(t, TokenTree::Group(_))
                || matches!(t, TokenTree::Punct(p) if matches!(p.as_char(), '=' | ','))
        })
        .unwrap_or(trees.len());
    let open = trees[..head].iter().enumerate().any(|(k, t)| {
        matches!(t, TokenTree::Punct(p) if p.as_char() == '$')
            && !matches!(trees.get(k + 1), Some(TokenTree::Ident(i)) if i == "crate")
    });
    if open {
        return true;
    }
    let cfg_attr = matches!(&trees[..head], [TokenTree::Ident(i)] if i == "cfg_attr");
    match trees.get(head) {
        Some(TokenTree::Group(group))
            if cfg_attr && group.delimiter() == Delimiter::Parenthesis =>
        {
            let inner: Vec<TokenTree> = group.stream().into_iter().collect();
            inner
                .split(|t| matches!(t, TokenTree::Punct(p) if p.as_char() == ','))
                .skip(1)
                .any(|branch| !branch.is_empty() && metavariable_named(branch, forwarded))
        }
        _ => false,
    }
}

fn scan_into(tokens: &TokenStream, scan: &mut TokenScan) {
    let trees: Vec<TokenTree> = tokens.clone().into_iter().collect();
    let mut i = 0;
    while i < trees.len() {
        match &trees[i] {
            TokenTree::Group(group) => {
                scan_into(&group.stream(), scan);
                i += 1;
            }
            TokenTree::Ident(ident) if ident == "unsafe" => {
                scan.unsafe_lines.push(ident.span().start().line);
                i += 1;
            }
            TokenTree::Ident(ident) if ident == "use" || ident == "extern" => {
                parse_item_at(&trees, i, scan);
                i += 1;
            }
            // `macro` is reserved: as a bare identifier it starts a macro 2.0 item.
            TokenTree::Ident(ident)
                if ident == "macro" && matches!(trees.get(i + 1), Some(TokenTree::Ident(_))) =>
            {
                scan.macro_items.push(ident.span().start().line);
                i += 1;
            }
            TokenTree::Punct(p) if p.as_char() == '#' => {
                attribute_at(&trees, i, scan);
                i += 1;
            }
            _ => match path_at(&trees, i) {
                Some((path, next)) => {
                    scan.paths.push(path);
                    i = next;
                }
                None => i += 1,
            },
        }
    }
}

/// Records the attribute whose `#` is at `start`, and the test function it may mark.
fn attribute_at(trees: &[TokenTree], start: usize, scan: &mut TokenScan) {
    let line = trees[start].span().start().line;
    let bang = usize::from(is_punct(trees.get(start + 1), '!'));
    let Some(TokenTree::Group(group)) = trees.get(start + 1 + bang) else {
        return;
    };
    if group.delimiter() != Delimiter::Bracket {
        return;
    }
    let inner: Vec<TokenTree> = group.stream().into_iter().collect();
    if metavariable_named(&inner, &scan.forwarded) {
        scan.open_attributes
            .push((line, group.stream().to_string()));
        return;
    }
    let Ok(meta) = syn::parse2::<syn::Meta>(group.stream()) else {
        return;
    };
    let is_test = super::is_test_path(meta.path());
    scan.attributes.push((meta, line));
    if !is_test {
        return;
    }
    let after = start + 2 + bang;
    let fn_at = trees[after..]
        .iter()
        .take(16)
        .position(|t| matches!(t, TokenTree::Ident(i) if i == "fn"))
        .map(|k| after + k);
    let name = fn_at.and_then(|k| match trees.get(k + 1) {
        Some(TokenTree::Ident(ident)) => Some(name(ident)),
        _ => None,
    });
    // The body is the first brace group after the name (parameters and a return type,
    // which have no braces in the test macros the checker reads, come before it).
    let body = fn_at.and_then(|k| {
        trees[k..].iter().take(64).find_map(|t| match t {
            TokenTree::Group(g) if g.delimiter() == Delimiter::Brace => {
                Some((g.span_open().start().line, g.span_close().end().line))
            }
            _ => None,
        })
    });
    scan.test_fns.push(TokenTest {
        name: name.unwrap_or_else(|| "<generated>".to_owned()),
        line,
        body,
    });
}

/// A function after a test attribute in a token stream.
pub struct TokenTest {
    /// Its name, or `<generated>` when none follows.
    pub name: String,
    /// Line of the attribute.
    pub line: usize,
    /// First and last line of its body, when one follows.
    pub body: Option<(usize, usize)>,
}

/// Parses `use ...;` or `extern crate ...;` starting at `start`, if it is one.
fn parse_item_at(trees: &[TokenTree], start: usize, scan: &mut TokenScan) {
    let Some(end) = trees[start..]
        .iter()
        .position(|t| matches!(t, TokenTree::Punct(p) if p.as_char() == ';'))
    else {
        return;
    };
    let stream: TokenStream = trees[start..=start + end].iter().cloned().collect();
    if let Ok(item) = syn::parse2::<ItemUse>(stream.clone()) {
        scan.uses.push(item);
    } else if let Ok(item) = syn::parse2::<ItemExternCrate>(stream) {
        scan.extern_crates.push(item);
    }
}

fn is_punct(tree: Option<&TokenTree>, ch: char) -> bool {
    matches!(tree, Some(TokenTree::Punct(p)) if p.as_char() == ch)
}

/// Whether `trees[i..]` starts with `::`.
fn is_path_sep(trees: &[TokenTree], i: usize) -> bool {
    matches!(trees.get(i), Some(TokenTree::Punct(p)) if p.as_char() == ':' && p.spacing() == Spacing::Joint)
        && is_punct(trees.get(i + 1), ':')
}

/// The path starting at `trees[i]`, if one starts there, and the index after it.
fn path_at(trees: &[TokenTree], i: usize) -> Option<(TokenPath, usize)> {
    let previous = i.checked_sub(1).and_then(|p| trees.get(p));
    let mut open_head = false;
    let (absolute, mut j, line, mut segments) = match &trees[i] {
        TokenTree::Ident(ident) => {
            let name = name(ident);
            let after_dot_or_dollar = is_punct(previous, '.') || is_punct(previous, '$');
            let is_lifetime = is_punct(previous, '\'');
            // After `::` it continues a path the scan could not follow (`$m::x`, `>::x`).
            let continues = i >= 2 && is_path_sep(trees, i - 2);
            if after_dot_or_dollar || is_lifetime || continues || is_keyword(&name) {
                return None;
            }
            (false, i + 1, ident.span().start().line, vec![name])
        }
        TokenTree::Punct(p) if p.as_char() == '$' => match trees.get(i + 1) {
            Some(TokenTree::Ident(ident)) if ident == "crate" => (
                false,
                i + 2,
                ident.span().start().line,
                vec!["$crate".to_owned()],
            ),
            // `$m::fs::read`: the head is whatever the caller passes.
            Some(TokenTree::Ident(ident)) if is_path_sep(trees, i + 2) => match trees.get(i + 4) {
                Some(TokenTree::Ident(next)) => {
                    open_head = true;
                    (false, i + 5, ident.span().start().line, vec![name(next)])
                }
                _ => return None,
            },
            _ => return None,
        },
        TokenTree::Punct(_) if is_path_sep(trees, i) => {
            let continues = matches!(previous, Some(TokenTree::Ident(_)))
                || is_punct(previous, '>')
                || is_punct(previous, ':');
            match trees.get(i + 2) {
                Some(TokenTree::Ident(ident)) if !continues => {
                    (true, i + 3, ident.span().start().line, vec![name(ident)])
                }
                _ => return None,
            }
        }
        _ => return None,
    };
    let mut open_tail = false;
    while is_path_sep(trees, j) {
        match (trees.get(j + 2), trees.get(j + 3)) {
            (Some(TokenTree::Ident(ident)), _) => {
                segments.push(name(ident));
                j += 3;
            }
            (Some(TokenTree::Punct(p)), Some(TokenTree::Ident(_))) if p.as_char() == '$' => {
                open_tail = true;
                j += 4;
                break;
            }
            _ => break,
        }
    }
    let is_macro = matches!(trees.get(j), Some(TokenTree::Punct(p)) if p.as_char() == '!' && p.spacing() == Spacing::Alone)
        || matches!(
            (trees.get(j), trees.get(j + 1)),
            (Some(TokenTree::Punct(p)), Some(TokenTree::Group(g)))
                if p.as_char() == '!' && g.delimiter() != Delimiter::None
        );
    Some((
        TokenPath {
            path: Written { absolute, segments },
            line,
            open_tail,
            open_head,
            is_macro,
        },
        j,
    ))
}

/// Rust keywords that never start a path (`self`, `super`, `crate` and `Self` do).
fn is_keyword(name: &str) -> bool {
    matches!(
        name,
        "as" | "async"
            | "await"
            | "break"
            | "const"
            | "continue"
            | "dyn"
            | "else"
            | "enum"
            | "extern"
            | "false"
            | "fn"
            | "for"
            | "gen"
            | "if"
            | "impl"
            | "in"
            | "let"
            | "loop"
            | "match"
            | "mod"
            | "move"
            | "mut"
            | "pub"
            | "ref"
            | "return"
            | "static"
            | "struct"
            | "trait"
            | "true"
            | "type"
            | "unsafe"
            | "use"
            | "where"
            | "while"
            | "yield"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn written(path: &str) -> Written {
        let absolute = path.starts_with("::");
        Written {
            absolute,
            segments: path
                .trim_start_matches("::")
                .split("::")
                .map(str::to_owned)
                .collect(),
        }
    }

    fn joined(paths: Vec<Vec<String>>) -> Vec<String> {
        paths.into_iter().map(|p| p.join("::")).collect()
    }

    /// Scopes for one file, with every inline module registered.
    fn scopes_of(source: &str) -> Scopes {
        fn walk(scopes: &mut Scopes, module: &[String], items: &[syn::Item]) {
            for item in items {
                match item {
                    syn::Item::Use(u) => scopes.add_use(module, u),
                    syn::Item::ExternCrate(e) => scopes.add_extern_crate(module, e),
                    syn::Item::Mod(m) => {
                        scopes.add_child(module, &m.ident.to_string());
                        let mut child = module.to_vec();
                        child.push(m.ident.to_string());
                        if let Some((_, items)) = &m.content {
                            walk(scopes, &child, items);
                        }
                    }
                    syn::Item::Struct(s) => scopes.add_item(module, s.ident.to_string()),
                    _ => {}
                }
            }
        }
        let file = syn::parse_file(source).unwrap();
        let mut scopes = Scopes::new(BTreeMap::from([(
            "fsa".to_owned(),
            "mayfly_adapter_sqlite".to_owned(),
        )]));
        walk(&mut scopes, &[], &file.items);
        scopes
    }

    #[test]
    fn should_resolve_alias_when_module_is_renamed_by_use() {
        // given
        let scopes = scopes_of("use std::fs as f;");

        // when
        let resolved = scopes.resolve(&[], &written("f::read"));

        // then
        assert_eq!(joined(resolved), ["std::fs::read"]);
    }

    #[test]
    fn should_resolve_reexport_when_alias_is_declared_in_another_module() {
        // given
        let scopes = scopes_of(
            "mod a { pub use std::fs as f; } mod b { use crate::a::f as g; } mod c { use super::a; }",
        );
        let b = vec!["b".to_owned()];
        let c = vec!["c".to_owned()];

        // when
        let via_b = scopes.resolve(&b, &written("g::write"));
        let via_c = scopes.resolve(&c, &written("a::f::read"));

        // then
        assert_eq!(joined(via_b), ["std::fs::write"]);
        assert_eq!(joined(via_c), ["std::fs::read"]);
    }

    #[test]
    fn should_resolve_associated_function_when_type_is_imported() {
        // given
        let scopes = scopes_of("use std::time::{self, Instant};");

        // when
        let now = scopes.resolve(&[], &written("Instant::now"));
        let module = scopes.resolve(&[], &written("time::SystemTime::now"));

        // then
        assert_eq!(joined(now), ["std::time::Instant::now"]);
        assert_eq!(joined(module), ["std::time::SystemTime::now"]);
    }

    #[test]
    fn should_resolve_type_alias_to_its_target_when_alias_is_used() {
        // given
        let mut scopes = scopes_of("use std::time::SystemTime;");
        scopes.add_item(&[], "T".to_owned());
        scopes.add_alias(&[], "T".to_owned(), written("SystemTime"));

        // when
        let resolved = scopes.resolve(&[], &written("T::now"));

        // then
        assert_eq!(
            joined(resolved),
            ["crate::T::now", "std::time::SystemTime::now"]
        );
    }

    #[test]
    fn should_add_glob_candidate_when_name_is_unbound() {
        // given
        let scopes = scopes_of("use std::thread::*;");

        // when
        let resolved = scopes.resolve(&[], &written("sleep"));

        // then
        assert_eq!(joined(resolved), ["std::thread::sleep"]);
    }

    #[test]
    fn should_resolve_through_alias_when_extern_crate_or_dependency_is_renamed() {
        // given
        let scopes = scopes_of("extern crate std as s; use fsa::Root;");

        // when
        let via_extern = scopes.resolve(&[], &written("s::net::TcpStream"));
        let via_rename = scopes.resolve(&[], &written("Root"));

        // then
        assert_eq!(joined(via_extern), ["std::net::TcpStream"]);
        assert_eq!(joined(via_rename), ["mayfly_adapter_sqlite::Root"]);
    }

    #[test]
    fn should_treat_name_as_local_value_when_lone_and_unbound() {
        // given
        let scopes = scopes_of("struct Local;");

        // when
        let value = scopes.resolve(&[], &written("tokio"));
        let import = scopes.resolve_through(&[], &written("tokio"), true).0;
        let item = scopes.resolve(&[], &written("Local::new"));

        // then
        assert!(value.is_empty(), "{value:?}");
        assert_eq!(joined(import), ["tokio"]);
        assert_eq!(joined(item), ["crate::Local::new"]);
    }

    #[test]
    fn should_stop_when_aliases_form_a_cycle() {
        // given
        let scopes = scopes_of("use self::b as a; use self::a as b; use tokio::{self};");

        // when
        let cycle = scopes.resolve(&[], &written("a::x"));
        let own = scopes.resolve(&[], &written("tokio::spawn"));

        // then
        assert!(!cycle.is_empty());
        assert_eq!(joined(own), ["tokio::spawn"]);
    }

    #[test]
    fn should_record_attributes_named_by_a_metavariable_when_tokens_are_scanned() {
        // given: lint levels named by metavariables, directly and through `cfg_attr`, next
        // to the forms that stay readable (a forwarded whole `meta`, `$crate`, a matcher)
        let tokens: TokenStream = "\
($l:ident, $(#[$m:meta])*) => {\n\
#[$l(clippy::disallowed_methods)] fn a() {}\n\
#[cfg_attr(all(), $l(unused))] fn b() {}\n\
#![$l(clippy::all)]\n\
$(#[$m])* fn c() {}\n\
#[$crate::marker] fn d() {}\n\
#[doc = $l] fn e() {}\n\
}"
        .parse()
        .unwrap();
        let rebound: TokenStream = "(#[$m:meta]) => {}; ($m:ident) => { #[$m] fn f() {} }"
            .parse()
            .unwrap();

        // when
        let scan = scan_tokens(&tokens);
        let other = scan_tokens(&rebound);

        // then
        let lines: Vec<usize> = scan.open_attributes.iter().map(|(l, _)| *l).collect();
        assert_eq!(lines, [2, 3, 4], "{:?}", scan.open_attributes);
        assert_eq!(
            other.open_attributes.len(),
            1,
            "{:?}",
            other.open_attributes
        );
    }

    #[test]
    fn should_record_macro_items_when_tokens_hold_macros_two_point_zero() {
        // given
        let tokens: TokenStream = "() => { pub macro m($x:expr) { $x } fn r#macro() {} }"
            .parse()
            .unwrap();

        // when
        let scan = scan_tokens(&tokens);

        // then
        assert_eq!(scan.macro_items, [1]);
    }

    #[test]
    fn should_find_paths_uses_and_open_tails_when_tokens_are_scanned() {
        // given
        let tokens: TokenStream = "($m:ident) => {{ use std::{fs as f}; tokio::spawn(x.read()); std::$m::read(); $crate::inner!(); ::core::mem::drop(1); unsafe { } }}"
            .parse()
            .unwrap();

        // when
        let scan = scan_tokens(&tokens);

        // then
        let paths: Vec<String> = scan.paths.iter().map(|p| p.path.render()).collect();
        assert!(paths.contains(&"tokio::spawn".to_owned()), "{paths:?}");
        assert!(!paths.contains(&"read".to_owned()), "{paths:?}");
        assert!(paths.contains(&"::core::mem::drop".to_owned()), "{paths:?}");
        let open: Vec<String> = scan
            .paths
            .iter()
            .filter(|p| p.open_tail)
            .map(|p| p.path.render())
            .collect();
        assert_eq!(open, ["std"]);
        let macros: Vec<String> = scan
            .paths
            .iter()
            .filter(|p| p.is_macro)
            .map(|p| p.path.render())
            .collect();
        assert_eq!(macros, ["$crate::inner"]);
        assert_eq!(scan.uses.len(), 1);
        assert_eq!(scan.unsafe_lines, [1]);
    }

    #[test]
    fn should_list_every_leaf_when_use_tree_is_grouped() {
        // given
        let item: ItemUse =
            syn::parse_str("use ::std::{fs as f, time::{self, Instant}, io::*};").unwrap();

        // when
        let leaves = use_leaves(&item);

        // then
        let rendered: Vec<(Option<String>, String)> = leaves
            .into_iter()
            .map(|l| (l.name, l.path.render()))
            .collect();
        assert_eq!(
            rendered,
            [
                (Some("f".to_owned()), "::std::fs".to_owned()),
                (Some("time".to_owned()), "::std::time".to_owned()),
                (
                    Some("Instant".to_owned()),
                    "::std::time::Instant".to_owned()
                ),
                (None, "::std::io".to_owned()),
            ]
        );
    }
}
