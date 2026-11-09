//! Hygiene and test quality (foundation.md sections 3.3 and 5.2; doc/tasks.md rules 4 and
//! 5): no unfinished code in production, no ignored tests, no unapproved lint suppression
//! (nor one named by a macro metavariable), no nightly features or macro 2.0 items,
//! no sleeps in tests outside the bounded poll helper, named, structured tests, and no
//! `cfg!` platform branches in `crates/*/src`.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use super::super::POLICY_FILE;
use super::super::policy::{LintException, Policy};
use super::super::report::{Diagnostic, Rule};
use super::{Context, Facts, METAVARIABLE_ATTRIBUTE, RefKind, Site, first_hit};

/// The `lint` an `[[exceptions.lints]]` entry names to approve a `#[macro_export]`.
pub const MACRO_EXPORT: &str = "macro_export";

/// The `lint` an `[[exceptions.lints]]` entry names to approve real-time waiting in one file of a
/// test-only crate (a fake tool binary, whose delays are its behaviour, not a test's wait). It is
/// honoured only in that crate's production code; a `tests/` file or a test module never gets it.
pub const TEST_SLEEP: &str = "test_sleep";

/// Macros that mark unfinished code, with the clippy lint an exception names.
const UNFINISHED: &[(&str, &str)] = &[
    ("todo", "clippy::todo"),
    ("unimplemented", "clippy::unimplemented"),
    ("dbg", "clippy::dbg_macro"),
];

/// Calls that block a test on wall-clock time: sleeps, timed parks and receives, and tokio's
/// timers. A method call on a value (`rx.recv_timeout(..)`) is not type-resolved; only the
/// path form is seen.
const SLEEPS: &[&[&str]] = &[
    &["std", "thread", "sleep"],
    &["std", "thread", "sleep_ms"],
    &["std", "thread", "park_timeout"],
    &["std", "sync", "mpsc", "Receiver", "recv_timeout"],
    &["std", "sync", "mpsc", "Receiver", "recv_deadline"],
    &["tokio", "time", "sleep"],
    &["tokio", "time", "sleep_until"],
    &["tokio", "time", "timeout"],
    &["tokio", "time", "timeout_at"],
    &["tokio", "time", "interval"],
    &["tokio", "time", "interval_at"],
];

/// The bounded polling helper: the one place test support may sleep, in its production code.
/// Package and file inside it. It lives in the shared test kit, not in a plane's kit, so that
/// both planes' tests can use it (ADR 0019).
pub const POLL_HELPER: (&str, &str) = ("mayfly-testkit-shared", "src/poll.rs");

/// Whether the poll helper's file must exist (`ARCH-missing-selector`): the exemption names a
/// file, and a rename of it would otherwise leave an exemption for nothing and a helper that
/// no rule watches.
pub const POLL_HELPER_REQUIRED: bool = true;

/// The hygiene rules over all packages: suppressions are matched against
/// `[[exceptions.lints]]`, and entries that match nothing are reported by [`Hygiene::finish`].
pub struct Hygiene<'a> {
    policy: &'a Policy,
    adr_exists: &'a dyn Fn(&str) -> bool,
    used: BTreeSet<usize>,
    vendor_used: BTreeSet<String>,
}

impl<'a> Hygiene<'a> {
    /// Rules for `policy`; `adr_exists` checks an exception's ADR.
    pub fn new(policy: &'a Policy, adr_exists: &'a dyn Fn(&str) -> bool) -> Self {
        Self {
            policy,
            adr_exists,
            used: BTreeSet::new(),
            vendor_used: BTreeSet::new(),
        }
    }

    /// The diagnostics for one package.
    pub fn check(&mut self, facts: &Facts) -> Vec<Diagnostic> {
        let mut out = Vec::new();
        self.unfinished(facts, &mut out);
        self.suppressions(facts, &mut out);
        self.macro_exports(facts, &mut out);
        self.sleeps(facts, &mut out);
        tests(facts, &mut out);
        platform_branches(facts, &mut out);
        nightly(facts, &mut out);
        out
    }

