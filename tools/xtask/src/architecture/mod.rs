//! `cargo xtask architecture`: the versioned dependency policy (foundation.md section 3.2)
//! checked against `cargo metadata` and the source tree, the semantic rules of section 3.3
//! checked against the parsed sources (`semantic`), plus the negative fixtures of section
//! 3.4.
//!
//! Without `--manifest-path` it checks this repository, confirms that every fixture under
//! [`FIXTURES_DIR`] fails with the rule its `expected.txt` names, that every fixture under
//! [`VALID_FIXTURES_DIR`] passes, and that every cataloged rule has a negative fixture. With
//! it, it checks only that workspace, whose root must hold `architecture.toml` and a
//! `Cargo.lock`.

mod external;
mod fixtures;
mod graph;
mod inventory;
mod policy;
mod report;
mod rules;
mod semantic;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::thread;

use cargo_metadata::{Metadata, MetadataCommand};

pub use rules::Rule;

use inventory::Crate;
use report::Diagnostic;

/// The policy file at the workspace root.
pub const POLICY_FILE: &str = "architecture.toml";

/// Targets whose dependency edges are evaluated, each through `cargo metadata
/// --filter-platform` (D2): the product target and the two Linux targets every crate builds
/// and tests on. Windows is not evaluated.
pub const TARGETS: [&str; 3] = [
    "aarch64-apple-darwin",
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
];

/// Negative fixtures, relative to the repository root: one mini workspace per directory.
pub const FIXTURES_DIR: &str = "tools/xtask/tests/architecture-invalid";

/// Positive fixtures, relative to the repository root: each must pass.
pub const VALID_FIXTURES_DIR: &str = "tools/xtask/tests/architecture-valid";

/// The result of checking one workspace.
#[derive(Debug)]
pub struct Outcome {
    /// Violations, sorted.
    pub diagnostics: Vec<Diagnostic>,
    /// Number of workspace crates checked.
    pub crates: usize,
}

/// Runs the command and returns the exit code: 0 clean, 1 on any violation.
pub fn run(manifest_path: Option<&Path>) -> i32 {
    let (manifest, with_fixtures) = match manifest_path {
        Some(path) => (path.to_path_buf(), false),
        None => (repository_manifest(), true),
    };
    let outcome = check(&manifest);
    println!(
        "architecture: targets checked: {} (Windows not evaluated, D2)",
        TARGETS.join(", ")
    );
    for diagnostic in &outcome.diagnostics {
        eprintln!("{diagnostic}");
    }
    let mut failed = !outcome.diagnostics.is_empty();
    if failed {
        eprintln!(
            "xtask: architecture: {} violation(s)",
            outcome.diagnostics.len()
        );
    } else {
        println!(
            "architecture: ok: {} crates match {POLICY_FILE}",
            outcome.crates
        );
    }
    if with_fixtures {
        let root = workspace_root(&manifest);
        let results = [
            fixtures::verify(&root.join(FIXTURES_DIR))
                .map(|n| format!("{n} negative fixtures fail as expected")),
            fixtures::verify_valid(&root.join(VALID_FIXTURES_DIR))
                .map(|n| format!("{n} positive fixtures pass")),
            match rules::completeness_problems(&root.join(FIXTURES_DIR)) {
                problems if problems.is_empty() => Ok(format!(
                    "{} rules each have a negative fixture",
                    rules::Rule::ALL.len()
                )),
                problems => Err(problems),
            },
        ];
        for result in results {
            match result {
                Ok(line) => println!("architecture: {line}"),
                Err(errors) => {
                    for error in errors {
                        eprintln!("xtask: architecture: {error}");
                    }
                    failed = true;
                }
            }
        }
    }
    i32::from(failed)
}

/// Checks the workspace whose root manifest is `manifest`.
pub fn check(manifest: &Path) -> Outcome {
    // Cargo runs in the workspace root, so a relative manifest path would resolve twice.
    let manifest = std::path::absolute(manifest).unwrap_or_else(|_| manifest.to_path_buf());
    let mut diagnostics = Vec::new();
    let crates = check_into(&manifest, &mut diagnostics);
    diagnostics.sort();
    diagnostics.dedup();
    Outcome {
        diagnostics,
        crates,
    }
}

