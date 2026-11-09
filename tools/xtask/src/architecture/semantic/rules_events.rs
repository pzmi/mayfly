//! The rules behind the event shape tracer (ADR 0018, foundation.md section 3.3): what the
//! tracer in `mayfly_control_domain::shape` cannot see is kept out of the code it traces.
//!
//! - `ARCH-event-payload-serde`: production code of `mayfly-control-domain`, where events are
//!   declared, writes no serde attribute that codes through a conversion, a module or a
//!   function (`try_from`, `from`, `into`, `with`, `deserialize_with`, `serialize_with`),
//!   accepts a second name (`alias`), hides variants behind a catch-all (`other`) or needs a
//!   self-describing format (`untagged`, `tag`, `content`, `flatten`), and no hand-written
//!   `Serialize` or `Deserialize` impl. An enum coded as a string or an integer traces as a
//!   plain leaf whose variants no sample must show; this rule is what keeps one out. It covers
//!   every type of the crate, not only those an `event_set!` reaches: the crate holds events and
//!   the types they carry, and a reachability analysis by name could miss exactly the type that
//!   matters. `mayfly-domain`, whose value types events carry, is held to the same rule with one
//!   exception: a struct may convert (`try_from`, `from`, `into`), because its validated value
//!   types decode only through their parsers; they are covered by the next rule. An enum there
//!   may not. The edge table lets `mayfly-control-domain` depend on `mayfly-domain` alone
//!   (foundation.md section 2.2), so the two crates hold every workspace type an event carries.
//! - `ARCH-sample-coverage`: every type of `mayfly-domain` decoded through a container
//!   `#[serde(try_from = "..")]` from anything but an integer implements
//!   `mayfly_domain::Sample` and is listed in `texts()` of `mayfly-control-domain`'s
//!   `src/shape.rs`, so the tracer can offer it a value it accepts. Integer-coded types
//!   (`Generation`) need neither: the tracer offers an integer leaf its type's maximum.
//!   Types declared by a macro (`hex_id!(AttemptId)`) are resolved through the macro's
//!   invocations; one whose name cannot be resolved is reported, never passed.
//! - `ARCH-event-set-macro`: production code implements `EventSet` only in the body of
//!   `macro_rules! event_set` in `mayfly-control-domain/src/event.rs`, so every event set has
//!   the generated `shape()` and codec; test code may write one by hand (the harness's own
//!   negative cases do).
//!
//! Matching is by name, like the other declaration rules: a type, trait or macro is not
//! resolved. Each rule is pinned by its negative fixtures (`event-payload-try-from-enum`,
//! `event-payload-alias-field`, `event-payload-flatten`, `validated-type-without-sample`,
//! `event-set-by-hand`) and by the unit tests at the end of this file.

use std::collections::{BTreeMap, BTreeSet};

use proc_macro2::{Delimiter, Spacing, TokenStream, TokenTree};
use syn::visit::{self, Visit};
use syn::{Attribute, Meta};

use super::super::report::{Diagnostic, Rule};
use super::{Context, Facts, MacroFacts, Site, effective_metas, paths};

/// The crate that declares events (`event_set!`) and the shape tracer.
pub const EVENTS_CRATE: &str = "mayfly-control-domain";

/// The crate of the validated value types events carry.
pub const VALUES_CRATE: &str = "mayfly-domain";

/// The key recorded for a `#[serde(..)]` whose arguments do not parse.
pub const UNREADABLE: &str = "<unreadable>";

/// The crates [`REFUSED`] applies to, and whether a struct there may convert.
const PAYLOAD_CRATES: &[(&str, bool)] = &[(EVENTS_CRATE, false), (VALUES_CRATE, true)];

/// The container keys that code a type through a conversion.
const CONVERSIONS: &[&str] = &["try_from", "from", "into"];

/// Serde keys refused in the production code of [`PAYLOAD_CRATES`], each with why.
const REFUSED: &[(&str, &str)] = &[
    ("try_from", "decodes through a conversion"),
    ("from", "decodes through a conversion"),
    ("into", "encodes through a conversion"),
    ("with", "codes through a module"),
    ("deserialize_with", "decodes through a function"),
    ("serialize_with", "encodes through a function"),
    ("alias", "accepts a second name"),
    ("other", "decodes an unknown variant into a catch-all"),
    ("untagged", "needs a self-describing format"),
    ("tag", "needs a self-describing format"),
    ("content", "needs a self-describing format"),
    ("flatten", "needs a self-describing format"),
    (UNREADABLE, "cannot be read"),
];