    /// `ARCH-lint-config`: lints relaxed outside Rust source. `root` is the workspace root and
    /// `manifests` every member's manifest, relative to it.
    pub fn config(&mut self, root: &Path, manifests: &[String]) -> Vec<Diagnostic> {
        let mut out = Vec::new();
        let workspace_lints = self.workspace_lints(root, &mut out);
        for manifest in manifests {
            let lints = read_toml(root, manifest, &mut out).and_then(|t| t.get("lints").cloned());
            let inherits = lints.as_ref().is_some_and(|l| {
                l.as_table().is_some_and(|t| {
                    t.len() == 1 && t.get("workspace") == Some(&toml::Value::Boolean(true))
                })
            });
            let problem = match (&lints, inherits) {
                (Some(_), false) => Some("its [lints] must be exactly `workspace = true`"),
                (None, _) if workspace_lints => Some(
                    "it has no `[lints] workspace = true`, so it inherits none of [workspace.lints]",
                ),
                _ => None,
            };
            if let Some(problem) = problem {
                out.push(Diagnostic::new(Rule::LintConfig, problem, manifest.clone()));
            }
        }
        for rel in cargo_configs(root, manifests) {
            let dir = rel
                .strip_suffix(".cargo/config.toml")
                .or_else(|| rel.strip_suffix(".cargo/config"))
                .unwrap_or_default()
                .to_owned();
            if let Some(config) = read_toml(root, &rel, &mut out) {
                rustflags(&config, &rel, &mut out);
                build_steering(&config, &rel, &mut out);
                self.sources(&config, &dir, &rel, &mut out);
            }
        }
        let mut dirs: Vec<String> = vec![String::new()];
        dirs.extend(
            manifests
                .iter()
                .filter_map(|m| m.rsplit_once('/').map(|(dir, _)| format!("{dir}/"))),
        );
        for dir in &dirs {
            for name in ["clippy.toml", ".clippy.toml"] {
                let rel = format!("{dir}{name}");
                if root.join(&rel).is_file()
                    && let Some(config) = read_toml(root, &rel, &mut out)
                {
                    for (key, value) in &config {
                        if key.starts_with("allow") && value != &toml::Value::Boolean(false) {
                            out.push(Diagnostic::new(
                                Rule::LintConfig,
                                format!("`{key} = {value}` relaxes clippy outside the source"),
                                rel.clone(),
                            ));
                        }
                    }
                }
            }
        }
        for (dir, approval) in &self.policy.exceptions.vendor {
            if !self.vendor_used.contains(dir) {
                out.push(Diagnostic::new(
                    Rule::LintConfig,
                    format!(
                        "[exceptions.vendor] `{dir}` (ADR {}) is substituted by no .cargo/config.toml; remove it",
                        approval.adr
                    ),
                    POLICY_FILE,
                ));
            }
        }
        out
    }

    /// `[source]` replacement in a `.cargo/config.toml` under `dir` (the workspace root or a
    /// member, `dir` ending in `/` or empty): every crate would then come from somewhere
    /// `cargo deny` and the lockfile do not show. Only a `directory` source inside the
    /// workspace that `[exceptions.vendor]` approves with an existing ADR passes.
    fn sources(&mut self, config: &toml::Table, dir: &str, rel: &str, out: &mut Vec<Diagnostic>) {
        let Some(sources) = config.get("source").and_then(toml::Value::as_table) else {
            return;
        };
        for (name, source) in sources {
            let Some(target) = source.get("replace-with").and_then(toml::Value::as_str) else {
                continue;
            };
            let directory = sources
                .get(target)
                .and_then(|t| t.get("directory"))
                .and_then(toml::Value::as_str)
                .map(|d| format!("{dir}{d}"));
            let approved = directory.as_ref().and_then(|d| {
                self.policy
                    .exceptions
                    .vendor
                    .get(d)
                    .map(|approval| (d.clone(), approval.adr.clone()))
            });
            match approved {
                Some((d, adr)) if (self.adr_exists)(&adr) => {
                    self.vendor_used.insert(d);
                }
                Some((d, adr)) => {
                    self.vendor_used.insert(d.clone());
                    out.push(Diagnostic::new(
                        Rule::LintConfig,
                        format!(
                            "[exceptions.vendor] `{d}` cites ADR {adr}, but no doc/adr/{adr}-*.md exists"
                        ),
                        rel,
                    ));
                }
                None => out.push(Diagnostic::new(
                    Rule::LintConfig,
                    format!(
                        "[source.{name}] is replaced with `{target}`{}: packages would come from a source the lockfile and cargo-deny do not show; only an approved vendored directory ([exceptions.vendor]) may replace one",
                        directory.map(|d| format!(" (directory `{d}`)")).unwrap_or_default()
                    ),
                    rel,
                )),
            }
        }
    }

    /// Reports `allow` levels in the root `[workspace.lints]` that no exception approves;
    /// returns whether the table exists.
    fn workspace_lints(&mut self, root: &Path, out: &mut Vec<Diagnostic>) -> bool {
        let Some(manifest) = read_toml(root, "Cargo.toml", out) else {
            return false;
        };
        let Some(tools) = manifest
            .get("workspace")
            .and_then(|w| w.get("lints"))
            .and_then(toml::Value::as_table)
        else {
            return false;
        };
        let site = Site {
            file: "Cargo.toml".to_owned(),
            line: 0,
            context: Context::Production,
        };
        for (tool, lints) in tools {
            for (lint, level) in lints.as_table().into_iter().flatten() {
                let level = level
                    .as_str()
                    .or_else(|| level.get("level").and_then(toml::Value::as_str));
                if level != Some("allow") {
                    continue;
                }
                let name = if tool == "rust" {
                    lint.clone()
                } else {
                    format!("{tool}::{lint}")
                };
                if let Err(reason) = self.excepted(&site, &name) {
                    out.push(Diagnostic::new(
                        Rule::LintConfig,
                        format!("[workspace.lints] allows `{name}`; {reason}"),
                        "Cargo.toml",
                    ));
                }
            }
        }
        true
    }

