//! The semantic layer (foundation.md section 3.3; ported from site-preview): rules about
//! what source code names, checked by parsing every workspace crate with `syn`.
//!
//! For each Cargo target of each workspace package, the module tree is walked from the
//! target's root file through `mod` declarations (`#[path]` and `cfg_attr(.., path)`
//! included), inline modules and `include!` of a literal path written by its own name (any
//! other route to the include family is refused by `rules_include`). Every `cfg` branch is read:
//! nothing is evaluated, so code for another platform is checked too. Code that cannot be
//! analysed fails instead of passing: a file that does not parse, a module file that does
//! not exist, and an `include!` or `#[path]` whose path is not a literal. A target root,
//! `#[path]` or `include!` that resolves outside the package's own directory is refused
//! (`ARCH-foreign-source`) and not loaded: one crate never analyses, or compiles, another's
//! source under its own policy.
//!
//! References to another workspace crate are followed through that crate's `use` items
//! (re-exports, globs, renames) and type aliases, so `pub use std::process::Command as
//! Spawner` or `pub type Spawner = std::process::Command;` in one crate still names
//! `std::process::Command` where a second crate writes `other::Spawner`. A type alias also
//! stands for its target inside its own crate, and `Self` in an impl for the impl's type.
//!
//! The collector records facts (resolved path references, `unsafe` sites, lint, `ignore` and
//! `macro_export` attributes, include-family macros, item declarations, test functions) with
//! their file, line and context (production or test code); `rules_purity`, `rules_include`,
//! `rules_runtime`, `rules_types`, `rules_events`, `rules_hygiene`, `rules_cqrs`,
//! `rules_transition`, `rules_seams`, `rules_store`, `rules_append`, `rules_projection`, `rules_order`,
//! `rules_journal_sql`, `rules_locks` and `rules_actor` turn facts into diagnostics, and
//! `rules_clippy` checks the pure crates' Clippy configuration, the
//! semantic layer behind these syntactic rules.

mod collect;
mod flow;
mod paths;
mod rules_actor;
mod rules_append;
mod rules_clippy;
mod rules_cqrs;
mod rules_events;
mod rules_hygiene;
mod rules_include;
mod rules_journal_sql;
mod rules_locks;
mod rules_order;
mod rules_projection;
mod rules_purity;
mod rules_runtime;
mod rules_seams;
mod rules_store;
mod rules_transition;
mod rules_types;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use cargo_metadata::{Metadata, Package, Target};
use syn::{Attribute, Item, Meta};

use super::policy::Policy;
use super::report::{Diagnostic, Rule};

/// How deep `mod` and `include!` nesting is followed before it is reported as a cycle.
const MAX_NESTING: usize = 64;

/// Where code is compiled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Context {
    /// Shipped code: library, binary and build-script targets outside test-only items.
    Production,
    /// Test, bench and example targets, `#[cfg(test)]` items and `#[test]` functions.
    Test,
}

/// A place in the source.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Site {
    /// File, relative to the workspace root.
    pub file: String,
    /// 1-based line.
    pub line: usize,
    /// Whether the code there is production or test code.
    pub context: Context,
}

impl Site {
    /// `file:line`, the location a diagnostic reports.
    pub fn at(&self) -> String {
        format!("{}:{}", self.file, self.line)
    }
}

/// How a path is used.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RefKind {
    /// A path in code, a type, an attribute or a `use` tree.
    Path,
    /// A macro invocation's path.
    Macro,
}

/// One path the code names, with every absolute path it may resolve to.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Reference {
    /// Where it is written.
    pub site: Site,
    /// The path as written.
    pub written: String,
    /// Candidate absolute paths; a path into the same target starts with `crate`.
    pub resolved: Vec<Vec<String>>,
    /// Written in a macro body before a metavariable (`std::$m`): it may expand to any path
    /// below each candidate.
    pub open_tail: bool,
    /// Written in a macro body after a metavariable (`$m::fs::read`): the candidate is the
    /// part after it, which may follow any crate.
    pub open_head: bool,
    /// Path or macro invocation.
    pub kind: RefKind,
    /// The module of the target it is written in (the root is empty).
    pub module: Vec<String>,
    /// Modules of the same target the resolution passed through (`crate::..`), so a
    /// re-export through `crate::queries` still names `crate::queries`.
    pub through: Vec<Vec<String>>,
    /// Written in a library target (`ARCH-anyhow-in-library`).
    pub library: bool,
}

/// A function signature that takes something by `&mut`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MutParam {
    /// Where the parameter is.
    pub site: Site,
    /// The module of the target it is written in.
    pub module: Vec<String>,
    /// The function's name.
    pub function: String,
    /// The parameter as written, for the diagnostic.
    pub written: String,
    /// It is the receiver (`&mut self`).
    pub receiver: bool,
    /// The referenced type is a generic parameter or `Self`: unknown, so possibly state.
    pub generic: bool,
    /// Candidates of every path inside the `&mut` type.
    pub resolved: Vec<Vec<String>>,
}

/// A method call, by name: method calls are not type-resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MethodCall {
    /// Where the method name is.
    pub site: Site,
    /// The module of the target it is written in.
    pub module: Vec<String>,
    /// The method's name.
    pub name: String,
}

/// An instrumentation value in a decision position: a branch condition, a `match`
/// scrutinee, a `let .. else` initializer or a returned value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    /// Where the instrumentation is named.
    pub site: Site,
    /// The module of the target it is written in.
    pub module: Vec<String>,
    /// Which decision position (`an if condition`, `a returned value`, ..).
    pub position: &'static str,
    /// The instrumentation as written, or the local it was bound to.
    pub instrumentation: String,
}