/// Hand-written impls refused in the production code of [`PAYLOAD_CRATES`].
const SERDE_TRAITS: &[&str] = &["Serialize", "Deserialize"];

/// `try_from` sources the tracer satisfies with its integer samples.
const INTEGERS: &[&str] = &[
    "u8", "u16", "u32", "u64", "u128", "usize", "i8", "i16", "i32", "i64", "i128", "isize",
];

/// The macro whose body alone may implement `EventSet`, and its file in [`EVENTS_CRATE`].
const EVENT_SET: (&str, &str) = ("event_set", "src/event.rs");

/// The tracer's file in [`EVENTS_CRATE`] and its list of sampled value types.
const SHAPE: (&str, &str) = ("src/shape.rs", "texts");

/// The diagnostics of the three rules over every package.
pub fn check(every: &[Facts]) -> Vec<Diagnostic> {
    let mut out = payload_serde(every);
    out.extend(sample_coverage(every));
    out.extend(event_set_macro(every));
    out.sort();
    out.dedup();
    out
}

/// The key and literal value of each argument of a `#[serde(..)]` meta.
pub fn serde_keys(meta: &Meta) -> Vec<(String, Option<String>)> {
    let Meta::List(list) = meta else {
        return Vec::new();
    };
    let Ok(args) =
        list.parse_args_with(syn::punctuated::Punctuated::<Meta, syn::Token![,]>::parse_terminated)
    else {
        return vec![(UNREADABLE.to_owned(), None)];
    };
    args.iter()
        .map(|arg| {
            let key = arg
                .path()
                .segments
                .iter()
                .map(|s| paths::name(&s.ident))
                .collect::<Vec<_>>()
                .join("::");
            let value = match arg {
                Meta::NameValue(nv) => match &nv.value {
                    syn::Expr::Lit(syn::ExprLit {
                        lit: syn::Lit::Str(text),
                        ..
                    }) => Some(text.value()),
                    _ => Some(UNREADABLE.to_owned()),
                },
                _ => None,
            };
            (key, value)
        })
        .collect()
}

/// The source type of a container `#[serde(try_from = "..")]` among `attrs`, through
/// `cfg_attr` too.
pub fn container_try_from(attrs: &[Attribute]) -> Option<String> {
    attrs
        .iter()
        .flat_map(|attr| effective_metas(&attr.meta))
        .find_map(|meta| try_from_of(&meta))
}

/// Whether a container `#[serde(..)]` among `attrs` names a conversion ([`CONVERSIONS`]).
pub fn converted(attrs: &[Attribute]) -> bool {
    attrs
        .iter()
        .flat_map(|attr| effective_metas(&attr.meta))
        .filter(|meta| meta.path().is_ident("serde"))
        .any(|meta| {
            serde_keys(&meta)
                .iter()
                .any(|(key, _)| CONVERSIONS.contains(&key.as_str()))
        })
}

fn try_from_of(meta: &Meta) -> Option<String> {
    if !meta.path().is_ident("serde") {
        return None;
    }
    serde_keys(meta)
        .into_iter()
        .find(|(key, _)| key == "try_from")
        .map(|(_, value)| value.unwrap_or_else(|| UNREADABLE.to_owned()))
}

/// What [`scan_macro`] finds in a macro's tokens, by line.
#[derive(Debug, Default)]
pub struct MacroScan {
    /// Trait impls: line, trait, self type (`$var` for a metavariable).
    pub impls: Vec<(usize, String, String)>,
    /// Types with a container `try_from`: line, name (`$var` for a metavariable), source,
    /// whether it is an enum.
    pub coded: Vec<(usize, String, String, bool)>,
    /// See [`MacroFacts::last_ident`].
    pub last_ident: Option<String>,
}

/// Scans a macro definition's (`definition`) or invocation's tokens.
pub fn scan_macro(tokens: &TokenStream, definition: bool) -> MacroScan {
    let mut scan = MacroScan::default();
    let trees: Vec<TokenTree> = tokens.clone().into_iter().collect();
    scan.last_ident = if definition {
        trees.iter().find_map(|t| match t {
            TokenTree::Group(g) => trailing_ident_binding(g.stream()),
            _ => None,
        })
    } else {
        trees.iter().rev().find_map(|t| match t {
            TokenTree::Ident(i) => Some(paths::name(i)),
            _ => None,
        })
    };
    walk(&trees, &mut scan);
    scan
}