    /// Entries in `[[exceptions.lints]]` that no suppression used.
    pub fn finish(&self) -> Vec<Diagnostic> {
        self.policy
            .exceptions
            .lints
            .iter()
            .enumerate()
            .filter(|(i, _)| !self.used.contains(i))
            .map(|(_, e)| {
                Diagnostic::new(
                    Rule::LintSuppression,
                    format!(
                        "[[exceptions.lints]] entry `{}` for `{}` (ADR {}) matches no suppression; remove it",
                        e.lint, e.path, e.adr
                    ),
                    POLICY_FILE,
                )
            })
            .collect()
    }

    /// Looks up the exception for `lint` in `site`'s file. `Ok` when one cites an existing
    /// ADR; otherwise the reason.
    fn excepted(&mut self, site: &Site, lint: &str) -> Result<(), String> {
        let found = self
            .policy
            .exceptions
            .lints
            .iter()
            .enumerate()
            .filter(|(_, e)| e.path == site.file && e.lint == lint)
            .collect::<Vec<(usize, &LintException)>>();
        for (i, _) in &found {
            self.used.insert(*i);
        }
        match found.iter().find(|(_, e)| (self.adr_exists)(&e.adr)) {
            Some(_) => Ok(()),
            None => match found.first() {
                Some((_, e)) => Err(format!(
                    "its [[exceptions.lints]] entry cites ADR {}, but no doc/adr/{}-*.md exists",
                    e.adr, e.adr
                )),
                None => Err(format!(
                    "add an [[exceptions.lints]] entry (path = \"{}\", lint = \"{lint}\") citing an ADR, or remove it",
                    site.file
                )),
            },
        }
    }

    fn unfinished(&mut self, facts: &Facts, out: &mut Vec<Diagnostic>) {
        for reference in &facts.references {
            if reference.kind != RefKind::Macro || reference.site.context != Context::Production {
                continue;
            }
            let hit = reference.resolved.iter().find_map(|candidate| {
                let (last, prefix) = candidate.split_last()?;
                let prelude = prefix.is_empty()
                    || (prefix.len() == 1
                        && matches!(prefix[0].as_str(), "std" | "core" | "alloc"));
                UNFINISHED.iter().find(|(name, _)| prelude && last == name)
            });
            if let Some((name, lint)) = hit
                && let Err(reason) = self.excepted(&reference.site, lint)
            {
                out.push(Diagnostic::new(
                    Rule::UnfinishedCode,
                    format!(
                        "{} calls `{name}!` in production code; {reason}",
                        facts.package
                    ),
                    reference.site.at(),
                ));
            }
        }
    }

    /// `ARCH-macro-export`: a production crate exports no macro without an `[[exceptions.lints]]`
    /// entry (`lint = "macro_export"`) citing an ADR. An exported macro's body is checked where
    /// it is written, under that crate's rules (composition may spawn a process), and expands
    /// in another crate (the CLI) that names nothing. Pinned by
    /// `should_report_macro_export_when_production_crate_exports_without_exception`.
    fn macro_exports(&mut self, facts: &Facts, out: &mut Vec<Diagnostic>) {
        let production = self
            .policy
            .workspace
            .production
            .as_ref()
            .is_some_and(|p| p.contains(&facts.package));
        if !production {
            return;
        }
        for site in &facts.macro_exports {
            if let Err(reason) = self.excepted(site, MACRO_EXPORT) {
                out.push(Diagnostic::new(
                    Rule::MacroExport,
                    format!(
                        "{} exports a macro with `#[macro_export]`: its body expands in other crates, outside this crate's rules; {reason}",
                        facts.package
                    ),
                    site.at(),
                ));
            }
        }
    }

    fn suppressions(&mut self, facts: &Facts, out: &mut Vec<Diagnostic>) {
        for attribute in &facts.attributes {
            if attribute.name == "ignore" {
                out.push(Diagnostic::new(
                    Rule::IgnoredTest,
                    format!(
                        "{} has `#[ignore]`: a test either runs or is deleted",
                        facts.package
                    ),
                    attribute.site.at(),
                ));
                continue;
            }
            if attribute.name == METAVARIABLE_ATTRIBUTE
                || attribute.lints.iter().any(|l| l.contains('$'))
            {
                // No exception approves it: the lint and its level are whatever a caller
                // passes, so no row can say which suppression it approves.
                out.push(Diagnostic::new(
                    Rule::LintSuppression,
                    format!(
                        "{} writes `#[{}]` in a macro: an attribute named by a metavariable sets a lint level the checker cannot read (a caller could pass `allow`); write the attribute out, or forward a whole `#[$m:meta]` the caller writes",
                        facts.package,
                        attribute.lints.join(", ")
                    ),
                    attribute.site.at(),
                ));
                continue;
            }
            for lint in &attribute.lints {
                if let Err(reason) = self.excepted(&attribute.site, lint) {
                    let shown = if attribute.name == "coverage" {
                        "coverage(off)".to_owned()
                    } else {
                        format!("{}({lint})", attribute.name)
                    };
                    out.push(Diagnostic::new(
                        Rule::LintSuppression,
                        format!("{} has `#[{shown}]`; {reason}", facts.package),
                        attribute.site.at(),
                    ));
                }
            }
        }
    }
}