/// What the collector found in one workspace package, across all its targets.
#[derive(Debug, Default)]
pub struct Facts {
    /// Package name.
    pub package: String,
    /// The package directory, relative to the workspace root.
    pub dir: String,
    /// Every path reference.
    pub references: Vec<Reference>,
    /// Every `unsafe` block, function, impl, trait, extern block or attribute.
    pub unsafe_sites: Vec<(Site, &'static str)>,
    /// Code that cannot be analysed, with the reason.
    pub problems: Vec<(String, String)>,
    /// `allow`, `expect`, `coverage` and `ignore` attributes.
    pub attributes: Vec<AttributeUse>,
    /// Test functions.
    pub tests: Vec<TestFn>,
    /// Text of every analysed file, by workspace-relative path.
    pub sources: BTreeMap<String, String>,
    /// Module paths of every production unit of every target.
    pub modules: BTreeSet<Vec<String>>,
    /// Signatures that take something by `&mut`.
    pub mut_params: Vec<MutParam>,
    /// Method calls, by name.
    pub method_calls: Vec<MethodCall>,
    /// Instrumentation values in decision positions.
    pub decisions: Vec<Decision>,
    /// Items marked `#[doc(hidden)]`.
    pub hidden: Vec<HiddenItem>,
    /// `cfg!` invocations that name a platform, with their predicate.
    pub platform_cfgs: Vec<(Site, String)>,
    /// Target roots, `#[path]` modules and `include!`s that resolve outside the package
    /// directory: where, and what.
    pub foreign: Vec<(String, String)>,
    /// `static` items, with whether they are `static mut`.
    pub statics: Vec<(Site, bool)>,
    /// Structs and enums.
    pub types: Vec<TypeDecl>,
    /// Type aliases (`type A = ..;`), with the paths their target names (`ARCH-secret-serialize`).
    pub aliases: Vec<TypeAlias>,
    /// Trait impls.
    pub impls: Vec<ImplDecl>,
    /// The types `ControlAggregates::accept` visits (`visitor.visit::<X>()`), by last segment.
    pub registry_visits: Vec<String>,
    /// Include-family macros the loader does not follow: data includes and every one written
    /// inside a macro token stream (`ARCH-include-macro`, `ARCH-foreign-source`).
    pub includes: Vec<IncludeUse>,
    /// Named item declarations (`ARCH-plane-vocabulary`).
    pub declarations: Vec<Declaration>,
    /// `#[macro_export]` attributes, where they are (`ARCH-macro-export`).
    pub macro_exports: Vec<Site>,
    /// Nightly-only constructs: `#![feature(..)]` (also through `cfg_attr` and in macro
    /// tokens) and macro 2.0 items, with what was found (`ARCH-nightly-features`).
    pub nightly: Vec<(Site, String)>,
    /// `env!` and `option_env!` invocations (also in macro tokens): where, which, and the
    /// variable when the first argument is a string literal (`ARCH-domain-purity` in tests).
    pub env_macros: Vec<(Site, String, Option<String>)>,
    /// `#[serde(..)]` attributes, parsed and in macro tokens, through `cfg_attr` too
    /// (`ARCH-event-payload-serde`).
    pub serde_attrs: Vec<SerdeAttr>,
    /// What each macro definition or invocation holds that the event rules read
    /// (`ARCH-sample-coverage`, `ARCH-event-set-macro`).
    pub macros: Vec<MacroFacts>,
}

/// One `#[serde(..)]` attribute: the key of each of its arguments, with the string a
/// `key = "value"` argument gives. An argument list that does not parse is one
/// [`rules_events::UNREADABLE`] key, so it is refused where serde keys are restricted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SerdeAttr {
    /// Where its `#` is.
    pub site: Site,
    /// `try_from`, `alias`, `rename_all`, ..; with the literal value when there is one.
    pub keys: Vec<(String, Option<String>)>,
}

/// A trait impl found in a macro's tokens (`impl $crate::event::EventSet for $name`), which
/// [`ImplDecl`] cannot see.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenImpl {
    /// Where `impl` is.
    pub site: Site,
    /// The trait's last segment.
    pub trait_name: String,
    /// The self type's name, or `$var` when it is a metavariable.
    pub self_name: String,
}

/// A struct or enum in a macro's tokens whose container `#[serde(try_from = "..")]` decodes it
/// through a conversion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodedType {
    /// Where its `#` is.
    pub site: Site,
    /// The type's name, or `$var` when it is a metavariable.
    pub name: String,
    /// The `try_from` source type, as written.
    pub try_from: String,
    /// Whether it is an enum.
    pub is_enum: bool,
}

/// One macro definition (`macro_rules! name`) or invocation, by what the event rules read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MacroFacts {
    /// Where the macro's path is.
    pub site: Site,
    /// The invoked macro's last segment (`macro_rules` for a definition).
    pub name: String,
    /// For a `macro_rules!` item, the name it defines.
    pub defines: Option<String>,
    /// For a definition, the metavariable of a trailing `$x:ident` in its first arm's matcher;
    /// for an invocation, its last top-level identifier: the type a type-declaring macro names.
    pub last_ident: Option<String>,
    /// Trait impls in its tokens.
    pub impls: Vec<TokenImpl>,
    /// Types in its tokens decoded through a container `try_from`.
    pub coded: Vec<CodedType>,
}

/// The `name` of an [`AttributeUse`] whose own name, or a `cfg_attr` branch's, is a macro
/// metavariable (`#[$level(clippy::x)]`): the level the expansion sets cannot be read, so it
/// is refused whatever `[[exceptions.lints]]` holds.
pub const METAVARIABLE_ATTRIBUTE: &str = "$metavariable";

/// The include family: `include!` compiles a file, the other two embed one as data.
pub const INCLUDE_FAMILY: &[&str] = &["include", "include_str", "include_bytes"];

/// How an include-family macro was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IncludeKind {
    /// Written as an expression (`include_str!("x")`); an `include!` there is followed.
    Direct,
    /// Written inside a macro's token stream (a `macro_rules!` body or another invocation's
    /// arguments), where the checker cannot follow the file.
    InTokens,
}

/// One include-family macro the loader did not follow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncludeUse {
    /// Where its name is.
    pub site: Site,
    /// `include`, `include_str` or `include_bytes`.
    pub name: String,
    /// The macro path as written.
    pub written: String,
    /// Where it was found.
    pub kind: IncludeKind,
    /// The `ARCH-foreign-source` message when its literal path lies outside the package.
    pub outside: Option<String>,
    /// Whether its argument is one string literal (not `concat!`, `env!` or a metavariable).
    pub literal: bool,
}

/// A named item: a struct, enum, union, trait, type, fn, mod, const, static or macro, also an
/// associated fn, type or const.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Declaration {
    /// Where its name is.
    pub site: Site,
    /// `struct`, `trait`, `fn`, ..
    pub kind: &'static str,
    /// Its name.
    pub name: String,
}

/// A struct or enum declaration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeDecl {
    /// Where its name is.
    pub site: Site,
    /// Its name.
    pub name: String,
    /// The last segment of every derived trait, through `cfg_attr` too.
    pub derives: Vec<String>,
    /// Its item path: the target's key (a library's crate name, as other crates write it),
    /// its module and its name.
    pub path: Vec<String>,
    /// Every identifier in its field types.
    pub fields: BTreeSet<String>,
    /// Every path in its field types, generic arguments, bounds and defaults included, with
    /// its candidates (its own type parameters left out).
    pub refs: Vec<TypeRef>,
    /// The source type of a container `#[serde(try_from = "..")]`, as written.
    pub try_from: Option<String>,
    /// Whether it is an enum.
    pub is_enum: bool,
    /// Whether a container `#[serde(..)]` decodes or encodes it through a conversion
    /// (`try_from`, `from` or `into`).
    pub converted: bool,
}

/// A trait impl (`impl Trait for Type`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImplDecl {
    /// Where the trait is named.
    pub site: Site,
    /// The trait's last segment.
    pub trait_name: String,
    /// The last segment of the self type's path (empty when it is not a path).
    pub self_name: String,
    /// Every path in the self type, generic arguments included, with its candidates (the
    /// impl's own type parameters left out).
    pub self_refs: Vec<TypeRef>,
}