/// `$var` when a matcher ends with `$var:ident`.
fn trailing_ident_binding(matcher: TokenStream) -> Option<String> {
    let trees: Vec<TokenTree> = matcher.into_iter().collect();
    match trees.as_slice() {
        [
            ..,
            TokenTree::Punct(dollar),
            TokenTree::Ident(var),
            TokenTree::Punct(colon),
            TokenTree::Ident(fragment),
        ] if dollar.as_char() == '$' && colon.as_char() == ':' && fragment == "ident" => {
            Some(format!("${}", paths::name(var)))
        }
        _ => None,
    }
}

fn walk(trees: &[TokenTree], scan: &mut MacroScan) {
    for (i, tree) in trees.iter().enumerate() {
        match tree {
            TokenTree::Group(group) => {
                let inner: Vec<TokenTree> = group.stream().into_iter().collect();
                walk(&inner, scan);
            }
            TokenTree::Ident(ident) if ident == "impl" => {
                if let Some((trait_name, self_name)) = token_impl(&trees[i + 1..]) {
                    scan.impls
                        .push((ident.span().start().line, trait_name, self_name));
                }
            }
            TokenTree::Punct(pound) if pound.as_char() == '#' => {
                if let Some(TokenTree::Group(attr)) = trees.get(i + 1)
                    && attr.delimiter() == Delimiter::Bracket
                    && let Ok(meta) = syn::parse2::<Meta>(attr.stream())
                    && let Some(source) = effective_metas(&meta).iter().find_map(try_from_of)
                    && let Some((name, is_enum)) = declared_after(&trees[i + 2..])
                {
                    scan.coded
                        .push((pound.span().start().line, name, source, is_enum));
                }
            }
            _ => {}
        }
    }
}

/// The name of the struct or enum the attributes before `rest` belong to, if they belong to
/// one, and whether it is an enum: further attributes, `pub` and its scope are skipped.
fn declared_after(rest: &[TokenTree]) -> Option<(String, bool)> {
    let mut k = 0;
    loop {
        match rest.get(k)? {
            TokenTree::Punct(p) if p.as_char() == '#' => k += 2,
            TokenTree::Ident(i) if i == "pub" => k += 1,
            TokenTree::Group(g) if g.delimiter() == Delimiter::Parenthesis => k += 1,
            // `$vis` or a `$(#[$meta])*` repetition before `struct`.
            TokenTree::Punct(p) if p.as_char() == '$' => k += 2,
            TokenTree::Punct(p) if p.as_char() == '*' => k += 1,
            TokenTree::Ident(i) if i == "struct" || i == "enum" => {
                let name = match (rest.get(k + 1)?, rest.get(k + 2)) {
                    (TokenTree::Punct(p), Some(TokenTree::Ident(var))) if p.as_char() == '$' => {
                        format!("${}", paths::name(var))
                    }
                    (TokenTree::Ident(name), _) => paths::name(name),
                    _ => return None,
                };
                return Some((name, i == "enum"));
            }
            _ => return None,
        }
    }
}

/// The trait and self type of an `impl` whose tokens after the keyword are `rest`; `None` for
/// an inherent impl and for `impl Trait` in a type (no `for` before the end or a body).
fn token_impl(rest: &[TokenTree]) -> Option<(String, String)> {
    let mut k = 0;
    if is_punct(rest.first(), '<') {
        k = skip_angles(rest, 0)?;
    }
    let mut depth = 0usize;
    let mut trait_name = None;
    loop {
        match rest.get(k)? {
            TokenTree::Ident(i) if i == "for" && depth == 0 => break,
            TokenTree::Ident(i) if depth == 0 => trait_name = Some(paths::name(i)),
            TokenTree::Punct(p) if p.as_char() == '<' => depth += 1,
            TokenTree::Punct(p) if p.as_char() == '>' && !arrow(rest, k) => {
                depth = depth.saturating_sub(1);
            }
            TokenTree::Punct(p) if p.as_char() == ';' => return None,
            TokenTree::Group(g) if g.delimiter() == Delimiter::Brace => return None,
            _ => {}
        }
        k += 1;
    }
    let trait_name = trait_name?;
    k += 1;
    let mut self_name = None;
    let mut depth = 0usize;
    while let Some(tree) = rest.get(k) {
        match tree {
            TokenTree::Group(g) if g.delimiter() == Delimiter::Brace => break,
            TokenTree::Ident(i) if i == "where" && depth == 0 => break,
            TokenTree::Punct(p) if p.as_char() == '$' && depth == 0 => {
                if let Some(TokenTree::Ident(var)) = rest.get(k + 1)
                    && var != "crate"
                {
                    self_name = Some(format!("${}", paths::name(var)));
                }
                k += 1;
            }
            TokenTree::Ident(i) if depth == 0 => self_name = Some(paths::name(i)),
            TokenTree::Punct(p) if p.as_char() == '<' => depth += 1,
            TokenTree::Punct(p) if p.as_char() == '>' && !arrow(rest, k) => {
                depth = depth.saturating_sub(1);
            }
            _ => {}
        }
        k += 1;
    }
    Some((trait_name, self_name.unwrap_or_default()))
}