/// Reads `rel` under `root` as TOML; a file that does not read or parse is reported.
fn read_toml(root: &Path, rel: &str, out: &mut Vec<Diagnostic>) -> Option<toml::Table> {
    let parsed = fs::read_to_string(root.join(rel))
        .map_err(|e| e.to_string())
        .and_then(|text| text.parse::<toml::Table>().map_err(|e| e.to_string()));
    match parsed {
        Ok(table) => Some(table),
        Err(err) => {
            out.push(Diagnostic::new(
                Rule::LintConfig,
                format!(
                    "cannot read lint configuration: {}",
                    err.lines().next().unwrap_or("")
                ),
                rel,
            ));
            None
        }
    }
}

/// Every Cargo configuration file in the repository, relative to `root`: the root's, each
/// member's (`manifests`), and any other `.cargo/config.toml` or `.cargo/config` below the
/// root, because Cargo reads the one of every directory from where it runs up to the root.
/// The walk skips `target`, `.git`, `node_modules` and nested workspaces (a directory whose
/// `Cargo.toml` has a `[workspace]` table: the fuzz crate and the checker's fixtures), whose
/// configuration their own run checks.
pub fn cargo_configs(root: &Path, manifests: &[String]) -> Vec<String> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeSet<String>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if !path.is_dir() || path.is_symlink() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if name == ".cargo" {
                for file in ["config.toml", "config"] {
                    if path.join(file).is_file() {
                        out.insert(super::relative(root, &path.join(file)));
                    }
                }
                continue;
            }
            let nested = fs::read_to_string(path.join("Cargo.toml"))
                .ok()
                .and_then(|t| t.parse::<toml::Table>().ok())
                .is_some_and(|t| t.contains_key("workspace"));
            if matches!(name.as_str(), "target" | ".git" | "node_modules") || nested {
                continue;
            }
            walk(root, &path, out);
        }
    }
    let mut out = BTreeSet::new();
    let mut dirs: Vec<String> = vec![String::new()];
    dirs.extend(
        manifests
            .iter()
            .filter_map(|m| m.rsplit_once('/').map(|(dir, _)| format!("{dir}/"))),
    );
    for dir in &dirs {
        for name in [".cargo/config.toml", ".cargo/config"] {
            let rel = format!("{dir}{name}");
            if root.join(&rel).is_file() {
                out.insert(rel);
            }
        }
    }
    walk(root, root, &mut out);
    out.into_iter().collect()
}

/// Whether a Cargo configuration's `[env]` must not set `name`: every variable the xtask
/// preflight refuses ([`crate::repo::steering`]; `RUSTFLAGS` with any value here), except
/// `CLIPPY_CONF_DIR`, which `ARCH-pure-clippy-config` reports.
fn steering_env(name: &str) -> bool {
    name != "CLIPPY_CONF_DIR" && crate::repo::steering(name, std::ffi::OsStr::new(""))
}

/// The one `[alias]` the repository keeps: `cargo xtask`, exactly as `.cargo/config.toml`
/// defines it. Any other alias could shadow a subcommand the gate runs (`clippy = "check"`).
const XTASK_ALIAS: (&str, &str) = ("xtask", "run --locked --package xtask --");

/// `ARCH-lint-config`: a Cargo configuration that swaps or wraps the compiler (`[build] rustc`,
/// `rustc-wrapper`, `rustc-workspace-wrapper`, `rustdoc`), sets a build-steering `[env]`
/// variable ([`steering_env`]), unlocks nightly options (`[unstable]`), runs tests through a
/// `[target.*] runner`, overrides dependencies (`paths`), or aliases anything but
/// [`XTASK_ALIAS`]: each would let Clippy or the tests report green over code they never
/// checked or ran. Pinned by `should_report_lint_config_when_cargo_config_steers_the_build`.
fn build_steering(config: &toml::Table, rel: &str, out: &mut Vec<Diagnostic>) {
    let mut refuse = |what: String| {
        out.push(Diagnostic::new(
            Rule::LintConfig,
            format!(
                "{what} in a Cargo configuration steers the compiler or the tests past the gate"
            ),
            rel,
        ));
    };
    for key in [
        "rustc",
        "rustc-wrapper",
        "rustc-workspace-wrapper",
        "rustdoc",
    ] {
        if config.get("build").and_then(|b| b.get(key)).is_some() {
            refuse(format!("`[build] {key}`"));
        }
    }
    for name in config
        .get("env")
        .and_then(toml::Value::as_table)
        .into_iter()
        .flat_map(|t| t.keys())
    {
        if steering_env(name) {
            refuse(format!("`[env] {name}`"));
        }
    }
    if config.contains_key("unstable") {
        refuse("`[unstable]`".to_owned());
    }
    if config.contains_key("paths") {
        refuse("`paths`".to_owned());
    }
    for (triple, target) in config
        .get("target")
        .and_then(toml::Value::as_table)
        .into_iter()
        .flatten()
    {
        if target.get("runner").is_some() {
            refuse(format!("`[target.{triple}] runner`"));
        }
    }
    for (name, value) in config
        .get("alias")
        .and_then(toml::Value::as_table)
        .into_iter()
        .flatten()
    {
        let text = match value {
            toml::Value::String(s) => s.split_whitespace().collect::<Vec<_>>().join(" "),
            toml::Value::Array(a) => a
                .iter()
                .filter_map(toml::Value::as_str)
                .collect::<Vec<_>>()
                .join(" "),
            _ => String::new(),
        };
        if (name.as_str(), text.as_str()) != XTASK_ALIAS {
            refuse(format!(
                "`[alias] {name}` (only `{} = \"{}\"` is kept)",
                XTASK_ALIAS.0, XTASK_ALIAS.1
            ));
        }
    }
}