/// A path named inside a type, with the item paths it may stand for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeRef {
    /// Its last segment as written.
    pub last: String,
    /// Candidate item paths: one into the same target starts with the target's key (as in
    /// [`TypeDecl::path`]), any other with the crate it names; re-exports of workspace
    /// libraries are followed ([`follow_candidates`]).
    pub resolved: Vec<Vec<String>>,
}

/// A type alias (`type A = ..;`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeAlias {
    /// Where its name is.
    pub site: Site,
    /// Its item path, as [`TypeDecl::path`].
    pub path: Vec<String>,
    /// Every path in its target type and generics, with its candidates.
    pub refs: Vec<TypeRef>,
}

/// An item marked `#[doc(hidden)]`: a function, a method, a type or a constant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HiddenItem {
    /// Where its name is.
    pub site: Site,
    /// Its name.
    pub name: String,
}

/// One attribute the hygiene rules look at, after `cfg_attr` expansion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttributeUse {
    /// Where its `#` is.
    pub site: Site,
    /// `allow`, `expect`, `coverage`, `ignore` or [`METAVARIABLE_ATTRIBUTE`].
    pub name: String,
    /// The lints it names (`coverage` for `coverage(off)`; empty for `ignore`).
    pub lints: Vec<String>,
}

/// One function marked as a test.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestFn {
    /// Where its name is (for a test in a macro body, where the attribute is).
    pub site: Site,
    /// Its name.
    pub name: String,
    /// First and last line of its body; `None` for a test in a macro body.
    pub body: Option<(usize, usize)>,
}

impl Facts {
    /// The path of `site`'s file relative to the package directory.
    pub fn in_package<'a>(&self, site: &'a Site) -> &'a str {
        site.file
            .strip_prefix(&self.dir)
            .and_then(|rest| rest.strip_prefix('/'))
            .unwrap_or(&site.file)
    }
}

/// Checks every workspace package of `metadata` against the semantic rules. `adr_exists`
/// says whether an ADR number names a file under `doc/adr/`.
pub fn check(
    metadata: &Metadata,
    policy: &Policy,
    adr_exists: &dyn Fn(&str) -> bool,
) -> Vec<Diagnostic> {
    let root = metadata.workspace_root.as_std_path();
    let mut out = selector_problems(root, metadata, policy);
    let mut hygiene = rules_hygiene::Hygiene::new(policy, adr_exists);
    let manifests: Vec<String> = metadata
        .workspace_packages()
        .iter()
        .map(|p| relative(root, p.manifest_path.as_std_path()))
        .collect();
    out.extend(hygiene.config(root, &manifests));
    out.extend(rules_clippy::check(root, metadata, policy, &manifests));
    let mut every = Vec::new();
    let mut libraries = BTreeMap::new();
    for package in metadata.workspace_packages() {
        let (facts, library) = collect_package(root, package);
        libraries.extend(library);
        every.push(facts);
    }
    for facts in &mut every {
        follow_reexports(&libraries, &mut facts.references);
        follow_type_refs(&libraries, facts);
    }
    for facts in &every {
        for (at, problem) in &facts.problems {
            out.push(Diagnostic::new(
                Rule::SourceParse,
                problem.clone(),
                at.clone(),
            ));
        }
        for (at, problem) in &facts.foreign {
            out.push(Diagnostic::new(
                Rule::ForeignSource,
                problem.clone(),
                at.clone(),
            ));
        }
        out.extend(rules_purity::check(facts, policy));
        out.extend(rules_include::check(facts, policy));
        out.extend(rules_runtime::check(facts, policy));
        out.extend(rules_cqrs::check(facts, policy));
        out.extend(rules_store::check(root, facts));
        out.extend(rules_append::check(facts, policy));
        out.extend(rules_actor::check(facts, policy));
        out.extend(rules_projection::check(facts, policy));
        out.extend(rules_order::check(root, facts, policy));
        out.extend(rules_journal_sql::check(root, facts, policy));
        out.extend(hygiene.check(facts));
    }
    out.extend(rules_types::check(&every));
    out.extend(rules_transition::check(&every, policy));
    out.extend(rules_events::check(&every));
    out.extend(rules_seams::check(&every));
    out.extend(rules_locks::check(root, &every));
    out.extend(hygiene.finish());
    // Line numbers are computed; the parsed sources need not stay in this thread's span map.
    proc_macro2::extra::invalidate_current_thread_spans();
    out
}

/// Every workspace crate a rule names in code, as a package name: the crates the rules apply
/// to, exempt or forbid. A renamed crate would make those rules pass without checking
/// anything. The crates `[planes]` and `[purity]` name are checked by `ARCH-missing-crate`.
fn selector_crates() -> BTreeSet<String> {
    let mut out: BTreeSet<String> = rules_purity::APPLICATIONS
        .iter()
        .chain([
            &rules_hygiene::POLL_HELPER.0,
            &rules_cqrs::APPLICATION,
            &rules_seams::DEFINER,
            &rules_actor::DEFINER,
            &rules_actor::SOCKET.0,
            &rules_store::ADAPTER,
            &rules_types::AGGREGATES.0,
            &rules_events::EVENTS_CRATE,
            &rules_events::VALUES_CRATE,
        ])
        .chain(rules_seams::USERS)
        .chain(rules_journal_sql::STORE_ADAPTERS)
        .chain(rules_runtime::SPAWN_MODULES.iter().map(|(name, _)| name))
        .map(|name| (*name).to_owned())
        .collect();
    for (package, crates) in rules_purity::ISOLATION {
        out.insert((*package).to_owned());
        out.extend(
            crates
                .iter()
                .filter(|c| c.starts_with("mayfly_"))
                .map(|c| c.replace('_', "-")),
        );
    }
    out
}

/// `ARCH-missing-selector`: each crate the rules name must be a workspace member unless
/// `[semantic] absent` lists it (fixtures with part of the topology), an `absent` entry
/// must name a crate a rule names and must not name a member, and, once
/// [`rules_hygiene::POLL_HELPER_REQUIRED`] is set, the poll helper's file must exist.
fn selector_problems(root: &Path, metadata: &Metadata, policy: &Policy) -> Vec<Diagnostic> {
    let members: BTreeMap<String, &Package> = metadata
        .workspace_packages()
        .into_iter()
        .map(|p| (p.name.to_string(), p))
        .collect();
    let absent: BTreeSet<&str> = policy.semantic.absent.iter().map(String::as_str).collect();
    let selected = selector_crates();
    let mut out = Vec::new();
    for name in &selected {
        match (members.contains_key(name), absent.contains(name.as_str())) {
            (false, false) => out.push(Diagnostic::new(
                Rule::MissingSelector,
                format!(
                    "the semantic rules name crate `{name}`, which is not in the workspace: a rename would make them pass unchecked; rename the rule's selector too, or list it in [semantic] absent"
                ),
                super::POLICY_FILE,
            )),
            (true, true) => out.push(Diagnostic::new(
                Rule::MissingSelector,
                format!("[semantic] absent lists `{name}`, which is a workspace crate"),
                super::POLICY_FILE,
            )),
            _ => {}
        }
    }
    for name in &absent {
        if !selected.contains(*name) {
            out.push(Diagnostic::new(
                Rule::MissingSelector,
                format!("[semantic] absent lists `{name}`, which no rule names"),
                super::POLICY_FILE,
            ));
        }
    }
    let (helper, file) = rules_hygiene::POLL_HELPER;
    if rules_hygiene::POLL_HELPER_REQUIRED
        && let Some(package) = members.get(helper)
        && let Some(dir) = package.manifest_path.parent()
        && !dir.as_std_path().join(file).is_file()
    {
        out.push(Diagnostic::new(
            Rule::MissingSelector,
            format!("the test-sleep exemption names `{file}` in {helper}, which does not exist"),
            relative(root, &dir.as_std_path().join(file)),
        ));
    }
    out
}