fn is_punct(tree: Option<&TokenTree>, c: char) -> bool {
    matches!(tree, Some(TokenTree::Punct(p)) if p.as_char() == c)
}

/// Whether the `>` at `k` ends a `->`.
fn arrow(trees: &[TokenTree], k: usize) -> bool {
    k > 0
        && matches!(&trees[k - 1], TokenTree::Punct(p) if p.as_char() == '-' && p.spacing() == Spacing::Joint)
}

/// The index after the angle-bracketed list that starts at `start`.
fn skip_angles(trees: &[TokenTree], start: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (k, tree) in trees.iter().enumerate().skip(start) {
        if let TokenTree::Punct(p) = tree {
            match p.as_char() {
                '<' => depth += 1,
                '>' if !arrow(trees, k) => {
                    depth = depth.checked_sub(1)?;
                    if depth == 0 {
                        return Some(k + 1);
                    }
                }
                _ => {}
            }
        }
    }
    None
}

fn production(site: &Site) -> bool {
    site.context == Context::Production
}

/// `ARCH-event-payload-serde`.
fn payload_serde(every: &[Facts]) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    for &(package, structs_convert) in PAYLOAD_CRATES {
        if let Some(facts) = every.iter().find(|f| f.package == package) {
            out.extend(payload_serde_in(facts, structs_convert));
        }
    }
    out
}

fn payload_serde_in(facts: &Facts, structs_convert: bool) -> Vec<Diagnostic> {
    let package = &facts.package;
    let mut out = Vec::new();
    for attr in facts.serde_attrs.iter().filter(|a| production(&a.site)) {
        let refused: Vec<String> = attr
            .keys
            .iter()
            .filter(|(key, _)| !(structs_convert && CONVERSIONS.contains(&key.as_str())))
            .filter_map(|(key, _)| {
                REFUSED
                    .iter()
                    .find(|(name, _)| name == key)
                    .map(|(name, why)| format!("`{name}` ({why})"))
            })
            .collect();
        if !refused.is_empty() {
            out.push(Diagnostic::new(
                Rule::EventPayloadSerde,
                format!(
                    "{package} writes #[serde({})] in production code: the event shape tracer would pass what it cannot see (an enum coded as a string or integer traces as a plain leaf), so event payloads use plain derives, externally tagged enums and no aliases (ADR 0018); a validated value type is a struct in {VALUES_CRATE} with a Sample",
                    refused.join(", ")
                ),
                attr.site.at(),
            ));
        }
    }
    if structs_convert {
        let parsed = facts
            .types
            .iter()
            .filter(|t| production(&t.site) && t.is_enum && t.converted)
            .map(|t| (&t.site, &t.name));
        let in_macros = facts
            .macros
            .iter()
            .filter(|m| production(&m.site))
            .flat_map(|m| &m.coded)
            .filter(|c| c.is_enum)
            .map(|c| (&c.site, &c.name));
        for (site, name) in parsed.chain(in_macros) {
            out.push(Diagnostic::new(
                Rule::EventPayloadSerde,
                format!(
                    "{package} codes the enum `{name}` through a #[serde] conversion in production code: the event shape tracer would see a plain leaf, not its variants (ADR 0018); derive an externally tagged form"
                ),
                site.at(),
            ));
        }
    }
    let parsed = facts
        .impls
        .iter()
        .filter(|i| production(&i.site))
        .map(|i| (&i.site, i.trait_name.as_str(), i.self_name.as_str()));
    let in_macros = facts
        .macros
        .iter()
        .filter(|m| production(&m.site))
        .flat_map(|m| &m.impls)
        .map(|i| (&i.site, i.trait_name.as_str(), i.self_name.as_str()));
    for (site, trait_name, self_name) in parsed.chain(in_macros) {
        if SERDE_TRAITS.contains(&trait_name) {
            out.push(Diagnostic::new(
                Rule::EventPayloadSerde,
                format!(
                    "{package} implements {trait_name} for `{self_name}` by hand in production code: the event shape tracer and the golden samples trust derived serde forms (ADR 0018); derive it"
                ),
                site.at(),
            ));
        }
    }
    out
}

