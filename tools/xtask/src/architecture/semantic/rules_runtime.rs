//! Runtime discipline (foundation.md sections 3.3 and 4.3–4.4): libraries return typed errors,
//! queues are bounded, and tasks are spawned on a tracker handed down from the supervision
//! tree, never detached.
//!
//! All three rules read production code only: a test may build an unbounded channel to drive
//! a fake, or `anyhow` to shorten setup.

use super::super::policy::Policy;
use super::super::report::{Diagnostic, Rule};
use super::{Context, Facts, first_hit};

/// `anyhow` and its macros: an application's catch-all, never a library's error type.
const ANYHOW: &[&[&str]] = &[&["anyhow"]];

/// Constructors and types of unbounded channels and queues: backpressure is lost, so a slow
/// consumer grows memory without limit (plan section 3.3: the link is bounded).
const UNBOUNDED: &[&[&str]] = &[
    &["tokio", "sync", "mpsc", "unbounded_channel"],
    &["tokio", "sync", "mpsc", "UnboundedSender"],
    &["tokio", "sync", "mpsc", "UnboundedReceiver"],
    &["tokio", "sync", "mpsc", "WeakUnboundedSender"],
    &["std", "sync", "mpsc", "channel"],
    &["crossbeam_channel", "unbounded"],
    &["crossbeam", "channel", "unbounded"],
    &["flume", "unbounded"],
    &["async_channel", "unbounded"],
    &["futures", "channel", "mpsc", "unbounded"],
    &["futures_channel", "mpsc", "unbounded"],
    &["crossbeam_queue", "SegQueue"],
];

/// Free functions that start a task or thread nobody owns: it outlives its caller, is not
/// cancelled on shutdown and its panic is lost. `TaskTracker::spawn` and `JoinSet::spawn`
/// are method calls on an owner and pass.
const DETACHED: &[&[&str]] = &[
    &["tokio", "spawn"],
    &["tokio", "task", "spawn"],
    &["tokio", "task", "spawn_blocking"],
    &["tokio", "task", "spawn_local"],
    &["tokio", "task", "block_in_place"],
    &["std", "thread", "spawn"],
    &["std", "thread", "Builder"],
    &["async_std", "task", "spawn"],
    &["smol", "spawn"],
];

/// The modules that own long-lived tasks, by package and module path prefix: composition's
/// supervision tree, the SQLite writer thread, the link transports and the process runners
/// (foundation.md section 3.3), and the secrets adapter's runner. Everywhere else a task is spawned on a tracker handed down.
pub const SPAWN_MODULES: &[(&str, &[&str])] = &[
    ("mayfly-composition", &["supervision"]),
    ("mayfly-adapter-sqlite", &["writer"]),
    ("mayfly-adapter-link", &["transport"]),
    ("mayfly-adapter-tart", &["process"]),
    // The secrets adapter's blocking hand-off for its file and executable checks (ADR 0058).
    ("mayfly-adapter-secrets", &["command"]),
];

/// The diagnostics of these rules for one package.
pub fn check(facts: &Facts, policy: &Policy) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    let production = policy
        .workspace
        .production
        .as_ref()
        .is_some_and(|p| p.contains(&facts.package));
    let spawn_modules: Vec<&[&str]> = SPAWN_MODULES
        .iter()
        .filter(|(package, _)| *package == facts.package)
        .map(|(_, module)| *module)
        .collect();
    for reference in &facts.references {
        if reference.site.context != Context::Production {
            continue;
        }
        if reference.library
            && let Some(hit) = first_hit(reference, ANYHOW)
        {
            out.push(Diagnostic::new(
                Rule::AnyhowInLibrary,
                format!(
                    "{} names `{hit}` in its library: a library returns a typed error (thiserror); anyhow is for binaries (foundation.md section 4.3)",
                    facts.package
                ),
                reference.site.at(),
            ));
        }
        if !production {
            continue;
        }
        if let Some(hit) = first_hit(reference, UNBOUNDED) {
            out.push(Diagnostic::new(
                Rule::UnboundedChannel,
                format!(
                    "{} names `{hit}`: every queue is bounded, so backpressure reaches the producer (foundation.md section 4.4)",
                    facts.package
                ),
                reference.site.at(),
            ));
        }
        let owner = spawn_modules.iter().any(|module| {
            reference
                .module
                .starts_with(&module.iter().map(|m| (*m).to_owned()).collect::<Vec<_>>())
        });
        if !owner && let Some(hit) = first_hit(reference, DETACHED) {
            out.push(Diagnostic::new(
                Rule::DetachedSpawn,
                format!(
                    "{} names `{hit}` in module `{}`: spawn on the task tracker handed down from the supervision tree; detached tasks are allowed only in {}",
                    facts.package,
                    module_path(&reference.module),
                    SPAWN_MODULES
                        .iter()
                        .map(|(p, m)| format!("{p}::{}", m.join("::")))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                reference.site.at(),
            ));
        }
    }
    out
}