/// The first candidate of `reference` under a forbidden prefix, rendered. A macro path that
/// continues with a metavariable (`std::$m`) hits when a forbidden prefix lies below it; one
/// that starts with a metavariable (`$k::fs::read`) hits when what follows it matches the
/// rest of a forbidden prefix after its first segment.
pub fn first_hit(reference: &Reference, forbidden: &[&[&str]]) -> Option<String> {
    let agree = |a: &[String], b: &[&str]| a.iter().zip(b).all(|(x, y)| x == y);
    reference.resolved.iter().find_map(|candidate| {
        forbidden.iter().find_map(|prefix| {
            if reference.open_head {
                return (1..prefix.len())
                    .any(|k| !candidate.is_empty() && agree(candidate, &prefix[k..]))
                    .then(|| prefix.join("::"));
            }
            let related = agree(candidate, prefix);
            if related && candidate.len() >= prefix.len() {
                Some(candidate.join("::"))
            } else if related && reference.open_tail {
                Some(prefix.join("::"))
            } else {
                None
            }
        })
    })
}

/// The scopes of each workspace library target, by the name code uses for it
/// (`mayfly_composition`), for [`follow_reexports`].
type Libraries = BTreeMap<String, paths::Scopes>;

/// How many workspace crates a re-export chain is followed through.
const MAX_REEXPORT_HOPS: usize = 8;

/// Adds to every reference the candidates its workspace-crate candidates re-export: a
/// candidate `other::a::B` is looked up as `crate::a::B` in `other`'s library scopes; each
/// path that leaves that crate (`std::process::Command`) becomes a candidate and is followed
/// further (chains of crates), and each path inside it is added as `other::..` (so a
/// renamed re-export still shows the name it stands for). Candidates are only added, so a
/// rule can only find more. Pinned by `should_follow_reexports_when_another_crate_names_them`.
fn follow_reexports(libraries: &Libraries, references: &mut [Reference]) {
    for reference in references {
        follow_candidates(libraries, &mut reference.resolved);
    }
}

/// [`follow_reexports`] for the paths named inside types, aliases and impl self types, so
/// a holder another crate re-exports or aliases is found under its declared path
/// (`ARCH-secret-serialize`).
fn follow_type_refs(libraries: &Libraries, facts: &mut Facts) {
    let refs = facts
        .types
        .iter_mut()
        .flat_map(|t| &mut t.refs)
        .chain(facts.aliases.iter_mut().flat_map(|a| &mut a.refs))
        .chain(facts.impls.iter_mut().flat_map(|i| &mut i.self_refs));
    for type_ref in refs {
        follow_candidates(libraries, &mut type_ref.resolved);
    }
}

/// Adds to `resolved` every candidate its workspace-crate candidates re-export, as
/// [`follow_reexports`] describes.
fn follow_candidates(libraries: &Libraries, resolved: &mut Vec<Vec<String>>) {
    let mut frontier = resolved.clone();
    for _ in 0..MAX_REEXPORT_HOPS {
        let mut next = Vec::new();
        for candidate in &frontier {
            let Some((first, rest)) = candidate.split_first() else {
                continue;
            };
            let Some(scopes) = libraries.get(first) else {
                continue;
            };
            let mut segments = vec!["crate".to_owned()];
            segments.extend_from_slice(rest);
            let path = paths::Written {
                absolute: false,
                segments,
            };
            for mut found in scopes.resolve(&[], &path) {
                let local = found.first().is_some_and(|f| f == "crate");
                if local {
                    first.clone_into(&mut found[0]);
                }
                if !resolved.contains(&found) {
                    resolved.push(found.clone());
                    if !local {
                        next.push(found);
                    }
                }
            }
        }
        if next.is_empty() {
            break;
        }
        frontier = next;
    }
}

fn collect_package(root: &Path, package: &Package) -> (Facts, Option<(String, paths::Scopes)>) {
    let package_dir = package
        .manifest_path
        .parent()
        .map(|p| p.as_std_path().to_path_buf())
        .unwrap_or_default();
    let dir = relative(root, &package_dir);
    let mut facts = Facts {
        package: package.name.to_string(),
        dir,
        ..Facts::default()
    };
    let mut library = None;
    let renames: BTreeMap<String, String> = package
        .dependencies
        .iter()
        .filter_map(|d| {
            d.rename
                .as_ref()
                .map(|r| (r.replace('-', "_"), d.name.replace('-', "_")))
        })
        .collect();
    for target in &package.targets {
        let is_library = target.kind.iter().any(|k| {
            matches!(
                k.to_string().as_str(),
                "lib" | "rlib" | "dylib" | "proc-macro"
            )
        });
        let units = match load_target(root, &package_dir, target) {
            Ok(loaded) => {
                facts.sources.extend(loaded.texts);
                facts.foreign.extend(loaded.foreign);
                loaded.units
            }
            Err(problems) => {
                facts.problems.extend(problems);
                continue;
            }
        };
        let crate_name = target.name.replace('-', "_");
        // A library's items are keyed by the name other crates write; any other target's by
        // a key no path can spell, so a binary's or test's `crate::World` is never another
        // target's `World` (pinned by
        // `should_report_only_the_library_holder_when_a_binary_declares_a_same_named_type`).
        let key = if is_library {
            crate_name.clone()
        } else {
            format!("{} (target {})", package.name, target.name)
        };
        let place = collect::Target {
            root,
            package_dir: &package_dir,
            library: is_library,
            key: &key,
        };
        let scopes = collect::collect_target(&place, &units, renames.clone(), &mut facts);
        if is_library {
            library = Some((crate_name, scopes));
        }
    }
    (facts, library)
}

/// One module's items from one file. A module whose items come from several files (through
/// `include!`) has one unit per file.
pub struct Unit {
    /// Module path inside the target; the root is empty.
    pub module: Vec<String>,
    /// Absolute path of the file the items come from.
    pub file: PathBuf,
    /// Whether the whole module is test-only.
    pub test: bool,
    /// The file's inner attributes (`#![..]`), on the unit that starts the file.
    pub attrs: Vec<Attribute>,
    /// The items, with inline and out-of-line child modules moved to their own units.
    pub items: Vec<Item>,
}