/// One validated value type: where it is declared (or its macro invoked) and its name.
type Validated = (Site, String);

/// `ARCH-sample-coverage`.
fn sample_coverage(every: &[Facts]) -> Vec<Diagnostic> {
    let Some(values) = every.iter().find(|f| f.package == VALUES_CRATE) else {
        return Vec::new();
    };
    let (validated, mut out) = validated_types(values);
    if validated.is_empty() {
        return out;
    }
    let sampled = sampled_types(values);
    for (site, name) in &validated {
        if !sampled.contains(name) {
            out.push(Diagnostic::new(
                Rule::SampleCoverage,
                format!(
                    "{VALUES_CRATE} decodes `{name}` through #[serde(try_from)] but does not implement Sample for it: the event shape tracer could not offer it a value it accepts (ADR 0018); implement mayfly_domain::Sample"
                ),
                site.at(),
            ));
        }
    }
    let Some(events) = every.iter().find(|f| f.package == EVENTS_CRATE) else {
        return out;
    };
    let file = format!("{}/{}", events.dir, SHAPE.0);
    match events.sources.get(&file).and_then(|text| listed(text)) {
        Some(listed) => {
            for (site, name) in &validated {
                if !listed.contains(name) {
                    out.push(Diagnostic::new(
                        Rule::SampleCoverage,
                        format!(
                            "{VALUES_CRATE} decodes `{name}` through #[serde(try_from)], but `{}()` in {file} does not list `{name}::sample()`: the event shape tracer would report a payload carrying it as an opaque leaf (ADR 0018)",
                            SHAPE.1
                        ),
                        site.at(),
                    ));
                }
            }
        }
        None => out.push(Diagnostic::new(
            Rule::SampleCoverage,
            format!(
                "{VALUES_CRATE} has validated value types, but {file} has no top-level `fn {}()` listing their samples (ADR 0018)",
                SHAPE.1
            ),
            file,
        )),
    }
    out
}

/// The validated (non-integer `try_from`) types of the production code of `facts`, with the
/// diagnostics of macro-declared ones whose name cannot be resolved.
fn validated_types(facts: &Facts) -> (Vec<Validated>, Vec<Diagnostic>) {
    let coded = |source: &str| !INTEGERS.contains(&source);
    let mut out: Vec<Validated> = facts
        .types
        .iter()
        .filter(|t| production(&t.site) && t.try_from.as_deref().is_some_and(coded))
        .map(|t| (t.site.clone(), t.name.clone()))
        .collect();
    let mut problems = Vec::new();
    let definitions = definitions(facts);
    for (name, definition) in &definitions {
        for ty in definition.coded.iter().filter(|c| coded(&c.try_from)) {
            if !ty.name.starts_with('$') {
                out.push((ty.site.clone(), ty.name.clone()));
            } else if definition.last_ident.as_deref() != Some(ty.name.as_str()) {
                problems.push(unresolved(facts, &ty.site, name));
            }
        }
    }
    for call in facts.macros.iter().filter(|m| production(&m.site)) {
        let Some(definition) = definitions.get(call.name.as_str()) else {
            continue;
        };
        let declares = definition.coded.iter().any(|c| {
            coded(&c.try_from) && Some(c.name.as_str()) == definition.last_ident.as_deref()
        });
        if !declares {
            continue;
        }
        match &call.last_ident {
            Some(ty) => out.push((call.site.clone(), ty.clone())),
            None => problems.push(unresolved(facts, &call.site, &call.name)),
        }
    }
    (out, problems)
}

fn unresolved(facts: &Facts, site: &Site, name: &str) -> Diagnostic {
    Diagnostic::new(
        Rule::SampleCoverage,
        format!(
            "{} declares a type through `{name}!` with #[serde(try_from)], but the checker cannot tell which: name it by a trailing `$name:ident` in the macro's first arm, and pass it last",
            facts.package
        ),
        site.at(),
    )
}