fn module_path(module: &[String]) -> String {
    if module.is_empty() {
        "crate".to_owned()
    } else {
        format!("crate::{}", module.join("::"))
    }
}

#[cfg(test)]
mod tests {
    use super::super::testing::{facts_for, policy, rendered};
    use super::*;

    fn rules(package: &str, files: &[(&str, &str)], id: &str) -> Vec<String> {
        rendered(&check(&facts_for(package, files), &policy()))
            .into_iter()
            .filter(|d| d.starts_with(id))
            .collect()
    }

    #[test]
    fn should_report_anyhow_when_a_library_names_it_outside_tests() {
        // given
        let source = "pub fn f() -> anyhow::Result<()> {\n    Ok(())\n}\n#[cfg(test)]\nmod tests {\n    fn g() -> anyhow::Result<()> { Ok(()) }\n}\n";

        // when
        let got = rules(
            "mayfly-data",
            &[("src/lib.rs", source)],
            "ARCH-anyhow-in-library",
        );

        // then
        assert_eq!(got.len(), 1, "{got:?}");
        assert!(got[0].ends_with("src/lib.rs:1)"), "{got:?}");
    }

    #[test]
    fn should_report_unbounded_channels_when_production_code_builds_them() {
        // given
        let source = "use std::sync::mpsc;\npub fn f() {\n    let _ = mpsc::channel::<u8>();\n    let _ = mpsc::sync_channel::<u8>(1);\n    let _ = tokio::sync::mpsc::unbounded_channel::<u8>();\n    let _ = tokio::sync::mpsc::channel::<u8>(8);\n}\n";

        // when
        let got = rules(
            "mayfly-control",
            &[("src/lib.rs", source)],
            "ARCH-unbounded-channel",
        );

        // then
        let lines: Vec<&str> = got
            .iter()
            .map(|d| d.rsplit(':').next().unwrap_or(""))
            .collect();
        assert_eq!(lines, ["3)", "5)"], "{got:?}");
    }

    #[test]
    fn should_report_detached_spawn_when_outside_the_listed_modules() {
        // given
        let files = [
            (
                "src/lib.rs",
                "mod supervision;\npub fn f() {\n    tokio::spawn(async {});\n    let _ = std::thread::spawn(|| ());\n}\n",
            ),
            (
                "src/supervision.rs",
                "pub fn run() {\n    tokio::task::spawn_blocking(|| ());\n}\n",
            ),
        ];

        // when
        let composition = rules("mayfly-composition", &files, "ARCH-detached-spawn");
        let testkit = rules("mayfly-testkit", &files, "ARCH-detached-spawn");

        // then
        let at: Vec<String> = composition
            .iter()
            .map(|d| d.rsplit(" (at ").next().unwrap_or("").to_owned())
            .collect();
        assert_eq!(
            at,
            [
                "crates/mayfly-composition/src/lib.rs:3)",
                "crates/mayfly-composition/src/lib.rs:4)"
            ],
            "{composition:?}"
        );
        assert!(testkit.is_empty(), "{testkit:?}");
    }
}