/// Parses a target's module tree. Problems are reported per file; one problem anywhere
/// fails the target.
fn load_target(
    root: &Path,
    package_dir: &Path,
    target: &Target,
) -> Result<Loaded, Vec<(String, String)>> {
    let test = target
        .kind
        .iter()
        .any(|k| matches!(k.to_string().as_str(), "test" | "bench" | "example"));
    let file = target.src_path.as_std_path().to_path_buf();
    let mut loader = Loader::new(root, package_dir);
    let mod_dir = file.parent().map(Path::to_path_buf).unwrap_or_default();
    loader.load_file(&file, Vec::new(), &mod_dir, test, 0, None);
    if loader.problems.is_empty() {
        Ok(Loaded {
            units: loader.units,
            texts: loader.texts,
            foreign: loader.foreign,
        })
    } else {
        Err(loader.problems)
    }
}

/// A target's units, the text of each file by workspace-relative path, and the files it
/// refused to load because they lie outside the package.
struct Loaded {
    units: Vec<Unit>,
    texts: BTreeMap<String, String>,
    foreign: Vec<(String, String)>,
}

/// Whether `file` lies inside `package_dir` once both are canonical (symlinks and `..`
/// resolved). A file that does not exist is left to the missing-file diagnostics.
pub fn outside_package(file: &Path, package_dir: &Path) -> bool {
    match (fs::canonicalize(file), fs::canonicalize(package_dir)) {
        (Ok(file), Ok(dir)) => !file.starts_with(dir),
        _ => false,
    }
}

/// The `ARCH-foreign-source` message for `file`, reached through `what`: the file is shown
/// canonical and relative to `root` (`crates/other/src/lib.rs`, not `crates/a/src/../..`).
pub fn foreign_message(what: &str, root: &Path, file: &Path) -> String {
    let rel = match (fs::canonicalize(root), fs::canonicalize(file)) {
        (Ok(root), Ok(file)) => relative(&root, &file),
        _ => relative(root, file),
    };
    format!(
        "{what} resolves to `{rel}`, outside the package directory: a crate's source never comes from another package (it would be checked, and compiled, under the wrong policy)"
    )
}

struct Loader<'a> {
    root: &'a Path,
    package_dir: PathBuf,
    units: Vec<Unit>,
    problems: Vec<(String, String)>,
    foreign: Vec<(String, String)>,
    seen: BTreeSet<(PathBuf, Vec<String>)>,
    texts: BTreeMap<String, String>,
}

impl<'a> Loader<'a> {
    fn new(root: &'a Path, package_dir: &Path) -> Self {
        Self {
            root,
            package_dir: package_dir.to_path_buf(),
            units: Vec::new(),
            problems: Vec::new(),
            foreign: Vec::new(),
            seen: BTreeSet::new(),
            texts: BTreeMap::new(),
        }
    }

    fn problem(&mut self, at: String, message: String) {
        self.problems.push((at, message));
    }

    /// Parses `file` as the module `module` whose child modules live in `mod_dir`.
    fn load_file(
        &mut self,
        file: &Path,
        module: Vec<String>,
        mod_dir: &Path,
        test: bool,
        depth: usize,
        from: Option<String>,
    ) {
        let rel = relative(self.root, file);
        if outside_package(file, &self.package_dir) {
            let (at, what) = match from {
                Some(at) => (at, "a `#[path]` module or `include!`"),
                None => (
                    relative(self.root, &self.package_dir.join("Cargo.toml")),
                    "a target root",
                ),
            };
            self.foreign
                .push((at, foreign_message(what, self.root, file)));
            return;
        }
        let at = from.unwrap_or_else(|| rel.clone());
        if depth > MAX_NESTING || !self.seen.insert((file.to_path_buf(), module.clone())) {
            self.problem(at, format!("`{rel}` is loaded recursively"));
            return;
        }
        let text = match fs::read_to_string(file) {
            Ok(text) => text,
            Err(err) => {
                self.problem(at, format!("cannot read `{rel}`: {err}"));
                return;
            }
        };
        self.texts.insert(rel.clone(), text.clone());
        match syn::parse_file(&text) {
            Ok(parsed) => {
                let base = file.parent().map(Path::to_path_buf).unwrap_or_default();
                let mut ctx = Place {
                    file: file.to_path_buf(),
                    rel,
                    module,
                    mod_dir: mod_dir.to_path_buf(),
                    path_base: base,
                    test: test || is_test_only(&parsed.attrs),
                    depth,
                };
                self.load_items(parsed.attrs, parsed.items, &mut ctx);
            }
            Err(err) => {
                let line = err.span().start().line;
                self.problem(
                    format!("{rel}:{line}"),
                    format!("`{rel}` does not parse: {err}; unparsed code is never a pass"),
                );
            }
        }
    }

    /// Moves `items` into units: child modules get their own, the rest stays in `ctx`.
    fn load_items(&mut self, attrs: Vec<Attribute>, items: Vec<Item>, ctx: &mut Place) {
        let mut kept = Vec::new();
        for item in items {
            match item {
                Item::Mod(mut m) => {
                    let name = paths::name(&m.ident);
                    let mut child = ctx.module.clone();
                    child.push(name.clone());
                    let test = ctx.test || is_test_only(&m.attrs);
                    let line = m.ident.span().start().line;
                    let paths = path_attributes(&m.attrs);
                    if let Some((_, inner)) = m.content.as_mut() {
                        let inner = std::mem::take(inner);
                        let mod_dir = ctx.mod_dir.join(&name);
                        let mut inner_ctx = Place {
                            module: child,
                            mod_dir: mod_dir.clone(),
                            path_base: match paths.iter().find(|p| !p.conditional) {
                                Some(PathAttr { value: Ok(p), .. }) => ctx.path_base.join(p),
                                _ => mod_dir,
                            },
                            test,
                            ..ctx.clone()
                        };
                        self.load_items(Vec::new(), inner, &mut inner_ctx);
                    } else {
                        self.load_module_file(ctx, &name, child, test, line, &paths);
                    }
                    kept.push(Item::Mod(m));
                }
                Item::Macro(mac) if mac.ident.is_none() && is_include(&mac.mac.path) => {
                    let line = mac
                        .mac
                        .path
                        .segments
                        .last()
                        .map_or(0, |s| s.ident.span().start().line);
                    let at = format!("{}:{line}", ctx.rel);
                    match include_path(&mac.mac.tokens) {
                        Some(path) => {
                            let file = ctx.file.parent().unwrap_or(Path::new("")).join(path);
                            let mod_dir = ctx.mod_dir.clone();
                            self.load_file(
                                &file,
                                ctx.module.clone(),
                                &mod_dir,
                                ctx.test,
                                ctx.depth + 1,
                                Some(at),
                            );
                        }
                        None => self.problem(at, non_literal("include!")),
                    }
                }
                other => kept.push(other),
            }
        }
        self.units.push(Unit {
            module: ctx.module.clone(),
            file: ctx.file.clone(),
            test: ctx.test,
            attrs,
            items: kept,
        });
    }