fn check_into(manifest: &Path, out: &mut Vec<Diagnostic>) -> usize {
    let root = workspace_root(manifest);
    let policy = match fs::read_to_string(root.join(POLICY_FILE)) {
        Ok(text) => match policy::parse(&text) {
            Ok(policy) => policy,
            Err(err) => {
                out.push(Diagnostic::new(Rule::PolicySchema, err, POLICY_FILE));
                return 0;
            }
        },
        Err(err) => {
            out.push(Diagnostic::new(
                Rule::PolicySchema,
                format!("cannot read: {err}"),
                POLICY_FILE,
            ));
            return 0;
        }
    };
    out.extend(inventory::check_categories(&policy));
    match inventory::check_members_on_disk(&root) {
        Ok(missing) if missing.is_empty() => {}
        Ok(missing) => {
            out.extend(missing);
            return 0;
        }
        Err(diagnostic) => {
            out.push(diagnostic);
            return 0;
        }
    }
    let loaded: Vec<(&str, Result<Metadata, Diagnostic>)> = thread::scope(|scope| {
        let handles: Vec<_> = TARGETS
            .iter()
            .map(|&target| {
                let root = &root;
                (
                    target,
                    scope.spawn(move || load_metadata(manifest, root, target)),
                )
            })
            .collect();
        handles
            .into_iter()
            .map(|(target, handle)| {
                let result = handle.join().unwrap_or_else(|_| {
                    Err(Diagnostic::new(
                        Rule::Metadata,
                        "cargo metadata thread panicked",
                        "Cargo.toml",
                    ))
                });
                (target, result)
            })
            .collect()
    });
    let mut per_target = Vec::new();
    for (target, result) in loaded {
        match result {
            Ok(metadata) => per_target.push((target, metadata)),
            Err(diagnostic) => out.push(diagnostic),
        }
    }
    if per_target.len() < TARGETS.len() {
        return 0;
    }
    let members = workspace_crates(&per_target[0].1);
    match inventory::scan_source_tree(&root) {
        Ok(disk) => out.extend(inventory::check_crates(&policy, &members, &disk)),
        Err(diagnostic) => out.push(diagnostic),
    }
    out.extend(inventory::check_fuzz(&policy, &root, &members));
    let manifests: BTreeMap<String, String> = members
        .iter()
        .map(|c| (c.name.clone(), c.manifest.clone()))
        .collect();
    let adr_exists = |adr: &str| adr_exists(&root, adr);
    let mut found: BTreeMap<Diagnostic, BTreeSet<&str>> = BTreeMap::new();
    for (target, metadata) in &per_target {
        let graph = graph::Graph::from_metadata(metadata);
        let mut diagnostics = graph::check(&policy, &graph, &manifests);
        diagnostics.extend(external::check_edges(
            &policy,
            &external::edges(metadata),
            &manifests,
        ));
        diagnostics.extend(external::check_sources(
            &policy,
            &external::origins(metadata, &graph),
            &manifests,
            &adr_exists,
        ));
        for diagnostic in diagnostics {
            found.entry(diagnostic).or_default().insert(target);
        }
    }
    out.extend(semantic::check(&per_target[0].1, &policy, &adr_exists));
    for (mut diagnostic, targets) in found {
        if targets.len() < TARGETS.len() {
            let names: Vec<&str> = targets.into_iter().collect();
            diagnostic.message = format!("{} [targets: {}]", diagnostic.message, names.join(", "));
        }
        out.push(diagnostic);
    }
    members.len()
}

/// Whether `doc/adr/<adr>-*.md` exists under `root`.
fn adr_exists(root: &Path, adr: &str) -> bool {
    let prefix = format!("{adr}-");
    adr.len() == 4
        && adr.bytes().all(|b| b.is_ascii_digit())
        && fs::read_dir(root.join("doc/adr")).is_ok_and(|entries| {
            entries.filter_map(Result::ok).any(|e| {
                e.file_name()
                    .to_str()
                    .is_some_and(|n| n.starts_with(&prefix) && n.ends_with(".md"))
            })
        })
}

fn load_metadata(manifest: &Path, root: &Path, target: &str) -> Result<Metadata, Diagnostic> {
    MetadataCommand::new()
        .manifest_path(manifest)
        .current_dir(root)
        .other_options(vec![
            "--locked".to_owned(),
            "--filter-platform".to_owned(),
            target.to_owned(),
        ])
        .exec()
        .map_err(|err| {
            Diagnostic::new(
                Rule::Metadata,
                format!("cargo metadata failed: {}", scrub(&err.to_string(), root)),
                "Cargo.toml",
            )
        })
}

/// Workspace members with manifests relative to the workspace root.
fn workspace_crates(metadata: &Metadata) -> Vec<Crate> {
    metadata
        .workspace_packages()
        .into_iter()
        .map(|package| Crate {
            name: package.name.to_string(),
            manifest: relative(&metadata.workspace_root, &package.manifest_path),
        })
        .collect()
}

fn relative(
    root: &cargo_metadata::camino::Utf8Path,
    path: &cargo_metadata::camino::Utf8Path,
) -> String {
    path.strip_prefix(root)
        .map_or_else(|_| path.to_string(), |p| p.as_str().replace('\\', "/"))
}

/// Puts Cargo's message on one line and replaces the absolute workspace root in it, so
/// output stays one diagnostic per line and path-relative.
fn scrub(message: &str, root: &Path) -> String {
    let mut out = message
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" | ");
    for form in [fs::canonicalize(root).ok(), Some(root.to_path_buf())]
        .into_iter()
        .flatten()
    {
        let form = form.to_string_lossy().into_owned();
        if form.len() > 1 {
            out = out.replace(&form, ".");
        }
    }
    out
}

fn workspace_root(manifest: &Path) -> PathBuf {
    match manifest.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

/// The root of this repository, two levels above the xtask crate.
fn repository_root() -> PathBuf {
    let xtask = Path::new(env!("CARGO_MANIFEST_DIR"));
    xtask.ancestors().nth(2).unwrap_or(xtask).to_path_buf()
}

/// The root manifest of this repository.
fn repository_manifest() -> PathBuf {
    repository_root().join("Cargo.toml")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_join_lines_and_replace_root_when_cargo_message_names_it() {
        // given
        let root = Path::new("/somewhere/ws");
        let message =
            "error: bad\n  failed to load manifest at /somewhere/ws/crates/a/Cargo.toml\n";

        // when
        let scrubbed = scrub(message, root);

        // then
        assert_eq!(
            scrubbed,
            "error: bad | failed to load manifest at ./crates/a/Cargo.toml"
        );
    }

    #[test]
    fn should_use_current_directory_when_manifest_has_no_parent() {
        // given
        let manifest = Path::new("Cargo.toml");

        // when
        let root = workspace_root(manifest);

        // then
        assert_eq!(root, PathBuf::from("."));
    }

    #[test]
    fn should_pass_when_repository_is_checked() {
        // given
        let manifest = repository_manifest();

        // when
        let outcome = check(&manifest);

        // then
        assert_eq!(outcome.diagnostics, []);
        assert!(outcome.crates >= 21, "{}", outcome.crates);
    }
}