/// Reports rustflags in a Cargo config that allow or cap lints, or pass `-Z` options
/// (`-Zcrate-attr=allow(..)` injects a crate attribute no source shows).
fn rustflags(config: &toml::Table, rel: &str, out: &mut Vec<Diagnostic>) {
    let mut lists: Vec<&toml::Value> = Vec::new();
    for key in ["rustflags", "rustdocflags"] {
        lists.extend(config.get("build").and_then(|b| b.get(key)));
    }
    for target in config
        .get("target")
        .and_then(toml::Value::as_table)
        .into_iter()
        .flat_map(|t| t.values())
    {
        lists.extend(target.get("rustflags"));
    }
    for value in lists {
        let flags: Vec<String> = match value {
            toml::Value::String(s) => s.split_whitespace().map(str::to_owned).collect(),
            toml::Value::Array(a) => a
                .iter()
                .filter_map(toml::Value::as_str)
                .map(str::to_owned)
                .collect(),
            toml::Value::Table(t) => t
                .get("value")
                .and_then(toml::Value::as_str)
                .map(|s| s.split_whitespace().map(str::to_owned).collect())
                .unwrap_or_default(),
            _ => Vec::new(),
        };
        for flag in flags {
            if flag.starts_with("-A")
                || flag.starts_with("--allow")
                || flag.starts_with("--cap-lints")
            {
                out.push(Diagnostic::new(
                    Rule::LintConfig,
                    format!("rustflags `{flag}` allow or cap lints outside the source"),
                    rel,
                ));
            } else if flag.starts_with("-Z") {
                out.push(Diagnostic::new(
                    Rule::LintConfig,
                    format!(
                        "rustflags `{flag}` pass a nightly `-Z` option (`-Zcrate-attr` injects attributes no source shows)"
                    ),
                    rel,
                ));
            }
        }
    }
}

impl Hygiene<'_> {
    /// `ARCH-test-sleep`. A test-only crate's production code may wait in real time only in a file
    /// an `[[exceptions.lints]]` entry (`lint = "test_sleep"`) approves with an existing ADR.
    /// Pinned by `should_report_sleep_when_exception_is_missing_or_not_for_that_file`.
    fn sleeps(&mut self, facts: &Facts, out: &mut Vec<Diagnostic>) {
        let test_crate = self
            .policy
            .workspace
            .test_only
            .as_ref()
            .is_some_and(|t| t.contains(&facts.package));
        for reference in &facts.references {
            let production = reference.site.context == Context::Production;
            let test = !production || test_crate;
            let poll_helper = facts.package == POLL_HELPER.0
                && facts.in_package(&reference.site) == POLL_HELPER.1
                && production;
            if !test || poll_helper {
                continue;
            }
            let Some(candidate) = first_hit(reference, SLEEPS) else {
                continue;
            };
            let excepted = if test_crate && production {
                self.excepted(&reference.site, TEST_SLEEP)
            } else {
                Err(String::new())
            };
            if excepted.is_ok() {
                continue;
            }
            out.push(Diagnostic::new(
                Rule::TestSleep,
                format!(
                    "{} test code calls `{}`: wait on an event, an injected clock or `{}::poll::wait_until` instead{}",
                    facts.package,
                    candidate,
                    POLL_HELPER.0.replace('-', "_"),
                    match excepted {
                        Err(reason) if !reason.is_empty() && test_crate => format!(" ({reason})"),
                        _ => String::new(),
                    }
                ),
                reference.site.at(),
            ));
        }
    }
}

fn tests(facts: &Facts, out: &mut Vec<Diagnostic>) {
    for test in &facts.tests {
        let Some((open, close)) = test.body else {
            out.push(Diagnostic::new(
                Rule::TestName,
                format!(
                    "{} defines test `{}` inside a macro body, where its name and structure cannot be checked; write it as a plain function",
                    facts.package, test.name
                ),
                test.site.at(),
            ));
            continue;
        };
        if !well_named(&test.name) {
            out.push(Diagnostic::new(
                Rule::TestName,
                format!(
                    "{} test `{}` does not match should_<outcome>_when_<condition> (^should_[a-z0-9_]+_when_[a-z0-9_]+$)",
                    facts.package, test.name
                ),
                test.site.at(),
            ));
        }
        let text = facts
            .sources
            .get(&test.site.file)
            .map_or("", String::as_str);
        let body: Vec<&str> = text
            .lines()
            .skip(open.saturating_sub(1))
            .take(close + 1 - open.min(close))
            .collect();
        if let Err(missing) = given_when_then(&body) {
            out.push(Diagnostic::new(
                Rule::TestStructure,
                format!(
                    "{} test `{}` lacks {missing}: write // given, // when and // then in that order",
                    facts.package, test.name
                ),
                test.site.at(),
            ));
        }
    }
}