    fn load_module_file(
        &mut self,
        ctx: &Place,
        name: &str,
        module: Vec<String>,
        test: bool,
        line: usize,
        paths: &[PathAttr],
    ) {
        let at = format!("{}:{line}", ctx.rel);
        if paths.iter().any(|p| p.value.is_err()) {
            self.problem(at, non_literal("#[path]"));
            return;
        }
        // Every `#[path]`, conditional or not, names a file that must exist. Without an
        // unconditional one, the default file is what the other `cfg` branches load.
        let mut existing: Vec<(PathBuf, bool)> = Vec::new();
        for path in paths {
            if let Ok(p) = &path.value {
                let file = ctx.path_base.join(p);
                if !file.is_file() {
                    self.problem(
                        at.clone(),
                        format!(
                            "module `{name}` names `{}` in a #[path], which does not exist",
                            relative(self.root, &file)
                        ),
                    );
                    return;
                }
                existing.push((file, true));
            }
        }
        let defaults = [
            ctx.mod_dir.join(format!("{name}.rs")),
            ctx.mod_dir.join(name).join("mod.rs"),
        ];
        if paths.iter().all(|p| p.conditional) {
            existing.extend(
                defaults
                    .iter()
                    .find(|p| p.is_file())
                    .map(|p| (p.clone(), p.ends_with("mod.rs"))),
            );
        }
        if existing.is_empty() {
            let tried = defaults
                .iter()
                .map(|p| format!("`{}`", relative(self.root, p)))
                .collect::<Vec<_>>()
                .join(" or ");
            self.problem(
                at,
                format!("module `{name}` has no source file (tried {tried})"),
            );
            return;
        }
        for (file, mod_rs) in existing {
            let mod_dir = if mod_rs {
                file.parent().map(Path::to_path_buf).unwrap_or_default()
            } else {
                file.with_extension("")
            };
            self.load_file(
                &file,
                module.clone(),
                &mod_dir,
                test,
                ctx.depth + 1,
                Some(at.clone()),
            );
        }
    }
}

/// Where the loader is.
#[derive(Clone)]
struct Place {
    file: PathBuf,
    rel: String,
    module: Vec<String>,
    /// Directory of child module files.
    mod_dir: PathBuf,
    /// Directory a `#[path]` is relative to.
    path_base: PathBuf,
    test: bool,
    depth: usize,
}

fn non_literal(what: &str) -> String {
    format!(
        "{what} with a path that is not a string literal cannot be analysed (build-script \
         output and macro expansions are invisible to the checker)"
    )
}

/// Whether a macro path is `include!` (also `std::include!`, `core::include!`).
pub fn is_include(path: &syn::Path) -> bool {
    path.segments.last().is_some_and(|s| s.ident == "include")
}

/// The literal path of an `include!` invocation.
pub fn include_path(tokens: &proc_macro2::TokenStream) -> Option<String> {
    syn::parse2::<syn::LitStr>(tokens.clone())
        .ok()
        .map(|lit| lit.value())
}

/// One `#[path]` of a module declaration.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PathAttr {
    /// The literal path; `Err` when it is not a string literal.
    value: Result<String, ()>,
    /// Given through `cfg_attr`, so it applies only in some `cfg` branches.
    conditional: bool,
}

/// `#[path = ".."]` and every `#[cfg_attr(.., path = "..")]` branch.
fn path_attributes(attrs: &[Attribute]) -> Vec<PathAttr> {
    let mut out = Vec::new();
    for attr in attrs {
        for (i, meta) in effective_metas(&attr.meta).iter().enumerate() {
            if let Meta::NameValue(nv) = meta
                && nv.path.is_ident("path")
            {
                out.push(PathAttr {
                    value: match &nv.value {
                        syn::Expr::Lit(syn::ExprLit {
                            lit: syn::Lit::Str(s),
                            ..
                        }) => Ok(s.value()),
                        _ => Err(()),
                    },
                    conditional: i > 0,
                });
            }
        }
    }
    out
}

/// The attribute itself, plus every attribute a `cfg_attr` may expand to (any branch).
pub fn effective_metas(meta: &Meta) -> Vec<Meta> {
    let mut out = vec![meta.clone()];
    let Meta::List(list) = meta else {
        return out;
    };
    if !list.path.is_ident("cfg_attr") {
        return out;
    }
    // Each branch on its own: a condition that does not parse as a meta (a macro's
    // `$cond`) must not hide the attributes after it.
    let trees: Vec<proc_macro2::TokenTree> = list.tokens.clone().into_iter().collect();
    for branch in trees
        .split(|t| matches!(t, proc_macro2::TokenTree::Punct(p) if p.as_char() == ','))
        .skip(1)
        .filter(|b| !b.is_empty())
    {
        let stream: proc_macro2::TokenStream = branch.iter().cloned().collect();
        if let Ok(inner) = syn::parse2::<Meta>(stream) {
            out.extend(effective_metas(&inner));
        }
    }
    out
}

/// Whether `attrs` hold `#[doc(hidden)]`, also through `cfg_attr`.
pub fn is_doc_hidden(attrs: &[Attribute]) -> bool {
    attrs.iter().any(|attr| {
        effective_metas(&attr.meta).iter().any(|meta| {
            matches!(meta, Meta::List(list) if list.path.is_ident("doc")
                && list.tokens.to_string().trim() == "hidden")
        })
    })
}

/// Whether `attrs` make an item test-only: `#[cfg(test)]`, `#[cfg(all(test, ..))]`, or a
/// test attribute (`#[test]`, `#[tokio::test]`).
pub fn is_test_only(attrs: &[Attribute]) -> bool {
    attrs.iter().any(|attr| {
        is_test_attribute(attr)
            || matches!(&attr.meta, Meta::List(list) if list.path.is_ident("cfg")
                && list.parse_args::<Meta>().is_ok_and(|m| cfg_requires_test(&m)))
    })
}

/// The attributes that mark a test function. Any other attribute does not, so a test
/// framework the checker does not know is not treated as test code.
pub const TEST_ATTRIBUTES: &[&[&str]] = &[&["test"], &["tokio", "test"]];

/// Whether `path` is one of [`TEST_ATTRIBUTES`].
pub fn is_test_path(path: &syn::Path) -> bool {
    let segments: Vec<String> = path
        .segments
        .iter()
        .map(|s| paths::name(&s.ident))
        .collect();
    path.leading_colon.is_none() && TEST_ATTRIBUTES.iter().any(|t| *t == segments.as_slice())
}

/// Whether `attr` marks a test function (`#[test]`, `#[tokio::test]`).
pub fn is_test_attribute(attr: &Attribute) -> bool {
    is_test_path(attr.path())
}

