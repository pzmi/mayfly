//! Hygiene and test quality (foundation.md sections 3.3 and 5.2; doc/tasks.md rules 4 and
//! 5): no unfinished code in production, no ignored tests, no unapproved lint suppression,
//! no sleeps in tests outside the bounded poll helper, named, structured tests, and no
//! `cfg!` platform branches in `crates/*/src`.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use super::super::POLICY_FILE;
use super::super::policy::{LintException, Policy};
use super::super::report::{Diagnostic, Rule};
use super::{Context, Facts, RefKind, Site, first_hit};

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
/// Package and file inside it.
pub const POLL_HELPER: (&str, &str) = ("mayfly-testkit", "src/poll.rs");

/// Whether the poll helper's file must exist (`ARCH-missing-selector`). Off until FND-05
/// writes `mayfly-testkit::poll` and sets it; until then no test code may sleep at all.
pub const POLL_HELPER_REQUIRED: bool = false;

/// The hygiene rules over all packages: suppressions are matched against
/// `[[exceptions.lints]]`, and entries that match nothing are reported by [`Hygiene::finish`].
pub struct Hygiene<'a> {
    policy: &'a Policy,
    adr_exists: &'a dyn Fn(&str) -> bool,
    used: BTreeSet<usize>,
}

impl<'a> Hygiene<'a> {
    /// Rules for `policy`; `adr_exists` checks an exception's ADR.
    pub fn new(policy: &'a Policy, adr_exists: &'a dyn Fn(&str) -> bool) -> Self {
        Self {
            policy,
            adr_exists,
            used: BTreeSet::new(),
        }
    }

    /// The diagnostics for one package.
    pub fn check(&mut self, facts: &Facts) -> Vec<Diagnostic> {
        let mut out = Vec::new();
        self.unfinished(facts, &mut out);
        self.suppressions(facts, &mut out);
        sleeps(facts, self.policy, &mut out);
        tests(facts, &mut out);
        platform_branches(facts, &mut out);
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
        let mut dirs: Vec<String> = vec![String::new()];
        dirs.extend(
            manifests
                .iter()
                .filter_map(|m| m.rsplit_once('/').map(|(dir, _)| format!("{dir}/"))),
        );
        for dir in &dirs {
            for name in [".cargo/config.toml", ".cargo/config"] {
                let rel = format!("{dir}{name}");
                if root.join(&rel).is_file()
                    && let Some(config) = read_toml(root, &rel, &mut out)
                {
                    rustflags(&config, &rel, &mut out);
                }
            }
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
        out
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

/// Reports rustflags in a Cargo config that allow or cap lints.
fn rustflags(config: &toml::Table, rel: &str, out: &mut Vec<Diagnostic>) {
    let mut lists: Vec<&toml::Value> = Vec::new();
    lists.extend(config.get("build").and_then(|b| b.get("rustflags")));
    for target in config
        .get("target")
        .and_then(toml::Value::as_table)
        .into_iter()
        .flat_map(|t| t.values())
    {
        lists.extend(target.get("rustflags"));
    }
    lists.extend(config.get("env").and_then(|e| e.get("RUSTFLAGS")));
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
            }
        }
    }
}

fn sleeps(facts: &Facts, policy: &Policy, out: &mut Vec<Diagnostic>) {
    let test_crate = policy
        .workspace
        .test_only
        .as_ref()
        .is_some_and(|t| t.contains(&facts.package));
    for reference in &facts.references {
        let test = reference.site.context == Context::Test || test_crate;
        let poll_helper = facts.package == POLL_HELPER.0
            && facts.in_package(&reference.site) == POLL_HELPER.1
            && reference.site.context == Context::Production;
        if !test || poll_helper {
            continue;
        }
        if let Some(candidate) = first_hit(reference, SLEEPS) {
            out.push(Diagnostic::new(
                Rule::TestSleep,
                format!(
                    "{} test code calls `{}`: wait on an event, an injected clock or `{}::poll::wait_until` instead",
                    facts.package,
                    candidate,
                    POLL_HELPER.0.replace('-', "_")
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

/// Whether `name` matches `^should_[a-z0-9_]+_when_[a-z0-9_]+$`.
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