/// `ARCH-nightly-features`: a `#![feature(..)]` (also through `cfg_attr` or in a macro's
/// tokens) or a macro 2.0 item, in any crate. The toolchain is a pinned stable (ADR 0006), so
/// such code compiles only with `RUSTC_BOOTSTRAP`, which `ARCH-lint-config` and the xtask
/// preflight refuse; the feature it unlocks (`register_tool`, `custom_inner_attributes`,
/// `rustc_attrs`) could change what the lints see. Pinned by
/// `should_report_nightly_when_feature_or_macro_two_point_zero_is_written`.
fn nightly(facts: &Facts, out: &mut Vec<Diagnostic>) {
    for (site, what) in &facts.nightly {
        out.push(Diagnostic::new(
            Rule::NightlyFeatures,
            format!(
                "{} writes {what}: a nightly-only construct on a stable toolchain (ADR 0006); remove it",
                facts.package
            ),
            site.at(),
        ));
    }
}

/// `ARCH-platform-branch`: a `cfg!` that names a platform, in `crates/*/src`. Such a
/// branch compiles every platform's arm everywhere, so on Linux the macOS arm is instrumented
/// but never run, and the changed-lines gate counts it uncovered; `#[cfg]` items or blocks
/// compile only the platform's own code.
fn platform_branches(facts: &Facts, out: &mut Vec<Diagnostic>) {
    if !facts.dir.starts_with("crates/") {
        return;
    }
    let src = format!("{}/src/", facts.dir);
    for (site, predicate) in &facts.platform_cfgs {
        if site.file.starts_with(&src) {
            out.push(Diagnostic::new(
                Rule::PlatformBranch,
                format!(
                    "{} branches at run time on `cfg!({predicate})`: write the platform code as `#[cfg({predicate})]` items or blocks",
                    facts.package
                ),
                site.at(),
            ));
        }
    }
}

/// Whether `name` matches `^should_[a-z0-9_]+_when_[a-z0-9_]+$`.
fn well_named(name: &str) -> bool {
    let Some(rest) = name.strip_prefix("should_") else {
        return false;
    };
    name.bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        && rest
            .match_indices("_when_")
            .any(|(i, _)| i > 0 && i + "_when_".len() < rest.len())
}