/// Whether a `cfg` predicate can only hold when compiling tests.
fn cfg_requires_test(meta: &Meta) -> bool {
    match meta {
        Meta::Path(p) => p.is_ident("test"),
        Meta::List(list) => {
            let args = list
                .parse_args_with(
                    syn::punctuated::Punctuated::<Meta, syn::Token![,]>::parse_terminated,
                )
                .map(|a| a.into_iter().collect::<Vec<_>>())
                .unwrap_or_default();
            if list.path.is_ident("all") {
                args.iter().any(cfg_requires_test)
            } else if list.path.is_ident("any") {
                !args.is_empty() && args.iter().all(cfg_requires_test)
            } else {
                false
            }
        }
        Meta::NameValue(_) => false,
    }
}

/// `path` relative to `root`, with `/` separators.
pub fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// Builders for the rule modules' unit tests.
#[cfg(test)]
pub mod testing {
    use super::*;

    /// A policy with every rule crate in its usual category, plane and purity role.
    pub fn policy() -> Policy {
        super::super::policy::parse(
            "schema = 1\n[workspace]\nproduction = [\"mayfly-domain\", \"mayfly-protocol\", \"mayfly-control-domain\", \"mayfly-control\", \"mayfly-data-domain\", \"mayfly-data\", \"mayfly-adapter-sqlite\", \"mayfly-adapter-github\", \"mayfly-adapter-secrets\", \"mayfly-adapter-tart\", \"mayfly-adapter-host\", \"mayfly-adapter-link\", \"mayfly-composition\", \"mayfly-cli\"]\ntest_only = [\"mayfly-testkit\", \"mayfly-conformance\", \"mayfly-fake-tart\", \"mayfly-fake-github\"]\ntool_only = [\"xtask\", \"mayfly-faultlab\"]\n[dependencies.normal]\n[planes]\ncontrol = [\"mayfly-control-domain\", \"mayfly-control\", \"mayfly-adapter-sqlite\", \"mayfly-adapter-github\", \"mayfly-adapter-secrets\", \"mayfly-testkit\"]\ndata = [\"mayfly-data-domain\", \"mayfly-data\", \"mayfly-adapter-tart\", \"mayfly-adapter-host\"]\nshared = [\"mayfly-domain\", \"mayfly-protocol\", \"mayfly-adapter-link\"]\nboth = [\"mayfly-composition\", \"mayfly-cli\", \"mayfly-conformance\"]\n[planes.vocabulary]\ncontrol = [\"Journal\", \"AggregateStore\"]\ndata = [\"VmProvider\", \"GuestTransport\"]\n[purity]\npure = [\"mayfly-domain\", \"mayfly-protocol\", \"mayfly-control-domain\", \"mayfly-data-domain\"]\nprocess_authority = [\"mayfly-adapter-tart\", \"mayfly-composition\"]\nindependent = [\"mayfly-faultlab\", \"mayfly-fake-tart\", \"mayfly-fake-github\"]\n",
        )
        .unwrap()
    }

    /// The facts of a library package named `package` whose files (relative to the package
    /// directory, the first being the root `src/lib.rs`) hold `files`.
    pub fn facts_for(package: &str, files: &[(&str, &str)]) -> Facts {
        library_for(package, files).0
    }

    /// The facts of several library packages, each given as its name and the text of its
    /// `src/lib.rs`, with every path into another of them followed through its re-exports,
    /// as [`check`] does.
    pub fn workspace(packages: &[(&str, &str)]) -> Vec<Facts> {
        let mut libraries = Libraries::new();
        let mut every = Vec::new();
        for &(package, lib) in packages {
            let (facts, scopes) = library_for(package, &[("src/lib.rs", lib)]);
            libraries.insert(package.replace('-', "_"), scopes);
            every.push(facts);
        }
        for facts in &mut every {
            follow_reexports(&libraries, &mut facts.references);
            follow_type_refs(&libraries, facts);
        }
        every
    }