/// The production `macro_rules!` definitions of `facts`, by the name they define.
fn definitions(facts: &Facts) -> BTreeMap<&str, &MacroFacts> {
    facts
        .macros
        .iter()
        .filter(|m| production(&m.site))
        .filter_map(|m| m.defines.as_deref().map(|name| (name, m)))
        .collect()
}

/// The types of the production code of `facts` that implement `Sample`, by name; an impl in a
/// macro body for its trailing `$name:ident` covers every invocation's type.
fn sampled_types(facts: &Facts) -> BTreeSet<String> {
    let mut out: BTreeSet<String> = facts
        .impls
        .iter()
        .filter(|i| production(&i.site) && i.trait_name == "Sample")
        .map(|i| i.self_name.clone())
        .collect();
    let definitions = definitions(facts);
    for (name, definition) in &definitions {
        for imp in definition.impls.iter().filter(|i| i.trait_name == "Sample") {
            if !imp.self_name.starts_with('$') {
                out.insert(imp.self_name.clone());
            } else if definition.last_ident.as_deref() == Some(imp.self_name.as_str()) {
                out.extend(
                    facts
                        .macros
                        .iter()
                        .filter(|m| production(&m.site) && m.name == *name)
                        .filter_map(|m| m.last_ident.clone()),
                );
            }
        }
    }
    out
}

/// The types whose `X::sample()` the top-level `fn texts()` of `text` calls, or `None` when
/// the file does not parse or has no such function.
fn listed(text: &str) -> Option<BTreeSet<String>> {
    struct Samples(BTreeSet<String>);
    impl<'ast> Visit<'ast> for Samples {
        fn visit_expr_path(&mut self, expr: &'ast syn::ExprPath) {
            let segments: Vec<String> = expr
                .path
                .segments
                .iter()
                .map(|s| paths::name(&s.ident))
                .collect();
            // `<A as Sample>::sample()`: the path is `Sample::sample`, the type is the qself.
            if let Some(qself) = &expr.qself {
                if segments.last().is_some_and(|last| last == "sample")
                    && let syn::Type::Path(ty) = qself.ty.as_ref()
                    && let Some(ty) = ty.path.segments.last()
                {
                    self.0.insert(paths::name(&ty.ident));
                }
            } else if let [.., ty, last] = segments.as_slice()
                && last == "sample"
            {
                self.0.insert(ty.clone());
            }
            visit::visit_expr_path(self, expr);
        }

        /// `vec![A::sample(), ..]`: the arguments of a macro that parse as expressions.
        fn visit_macro(&mut self, mac: &'ast syn::Macro) {
            if let Ok(args) = mac.parse_body_with(
                syn::punctuated::Punctuated::<syn::Expr, syn::Token![,]>::parse_terminated,
            ) {
                for arg in &args {
                    self.visit_expr(arg);
                }
            }
        }
    }
    let file = syn::parse_file(text).ok()?;
    let texts = file.items.iter().find_map(|item| match item {
        syn::Item::Fn(f) if f.sig.ident == SHAPE.1 => Some(f),
        _ => None,
    })?;
    let mut samples = Samples(BTreeSet::new());
    samples.visit_item_fn(texts);
    Some(samples.0)
}