/// Checks that `body` has `// given`, `// when` and `// then` comment lines, in order.
fn given_when_then(body: &[&str]) -> Result<(), String> {
    let mut from = 0;
    for marker in ["// given", "// when", "// then"] {
        let found = body[from..].iter().position(|line| {
            line.trim()
                .strip_prefix(marker)
                .is_some_and(|rest| rest.is_empty() || !rest.starts_with(char::is_alphanumeric))
        });
        match found {
            Some(at) => from += at + 1,
            None => return Err(format!("`{marker}`")),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_report_lint_config_when_relaxed_outside_source() {
        // given
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join(".cargo")).unwrap();
        fs::create_dir_all(root.join("crates/a")).unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[workspace.lints.rust]\ndead_code = \"allow\"\nunused = { level = \"allow\", priority = 1 }\n[workspace.lints.clippy]\nall = \"deny\"\n",
        )
        .unwrap();
        fs::write(
            root.join("crates/a/Cargo.toml"),
            "[package]\nname = \"a\"\n",
        )
        .unwrap();
        fs::write(
            root.join(".cargo/config.toml"),
            "[target.x86_64-unknown-linux-gnu]\nrustflags = \"--cap-lints=warn\"\n",
        )
        .unwrap();
        let mut policy = super::super::testing::policy();
        policy.exceptions.lints.push(LintException {
            path: "Cargo.toml".to_owned(),
            lint: "unused".to_owned(),
            adr: "0001".to_owned(),
        });
        let adr_exists = |adr: &str| adr == "0001";
        let mut hygiene = Hygiene::new(&policy, &adr_exists);

        // when
        let got = super::super::testing::rendered(
            &hygiene.config(root, &["crates/a/Cargo.toml".to_owned()]),
        );

        // then
        assert_eq!(got.len(), 3, "{got:?}");
        assert!(got[0].contains("allows `dead_code`"), "{got:?}");
        assert!(got[1].contains("inherits none"), "{got:?}");
        assert!(got[2].contains("`--cap-lints=warn`"), "{got:?}");
        assert!(hygiene.finish().is_empty());
    }

    #[test]
    fn should_report_lint_config_when_cargo_config_steers_the_build() {
        // given: one configuration per way to steer the build, one in a directory that is
        // neither the root nor a member, one in a nested workspace (skipped: its own run checks
        // it), and the repository's own `xtask` alias, which passes
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();
        let write = |rel: &str, text: &str| {
            let path = root.join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        };
        write(
            "Cargo.toml",
            "[workspace]\nmembers = [\"crates/a\"]\n[workspace.lints.clippy]\nall = \"deny\"\n",
        );
        write(
            "crates/a/Cargo.toml",
            "[package]\nname = \"a\"\n[lints]\nworkspace = true\n",
        );
        write(
            ".cargo/config.toml",
            "paths = [\"../p\"]\n[alias]\nxtask = \"run --locked --package xtask --\"\n[build]\nrustc = \"r\"\nrustflags = [\"-Z\", \"crate-attr=allow(unused)\"]\n[unstable]\nbuild-std = [\"std\"]\n",
        );
        write(
            "crates/.cargo/config",
            "[target.x86_64-unknown-linux-gnu]\nrunner = \"true\"\n[env]\nRUSTFLAGS = \"-D warnings\"\nCARGO_BUILD_RUSTC_WRAPPER = \"w\"\nCLIPPY_CONF_DIR = \"c\"\nOTHER = \"1\"\n",
        );
        write("fuzz/Cargo.toml", "[workspace]\n");
        write(
            "fuzz/.cargo/config.toml",
            "[build]\nrustc-wrapper = \"w\"\n",
        );
        let policy = super::super::testing::policy();
        let adr_exists = |_: &str| true;
        let mut hygiene = Hygiene::new(&policy, &adr_exists);

        // when
        let got = super::super::testing::rendered(
            &hygiene.config(root, &["crates/a/Cargo.toml".to_owned()]),
        );

        // then
        let expected = [
            ("`-Z` option", ".cargo/config.toml"),
            ("`[build] rustc`", ".cargo/config.toml"),
            ("`[unstable]`", ".cargo/config.toml"),
            ("`paths`", ".cargo/config.toml"),
            ("`[env] CARGO_BUILD_RUSTC_WRAPPER`", "crates/.cargo/config"),
            ("`[env] RUSTFLAGS`", "crates/.cargo/config"),
            (
                "`[target.x86_64-unknown-linux-gnu] runner`",
                "crates/.cargo/config",
            ),
        ];
        assert_eq!(got.len(), expected.len(), "{got:?}");
        for (line, (what, at)) in got.iter().zip(expected) {
            assert!(
                line.contains(what) && line.ends_with(&format!("(at {at})")),
                "{got:?}"
            );
        }
    }

    #[test]
    fn should_report_nightly_when_feature_or_macro_two_point_zero_is_written() {
        // given
        let source = "#![feature(register_tool)]\n#![cfg_attr(all(), feature(rustc_attrs))]\npub macro m() {}\nmacro_rules! n { () => { #![feature(x)] pub(crate) macro inner() {} }; }\nfn r#macro() {}\n";
        let facts = super::super::testing::facts_for("mayfly-domain", &[("src/lib.rs", source)]);
        let policy = super::super::testing::policy();
        let adr_exists = |_: &str| true;
        let mut hygiene = Hygiene::new(&policy, &adr_exists);

        // when
        let got: Vec<String> = super::super::testing::rendered(&hygiene.check(&facts))
            .into_iter()
            .filter(|d| d.starts_with("ARCH-nightly-features"))
            .collect();

        // then
        let lines: Vec<&str> = got
            .iter()
            .map(|d| d.rsplit("lib.rs:").next().unwrap_or_default())
            .collect();
        assert_eq!(lines, ["1)", "2)", "3)", "4)", "4)"], "{got:?}");
    }

    #[test]
    fn should_report_metavariable_attribute_when_an_exception_row_tries_to_approve_it() {
        // given: a level named by a metavariable, directly and in `cfg_attr`, an exception row
        // that tries to approve it, and the forwarding of a whole `meta`, which passes
        let source = "macro_rules! a { ($l:ident) => { #[$l(clippy::all)] fn f() {} }; }\nmacro_rules! b { ($l:ident) => { #[cfg_attr(all(), $l(unused))] fn g() {} }; }\nmacro_rules! c { ($(#[$m:meta])*) => { $(#[$m])* fn h() {} }; }\nmacro_rules! d { ($c:meta) => { #[cfg_attr($c, allow(dead_code))] fn i() {} }; }\n";
        let facts = super::super::testing::facts_for("mayfly-domain", &[("src/lib.rs", source)]);
        let mut policy = super::super::testing::policy();
        policy.exceptions.lints.push(LintException {
            path: "crates/mayfly-domain/src/lib.rs".to_owned(),
            lint: "$ l (clippy :: all)".to_owned(),
            adr: "0001".to_owned(),
        });
        let adr_exists = |_: &str| true;
        let mut hygiene = Hygiene::new(&policy, &adr_exists);

        // when
        let got: Vec<String> = super::super::testing::rendered(&hygiene.check(&facts))
            .into_iter()
            .filter(|d| d.starts_with("ARCH-lint-suppression"))
            .collect();

        // then
        assert_eq!(got.len(), 3, "{got:?}");
        assert!(got[0].ends_with("lib.rs:1)"), "{got:?}");
        assert!(got[1].ends_with("lib.rs:2)"), "{got:?}");
        assert!(
            got[2].ends_with("lib.rs:4)") && got[2].contains("allow(dead_code)"),
            "{got:?}"
        );
        assert_eq!(hygiene.finish().len(), 1, "the row approves nothing");
    }

    #[test]
    fn should_report_macro_export_when_production_crate_exports_without_exception() {
        // given
        let source = "#[macro_export]\nmacro_rules! spawn { () => { std::process::Command::new(\"sh\") }; }\n#[cfg_attr(all(), macro_export)]\nmacro_rules! other { () => {}; }\n";
        let composition =
            super::super::testing::facts_for("mayfly-composition", &[("src/lib.rs", source)]);
        let excepted =
            super::super::testing::facts_for("mayfly-control-domain", &[("src/lib.rs", source)]);
        let tool = super::super::testing::facts_for("xtask", &[("src/lib.rs", source)]);
        let mut policy = super::super::testing::policy();
        policy.exceptions.lints.push(LintException {
            path: "crates/mayfly-control-domain/src/lib.rs".to_owned(),
            lint: MACRO_EXPORT.to_owned(),
            adr: "0001".to_owned(),
        });
        let adr_exists = |adr: &str| adr == "0001";
        let mut hygiene = Hygiene::new(&policy, &adr_exists);

        // when
        let got: Vec<String> = [&composition, &excepted, &tool]
            .into_iter()
            .flat_map(|facts| super::super::testing::rendered(&hygiene.check(facts)))
            .filter(|d| d.starts_with("ARCH-macro-export"))
            .collect();

        // then
        assert_eq!(got.len(), 2, "{got:?}");
        assert!(
            got[0].ends_with("crates/mayfly-composition/src/lib.rs:1)"),
            "{got:?}"
        );
        assert!(
            got[1].ends_with("crates/mayfly-composition/src/lib.rs:3)"),
            "{got:?}"
        );
        assert!(hygiene.finish().is_empty(), "{:?}", hygiene.finish());
    }

    #[test]
    fn should_report_sleep_when_exception_is_missing_or_not_for_that_file() {
        // given
        let source = "pub fn wait() { std::thread::sleep(std::time::Duration::from_millis(1)); }\n";
        let files = [("src/lib.rs", "pub mod wait;\n"), ("src/wait.rs", source)];
        let fake = super::super::testing::facts_for("mayfly-fake-tart", &files);
        let other = super::super::testing::facts_for("mayfly-fake-github", &files);
        let production = super::super::testing::facts_for("mayfly-control", &files);
        let mut policy = super::super::testing::policy();
        policy.exceptions.lints.push(LintException {
            path: "crates/mayfly-fake-tart/src/wait.rs".to_owned(),
            lint: TEST_SLEEP.to_owned(),
            adr: "0001".to_owned(),
        });
        let adr_exists = |adr: &str| adr == "0001";
        let mut hygiene = Hygiene::new(&policy, &adr_exists);

        // when
        let got: Vec<String> = [&fake, &other, &production]
            .into_iter()
            .flat_map(|facts| super::super::testing::rendered(&hygiene.check(facts)))
            .filter(|d| d.starts_with("ARCH-test-sleep"))
            .collect();

        // then
        assert_eq!(got.len(), 1, "{got:?}");
        assert!(got[0].contains("mayfly-fake-github"), "{got:?}");
        assert!(hygiene.finish().is_empty(), "{:?}", hygiene.finish());
    }

    #[test]
    fn should_report_unused_exception_when_the_excepted_file_does_not_sleep() {
        // given
        let fake = super::super::testing::facts_for(
            "mayfly-fake-tart",
            &[("src/lib.rs", "pub fn quiet() {}\n")],
        );
        let mut policy = super::super::testing::policy();
        policy.exceptions.lints.push(LintException {
            path: "crates/mayfly-fake-tart/src/lib.rs".to_owned(),
            lint: TEST_SLEEP.to_owned(),
            adr: "0001".to_owned(),
        });
        let adr_exists = |adr: &str| adr == "0001";
        let mut hygiene = Hygiene::new(&policy, &adr_exists);

        // when
        let checked = hygiene.check(&fake);
        let unused = super::super::testing::rendered(&hygiene.finish());

        // then
        assert!(checked.is_empty());
        assert_eq!(unused.len(), 1, "{unused:?}");
        assert!(unused[0].contains("matches no suppression"), "{unused:?}");
    }

    #[test]
    fn should_accept_name_when_it_has_outcome_and_condition() {
        // given
        let names = [
            ("should_pass_when_repository_is_checked", true),
            ("should_x_when_y", true),
            ("should_when_y", false),
            ("should_x_when_", false),
            ("it_works", false),
            ("should_Pass_when_y", false),
        ];

        // when
        let got: Vec<(&str, bool)> = names.iter().map(|(n, _)| (*n, well_named(n))).collect();

        // then
        assert_eq!(got, names);
    }

    #[test]
    fn should_require_markers_in_order_when_body_is_checked() {
        // given
        let good = ["{", "    // given", "    // when", "    // then: done", "}"];
        let reversed = ["    // when", "    // given", "    // then"];
        let missing = ["    // given", "    // when / then"];

        // when
        let results = [
            given_when_then(&good),
            given_when_then(&reversed),
            given_when_then(&missing),
        ];

        // then
        assert_eq!(
            results,
            [
                Ok(()),
                Err("`// when`".to_owned()),
                Err("`// then`".to_owned())
            ]
        );
    }
}