    /// The facts of a package named `package` with a library (`src/lib.rs` holding `lib`)
    /// and a binary (`src/main.rs` holding `bin`), keyed as [`check`] keys them.
    pub fn package_with_binary(package: &str, lib: &str, bin: &str) -> Facts {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();
        let package_dir = root.join("crates").join(package);
        let mut facts = Facts {
            package: package.to_owned(),
            dir: format!("crates/{package}"),
            ..Facts::default()
        };
        let library = package.replace('-', "_");
        let binary = format!("{package} (target {package})");
        for (rel, text, key, is_library) in [
            ("src/lib.rs", lib, &library, true),
            ("src/main.rs", bin, &binary, false),
        ] {
            let path = package_dir.join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, text).unwrap();
            let mut loader = Loader::new(root, &package_dir);
            loader.load_file(&path, Vec::new(), path.parent().unwrap(), false, 0, None);
            facts.sources.extend(loader.texts.clone());
            let place = collect::Target {
                root,
                package_dir: &package_dir,
                library: is_library,
                key,
            };
            collect::collect_target(&place, &loader.units, BTreeMap::new(), &mut facts);
        }
        facts
    }

    /// [`facts_for`], with the library's scopes.
    fn library_for(package: &str, files: &[(&str, &str)]) -> (Facts, paths::Scopes) {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();
        let package_dir = root.join("crates").join(package);
        for (rel, text) in files {
            let path = package_dir.join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        }
        let lib = package_dir.join(files[0].0);
        let mut loader = Loader::new(root, &package_dir);
        loader.load_file(&lib, Vec::new(), lib.parent().unwrap(), false, 0, None);
        let mut facts = Facts {
            package: package.to_owned(),
            dir: format!("crates/{package}"),
            problems: loader.problems.clone(),
            foreign: loader.foreign.clone(),
            sources: loader.texts.clone(),
            ..Facts::default()
        };
        let key = package.replace('-', "_");
        let place = collect::Target {
            root,
            package_dir: &package_dir,
            library: true,
            key: &key,
        };
        let scopes = collect::collect_target(&place, &loader.units, BTreeMap::new(), &mut facts);
        (facts, scopes)
    }

    /// Diagnostics as their rendered lines.
    pub fn rendered(diagnostics: &[Diagnostic]) -> Vec<String> {
        diagnostics.iter().map(ToString::to_string).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attrs(source: &str) -> Vec<Attribute> {
        let item: syn::ItemFn = syn::parse_str(&format!("{source} fn f() {{}}")).unwrap();
        item.attrs
    }

    #[test]
    fn should_mark_item_test_only_when_cfg_requires_test() {
        // given
        let cases = [
            ("#[cfg(test)]", true),
            ("#[cfg(all(test, unix))]", true),
            ("#[cfg(any(test, unix))]", false),
            ("#[cfg(not(test))]", false),
            ("#[tokio::test]", true),
            ("#[cfg(unix)]", false),
        ];

        // when
        let got: Vec<(&str, bool)> = cases
            .iter()
            .map(|(src, _)| (*src, is_test_only(&attrs(src))))
            .collect();

        // then
        assert_eq!(got, cases);
    }

    #[test]
    fn should_read_every_branch_when_path_is_given_through_cfg_attr() {
        // given
        let attrs = attrs(
            r#"#[cfg_attr(unix, path = "u.rs")] #[cfg_attr(windows, path = concat!("w", ".rs"))]"#,
        );

        // when
        let paths = path_attributes(&attrs);

        // then
        assert_eq!(
            paths,
            [
                PathAttr {
                    value: Ok("u.rs".to_owned()),
                    conditional: true
                },
                PathAttr {
                    value: Err(()),
                    conditional: true
                }
            ]
        );
    }

    #[test]
    fn should_follow_reexports_when_another_crate_names_them() {
        // given
        let composition = testing::facts_for(
            "mayfly-composition",
            &[(
                "src/lib.rs",
                "pub use std::process::Command as Spawner;\npub use self::inner::Journal as Log;\nmod inner { pub trait Journal {} }\n",
            )],
        );
        let dir = tempfile::TempDir::new().unwrap();
        let file = dir.path().join("lib.rs");
        fs::write(
            &file,
            &composition.sources["crates/mayfly-composition/src/lib.rs"],
        )
        .unwrap();
        let mut loader = Loader::new(dir.path(), dir.path());
        loader.load_file(&file, Vec::new(), dir.path(), false, 0, None);
        let place = collect::Target {
            root: dir.path(),
            package_dir: dir.path(),
            library: true,
            key: "mayfly_composition",
        };
        let scopes = collect::collect_target(
            &place,
            &loader.units,
            BTreeMap::new(),
            &mut Facts::default(),
        );
        let libraries = Libraries::from([("mayfly_composition".to_owned(), scopes)]);
        let mut cli = testing::facts_for(
            "mayfly-cli",
            &[(
                "src/lib.rs",
                "pub fn run(_: &dyn mayfly_composition::Log) {\n    let _ = mayfly_composition::Spawner::new(\"sh\");\n}\n",
            )],
        );

        // when
        follow_reexports(&libraries, &mut cli.references);

        // then
        let all: Vec<String> = cli
            .references
            .iter()
            .flat_map(|r| r.resolved.iter().map(|c| c.join("::")))
            .collect();
        assert!(
            all.contains(&"std::process::Command::new".to_owned()),
            "{all:?}"
        );
        assert!(
            all.contains(&"mayfly_composition::inner::Journal".to_owned()),
            "{all:?}"
        );
    }

    #[test]
    fn should_refuse_path_and_include_when_they_leave_the_package() {
        // given
        let files = [
            (
                "src/lib.rs",
                "#[path = \"../../other/src/lib.rs\"]\nmod x;\npub fn f() -> u8 {\n    include!(\"../../other/value.rs\")\n}\n",
            ),
            ("../other/src/lib.rs", "pub fn g() {}\n"),
            ("../other/value.rs", "1\n"),
        ];

        // when
        let facts = testing::facts_for("p", &files);

        // then
        let got: Vec<&str> = facts.foreign.iter().map(|(at, _)| at.as_str()).collect();
        assert_eq!(
            got,
            ["crates/p/src/lib.rs:2", "crates/p/src/lib.rs:4"],
            "{:?}",
            facts.foreign
        );
        assert!(facts.problems.is_empty(), "{:?}", facts.problems);
        assert!(!facts.sources.contains_key("crates/other/src/lib.rs"));
    }

    #[test]
    fn should_report_line_when_file_does_not_parse() {
        // given
        let dir = tempfile::TempDir::new().unwrap();
        let file = dir.path().join("lib.rs");
        fs::write(&file, "pub fn ok() {}\npub fn broken( {\n").unwrap();
        let mut loader = Loader::new(dir.path(), dir.path());

        // when
        loader.load_file(&file, Vec::new(), dir.path(), false, 0, None);

        // then
        assert_eq!(loader.problems.len(), 1, "{:?}", loader.problems);
        assert!(
            loader.problems[0].0.starts_with("lib.rs:"),
            "{:?}",
            loader.problems
        );
        assert!(loader.problems[0].1.contains("does not parse"));
        assert!(loader.units.is_empty());
    }

    #[test]
    fn should_follow_modules_and_includes_when_tree_is_loaded() {
        // given
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("a")).unwrap();
        fs::write(
            root.join("lib.rs"),
            "mod a;\n#[cfg(test)]\nmod t { mod inner; }\n#[path = \"other.rs\"]\nmod o;\ninclude!(\"gen.rs\");\n",
        )
        .unwrap();
        fs::write(root.join("a.rs"), "#![cfg(test)]\nmod b;\n").unwrap();
        fs::write(root.join("a/b.rs"), "").unwrap();
        fs::create_dir_all(root.join("t")).unwrap();
        fs::write(root.join("t/inner.rs"), "").unwrap();
        fs::write(root.join("other.rs"), "").unwrap();
        fs::write(root.join("gen.rs"), "fn generated() {}\n").unwrap();
        let mut loader = Loader::new(root, root);

        // when
        loader.load_file(&root.join("lib.rs"), Vec::new(), root, false, 0, None);

        // then
        assert!(loader.problems.is_empty(), "{:?}", loader.problems);
        let mut loaded: Vec<(String, String, bool)> = loader
            .units
            .iter()
            .map(|u| (u.module.join("::"), relative(root, &u.file), u.test))
            .collect();
        loaded.sort();
        assert_eq!(
            loaded,
            [
                (String::new(), "gen.rs".to_owned(), false),
                (String::new(), "lib.rs".to_owned(), false),
                ("a".to_owned(), "a.rs".to_owned(), true),
                ("a::b".to_owned(), "a/b.rs".to_owned(), true),
                ("o".to_owned(), "other.rs".to_owned(), false),
                ("t".to_owned(), "lib.rs".to_owned(), true),
                ("t::inner".to_owned(), "t/inner.rs".to_owned(), true),
            ]
        );
    }

    #[test]
    fn should_fail_when_module_file_or_literal_path_is_missing() {
        // given
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();
        fs::write(
            root.join("lib.rs"),
            "mod gone;\ninclude!(concat!(env!(\"OUT_DIR\"), \"/gen.rs\"));\n",
        )
        .unwrap();
        let mut loader = Loader::new(root, root);

        // when
        loader.load_file(&root.join("lib.rs"), Vec::new(), root, false, 0, None);

        // then
        let got: Vec<String> = loader
            .problems
            .iter()
            .map(|(at, m)| format!("{at}: {m}"))
            .collect();
        assert_eq!(got.len(), 2, "{got:?}");
        assert!(
            got[0].starts_with("lib.rs:1: module `gone` has no source file"),
            "{got:?}"
        );
        assert!(
            got[1].starts_with("lib.rs:2: include! with a path"),
            "{got:?}"
        );
    }
}