/// `ARCH-event-set-macro`.
fn event_set_macro(every: &[Facts]) -> Vec<Diagnostic> {
    let (macro_name, file) = EVENT_SET;
    let mut out = Vec::new();
    for facts in every {
        let parsed = facts
            .impls
            .iter()
            .filter(|i| production(&i.site) && i.trait_name == "EventSet")
            .map(|i| (&i.site, &i.self_name));
        let generated = format!("{}/{file}", facts.dir);
        let in_macros = facts
            .macros
            .iter()
            .filter(|m| production(&m.site))
            .filter(|m| {
                !(facts.package == EVENTS_CRATE
                    && m.defines.as_deref() == Some(macro_name)
                    && m.site.file == generated)
            })
            .flat_map(|m| &m.impls)
            .filter(|i| i.trait_name == "EventSet")
            .map(|i| (&i.site, &i.self_name));
        for (site, self_name) in parsed.chain(in_macros) {
            out.push(Diagnostic::new(
                Rule::EventSetMacro,
                format!(
                    "{} implements EventSet for `{self_name}` outside `{macro_name}!`: a hand-written set has no generated shape() or codec, so the golden harness could not check its payloads (ADR 0018); declare it with {macro_name}!",
                    facts.package
                ),
                site.at(),
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::super::testing::{facts_for, rendered};
    use super::*;

    const SHAPE_RS: &str = "pub fn texts() -> Vec<String> {\n    vec![String::from(PoolName::sample()), String::from(<AttemptId as Sample>::sample())]\n}\n";

    /// The line of each diagnostic, sorted.
    fn lines(got: &[String]) -> Vec<usize> {
        let mut lines: Vec<usize> = got
            .iter()
            .filter_map(|d| d.trim_end_matches(')').rsplit(':').next()?.parse().ok())
            .collect();
        lines.sort_unstable();
        lines
    }

    fn events(lib: &str) -> Facts {
        facts_for(
            EVENTS_CRATE,
            &[("src/lib.rs", lib), ("src/shape.rs", SHAPE_RS)],
        )
    }

    #[test]
    fn should_report_payload_serde_when_control_domain_codes_or_aliases_a_type() {
        // given
        let lib = "mod shape;\n#[derive(Deserialize)]\n#[serde(try_from = \"String\")]\npub enum Mode { A }\n#[derive(Deserialize)]\npub struct Lit {\n    #[serde(alias = \"w\")]\n    pub watts: u32,\n    #[cfg_attr(all(), serde(flatten))]\n    pub rest: Rest,\n}\n#[derive(Deserialize)]\n#[serde(rename_all = \"snake_case\", deny_unknown_fields)]\npub struct Fine { #[serde(default, skip_serializing_if = \"Option::is_none\")] pub a: Option<u8> }\nmacro_rules! m { () => { #[serde(untagged)] pub enum E { A } }; }\nimpl<'de> serde::Deserialize<'de> for Lit {}\n#[cfg(test)]\nmod tests {\n    #[serde(try_from = \"u8\")]\n    struct T;\n}\n";

        // when
        let got = rendered(&check(&[events(lib)]));

        // then
        assert_eq!(lines(&got), [3, 7, 9, 15, 16], "{got:?}");
        assert!(
            got.iter()
                .any(|d| d.contains("`try_from` (decodes through a conversion)")),
            "{got:?}"
        );
        assert!(
            got.iter()
                .any(|d| d.contains("implements Deserialize for `Lit` by hand")),
            "{got:?}"
        );
    }

    #[test]
    fn should_pass_payload_serde_when_value_crate_structs_convert() {
        // given
        let domain = facts_for(
            VALUES_CRATE,
            &[(
                "src/lib.rs",
                "#[serde(try_from = \"u32\", into = \"u32\")]\npub struct Generation(u32);\n#[serde(try_from = \"String\")]\npub struct PoolName(String);\nimpl Sample for PoolName {}\n#[serde(transparent)]\npub struct Slot(u32);\n",
            )],
        );

        // when
        let got = rendered(&check(&[domain, events("mod shape;\n")]));

        // then
        assert_eq!(got, Vec::<String>::new());
    }

    #[test]
    fn should_report_payload_serde_when_value_crate_codes_an_enum_or_aliases() {
        // given
        let lib = "#[serde(try_from = \"u8\")]\npub enum Level { Low }\n#[serde(from = \"String\")]\npub enum Mode { Any(String) }\npub struct Lit {\n    #[serde(alias = \"w\")]\n    pub watts: u32,\n}\nmacro_rules! coded {\n    ($name:ident) => {\n        #[serde(try_from = \"u8\")]\n        pub enum $name { A }\n    };\n}\nimpl serde::Serialize for Lit {}\n";
        let domain = facts_for(VALUES_CRATE, &[("src/lib.rs", lib)]);

        // when
        let got = rendered(&check(&[domain]));

        // then
        assert_eq!(lines(&got), [2, 4, 6, 11, 15], "{got:?}");
        assert!(
            got.iter().any(|d| d.contains("codes the enum `$name`")),
            "{got:?}"
        );
    }

    #[test]
    fn should_report_sample_coverage_when_a_validated_type_lacks_sample_or_a_text() {
        // given
        let lib = "#[serde(try_from = \"String\")]\npub struct PoolName(String);\nimpl Sample for PoolName {}\n#[serde(try_from = \"String\")]\npub struct HostId(String);\nmacro_rules! hex_id {\n    ($(#[$doc:meta])* $name:ident) => {\n        $(#[$doc])*\n        #[serde(try_from = \"String\", into = \"String\")]\n        pub struct $name(u128);\n        impl Sample for $name {}\n    };\n}\nhex_id!(\n    /// An attempt.\n    AttemptId\n);\nhex_id!(EventId);\n";
        let domain = facts_for(VALUES_CRATE, &[("src/lib.rs", lib)]);

        // when
        let got = rendered(&check(&[domain, events("mod shape;\n")]));

        // then
        assert_eq!(got.len(), 3, "{got:?}");
        assert!(
            got[0].contains("`EventId`")
                && got[0].contains("does not list")
                && got[0].ends_with("lib.rs:18)"),
            "{got:?}"
        );
        assert!(
            got[1].contains("`HostId`") && got[1].contains("does not implement Sample"),
            "{got:?}"
        );
        assert!(
            got[2].contains("`HostId`") && got[2].contains("does not list"),
            "{got:?}"
        );
    }

    #[test]
    fn should_report_sample_coverage_when_a_macro_type_cannot_be_resolved_or_texts_is_missing() {
        // given
        let lib = "macro_rules! odd {\n    ($name:ident, $other:ty) => {\n        #[serde(try_from = \"String\")]\n        pub struct $name(String);\n    };\n}\nodd!(Thing, u8);\n";
        let domain = facts_for(VALUES_CRATE, &[("src/lib.rs", lib)]);
        let events = facts_for(EVENTS_CRATE, &[("src/lib.rs", "pub fn f() {}\n")]);

        // when
        let got = rendered(&check(&[domain, events]));

        // then
        assert_eq!(got.len(), 1, "{got:?}");
        assert!(got[0].contains("cannot tell which"), "{got:?}");
    }

    #[test]
    fn should_report_missing_texts_when_the_shape_file_has_no_list() {
        // given
        let domain = facts_for(
            VALUES_CRATE,
            &[(
                "src/lib.rs",
                "#[serde(try_from = \"String\")]\npub struct PoolName(String);\nimpl Sample for PoolName {}\n",
            )],
        );
        let events = facts_for(
            EVENTS_CRATE,
            &[
                ("src/lib.rs", "mod shape;\n"),
                ("src/shape.rs", "pub fn other() {}\n"),
            ],
        );

        // when
        let got = rendered(&check(&[domain, events]));

        // then
        assert_eq!(got.len(), 1, "{got:?}");
        assert!(
            got[0].contains("no top-level `fn texts()`")
                && got[0].ends_with("(at crates/mayfly-control-domain/src/shape.rs)"),
            "{got:?}"
        );
    }

    #[test]
    fn should_report_event_set_when_implemented_outside_the_macro_in_production() {
        // given
        let event_rs = "macro_rules! event_set {\n    ($name:ident) => {\n        impl $crate::event::EventSet for $name {}\n    };\n}\n";
        let lib = "mod event;\npub struct Lamp;\nimpl EventSet for Lamp {}\nmacro_rules! sneaky {\n    ($n:ident) => { impl<T: Fn() -> u8> crate::event::EventSet for $n {} fn f(x: impl EventSet) {} };\n}\n#[cfg(test)]\nmod tests {\n    impl EventSet for Fake {}\n}\n";
        let events = facts_for(
            EVENTS_CRATE,
            &[("src/lib.rs", lib), ("src/event.rs", event_rs)],
        );
        let other = facts_for(
            "mayfly-control",
            &[(
                "src/lib.rs",
                "macro_rules! event_set {\n    ($n:ident) => { impl EventSet for $n {} };\n}\n",
            )],
        );

        // when
        let got = rendered(&check(&[events, other]));

        // then
        let mut at: Vec<&str> = got
            .iter()
            .map(|d| d.rsplit("(at ").next().unwrap_or(""))
            .collect();
        at.sort_unstable();
        assert_eq!(
            at,
            [
                "crates/mayfly-control-domain/src/lib.rs:3)",
                "crates/mayfly-control-domain/src/lib.rs:5)",
                "crates/mayfly-control/src/lib.rs:2)",
            ],
            "{got:?}"
        );
        assert!(
            got.iter()
                .any(|d| d.contains("`$n`") && d.contains("mayfly-control-domain/src/lib.rs:5")),
            "{got:?}"
        );
    }
}
