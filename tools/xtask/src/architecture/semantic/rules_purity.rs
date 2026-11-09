//! Purity of the pure crates, process authority, plane vocabulary, layer isolation, and
//! `unsafe` (foundation.md sections 2.3 and 3.3; plan sections 3.4 and 4).
//!
//! The crates each rule selects come from the policy (`[purity]`, `[planes]`). Purity and
//! process authority apply to production code (see [`Context`]): tests may spawn fakes and
//! build temporary state. Layer isolation and plane vocabulary apply to test code too, so a
//! data-plane test cannot name the SQLite adapter. `unsafe` is rejected everywhere.

use super::super::policy::Policy;
use super::super::report::{Diagnostic, Rule};
use super::{Context, Facts, Reference, first_hit};

/// The application crates of the two planes: their decisions are checked by
/// `ARCH-observability-isolation` like the pure crates'.
pub const APPLICATIONS: &[&str] = &["mayfly-control", "mayfly-data"];

/// What a pure crate must not reach: ambient I/O, processes, environment, threads, the clock,
/// randomness (a std `HashMap`/`HashSet` iterates in a per-process random order, so a
/// `decide` over one is not deterministic; `BTreeMap`/`BTreeSet` are the deterministic
/// choice), a backtrace (it reads `RUST_BACKTRACE`), global mutable state (atomics, locks,
/// lazy statics, `thread_local!`), async runtimes and executors, HTTP, SQL, console output and
/// the outer layers. Bare names are prelude macros (`println!`, `env!`).
const PURE_FORBIDDEN: &[&[&str]] = &[
    &["std", "fs"],
    &["std", "net"],
    &["std", "process"],
    &["std", "env"],
    &["std", "thread"],
    &["std", "os"],
    &["std", "io", "stdin"],
    &["std", "io", "stdout"],
    &["std", "io", "stderr"],
    &["std", "time", "SystemTime", "now"],
    &["std", "time", "SystemTime", "elapsed"],
    &["std", "time", "Instant", "now"],
    &["std", "time", "Instant", "elapsed"],
    &["std", "time", "UNIX_EPOCH"],
    &["std", "time", "SystemTime", "UNIX_EPOCH"],
    &["std", "collections", "hash_map", "RandomState"],
    &["std", "hash", "RandomState"],
    &["std", "collections", "HashMap"],
    &["std", "collections", "HashSet"],
    &["std", "collections", "hash_map"],
    &["std", "collections", "hash_set"],
    &["hashbrown"],
    &["std", "backtrace"],
    &["std", "sync", "atomic"],
    &["core", "sync", "atomic"],
    &["std", "sync", "Mutex"],
    &["std", "sync", "RwLock"],
    &["std", "sync", "Condvar"],
    &["std", "sync", "Once"],
    &["std", "sync", "OnceLock"],
    &["std", "sync", "LazyLock"],
    &["std", "sync", "Barrier"],
    &["std", "sync", "mpsc"],
    &["thread_local"],
    &["std", "thread_local"],
    &["rustix"],
    &["nix"],
    &["futures", "executor"],
    &["futures_executor"],
    &["print"],
    &["println"],
    &["eprint"],
    &["eprintln"],
    &["env"],
    &["option_env"],
    &["std", "print"],
    &["std", "println"],
    &["std", "eprint"],
    &["std", "eprintln"],
    &["std", "option_env"],
    &["core", "env"],
    &["core", "option_env"],
    &["tokio"],
    &["async_std"],
    &["smol"],
    &["futures_timer"],
    &["chrono", "Utc", "now"],
    &["chrono", "Local", "now"],
    &["time", "OffsetDateTime", "now_utc"],
    &["time", "OffsetDateTime", "now_local"],
    &["jiff", "Timestamp", "now"],
    &["jiff", "Zoned", "now"],
    &["rand"],
    &["getrandom"],
    &["fastrand"],
    &["uuid", "Uuid", "new_v4"],
    &["uuid", "Uuid", "now_v1"],
    &["uuid", "Uuid", "now_v6"],
    &["uuid", "Uuid", "now_v7"],
    &["axum"],
    &["hyper"],
    &["http"],
    &["reqwest"],
    &["ureq"],
    &["rusqlite"],
    &["sqlx"],
    &["libsqlite3_sys"],
    &["diesel"],
    &["mayfly_control"],
    &["mayfly_data"],
    &["mayfly_adapter_sqlite"],
    &["mayfly_adapter_github"],
    &["mayfly_adapter_secrets"],
    &["mayfly_adapter_tart"],
    &["mayfly_adapter_guest"],
    &["mayfly_adapter_host"],
    &["mayfly_adapter_link"],
    &["mayfly_composition"],
    &["mayfly_cli"],
    &["mayfly_testkit"],
    &["mayfly_conformance"],
];

/// Method names a pure crate must not call: `elapsed` reads the clock whatever value it is
/// called on (`UNIX_EPOCH.elapsed()`, an `Instant` handed in). Method calls are matched by
/// name only.
const PURE_FORBIDDEN_METHODS: &[&str] = &["elapsed"];

/// Paths that spawn, signal or otherwise control processes.
const PROCESS_AUTHORITY: &[&[&str]] = &[
    &["std", "process"],
    &["std", "os", "unix", "process"],
    &["std", "os", "linux", "process"],
    &["tokio", "process"],
    &["async_process"],
    &["rustix", "process"],
    &["nix", "unistd"],
    &["nix", "sys", "signal"],
    &["duct"],
];

/// Items under [`PROCESS_AUTHORITY`] that grant none: a `main` may still return an exit code.
const PROCESS_HARMLESS: &[&[&str]] = &[
    &["std", "process", "ExitCode"],
    &["std", "process", "Termination"],
];

/// Crates each crate must not name (plan sections 3.4 and 4): the planes never name each
/// other's crates, adapters never name a sibling adapter, composition or the CLI, the
/// applications name no adapter technology, and the CLI names only composition. The rule runs
/// over production and test code; test code may still name a test-only crate (the CLI's e2e
/// tests use the testkit). The dependency graph forbids the edges themselves
/// (`ARCH-forbidden-edge`, and `ARCH-plane-separation` over dev closures); this rule catches a
/// name that arrives anyway, through another workspace crate's re-export included
/// ([`super::follow_reexports`]).
pub const ISOLATION: &[(&str, &[&str])] = &[
    (
        "mayfly-protocol",
        &["mayfly_control_domain", "mayfly_data_domain"],
    ),
    (
        "mayfly-control-domain",
        &["mayfly_data_domain", "mayfly_protocol"],
    ),
    ("mayfly-data-domain", &["mayfly_control_domain"]),
    (
        "mayfly-control",
        &[
            "mayfly_data_domain",
            "mayfly_data",
            "mayfly_adapter_sqlite",
            "mayfly_adapter_github",
            "mayfly_adapter_secrets",
            "mayfly_adapter_tart",
            "mayfly_adapter_guest",
            "mayfly_adapter_host",
            "mayfly_adapter_link",
            "mayfly_composition",
            "mayfly_cli",
            "rusqlite",
            "sqlx",
            "reqwest",
            "hyper",
        ],
    ),
    (
        "mayfly-data",
        &[
            "mayfly_control_domain",
            "mayfly_control",
            "mayfly_adapter_sqlite",
            "mayfly_adapter_github",
            "mayfly_adapter_secrets",
            "mayfly_adapter_tart",
            "mayfly_adapter_guest",
            "mayfly_adapter_host",
            "mayfly_adapter_link",
            "mayfly_composition",
            "mayfly_cli",
            "rusqlite",
            "sqlx",
        ],
    ),
    (
        "mayfly-adapter-sqlite",
        &[
            "mayfly_adapter_github",
            "mayfly_adapter_secrets",
            "mayfly_adapter_tart",
            "mayfly_adapter_guest",
            "mayfly_adapter_host",
            "mayfly_adapter_link",
            "mayfly_data_domain",
            "mayfly_data",
            "mayfly_composition",
            "mayfly_cli",
        ],
    ),
    (
        "mayfly-adapter-github",
        &[
            "mayfly_adapter_sqlite",
            "mayfly_adapter_secrets",
            "mayfly_adapter_tart",
            "mayfly_adapter_guest",
            "mayfly_adapter_host",
            "mayfly_adapter_link",
            "mayfly_data_domain",
            "mayfly_data",
            "mayfly_composition",
            "mayfly_cli",
        ],
    ),
    (
        "mayfly-adapter-secrets",
        &[
            "mayfly_adapter_sqlite",
            "mayfly_adapter_github",
            "mayfly_adapter_tart",
            "mayfly_adapter_guest",
            "mayfly_adapter_host",
            "mayfly_adapter_link",
            "mayfly_data_domain",
            "mayfly_data",
            "mayfly_composition",
            "mayfly_cli",
        ],
    ),
    (
        "mayfly-adapter-tart",
        &[
            "mayfly_adapter_guest",
            "mayfly_adapter_host",
            "mayfly_adapter_link",
            "mayfly_adapter_sqlite",
            "mayfly_adapter_github",
            "mayfly_adapter_secrets",
            "mayfly_control_domain",
            "mayfly_control",
            "mayfly_composition",
            "mayfly_cli",
            "rusqlite",
            "sqlx",
        ],
    ),
    (
        "mayfly-adapter-guest",
        &[
            "mayfly_adapter_tart",
            "mayfly_adapter_host",
            "mayfly_adapter_link",
            "mayfly_adapter_sqlite",
            "mayfly_adapter_github",
            "mayfly_adapter_secrets",
            "mayfly_control_domain",
            "mayfly_control",
            "mayfly_composition",
            "mayfly_cli",
            "rusqlite",
            "sqlx",
        ],
    ),
    (
        "mayfly-adapter-host",
        &[
            "mayfly_adapter_tart",
            "mayfly_adapter_guest",
            "mayfly_adapter_link",
            "mayfly_adapter_sqlite",
            "mayfly_adapter_github",
            "mayfly_adapter_secrets",
            "mayfly_control_domain",
            "mayfly_control",
            "mayfly_composition",
            "mayfly_cli",
            "rusqlite",
            "sqlx",
        ],
    ),
    (
        "mayfly-adapter-link",
        &[
            "mayfly_adapter_sqlite",
            "mayfly_adapter_github",
            "mayfly_adapter_secrets",
            "mayfly_adapter_tart",
            "mayfly_adapter_guest",
            "mayfly_adapter_host",
            "mayfly_control_domain",
            "mayfly_control",
            "mayfly_data_domain",
            "mayfly_data",
            "mayfly_composition",
            "mayfly_cli",
        ],
    ),
    ("mayfly-composition", &["mayfly_cli"]),
    (
        "mayfly-cli",
        &[
            "mayfly_domain",
            "mayfly_protocol",
            "mayfly_control_domain",
            "mayfly_control",
            "mayfly_data_domain",
            "mayfly_data",
            "mayfly_adapter_sqlite",
            "mayfly_adapter_github",
            "mayfly_adapter_secrets",
            "mayfly_adapter_tart",
            "mayfly_adapter_guest",
            "mayfly_adapter_host",
            "mayfly_adapter_link",
            "mayfly_testkit",
            "mayfly_conformance",
        ],
    ),
];

/// The diagnostics of these rules for one package.
pub fn check(facts: &Facts, policy: &Policy) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    let production = policy
        .workspace
        .production
        .as_ref()
        .is_some_and(|p| p.contains(&facts.package));
    let pure = policy.purity.pure.contains(&facts.package);
    let allowed = &policy.purity.process_authority;
    let process_checked = production && !pure && !allowed.contains(&facts.package);
    let test_only: Vec<String> = policy
        .workspace
        .test_only
        .iter()
        .flatten()
        .map(|c| c.replace('-', "_"))
        .collect();
    let isolation = ISOLATION
        .iter()
        .find(|(name, _)| *name == facts.package)
        .map(|(_, crates)| *crates);
    out.extend(vocabulary(facts, policy));
    for reference in &facts.references {
        if let Some(crates) = isolation {
            let crates: Vec<&str> = crates
                .iter()
                .copied()
                .filter(|c| {
                    reference.site.context == Context::Production
                        || !test_only.iter().any(|t| t == c)
                })
                .collect();
            let prefixes: Vec<[&str; 1]> = crates.iter().map(|c| [*c]).collect();
            let prefixes: Vec<&[&str]> = prefixes.iter().map(|p| p.as_slice()).collect();
            if let Some(hit) = first_hit(reference, &prefixes) {
                out.push(Diagnostic::new(
                    Rule::LayerIsolation,
                    format!(
                        "{} names `{}`{} in {} code: this crate must not reach {}",
                        facts.package,
                        hit,
                        written(reference, &hit),
                        context_word(reference.site.context),
                        crates.join(", "),
                    ),
                    reference.site.at(),
                ));
            }
        }
        if reference.site.context != Context::Production {
            continue;
        }
        if pure && let Some(hit) = first_hit(reference, PURE_FORBIDDEN) {
            out.push(Diagnostic::new(
                Rule::DomainPurity,
                format!(
                    "{} names `{}`{}: the pure crates have no I/O, process, environment, clock, randomness, runtime, HTTP, SQL, console or outer layer (plan section 4)",
                    facts.package,
                    hit,
                    written(reference, &hit)
                ),
                reference.site.at(),
            ));
        }
        if process_checked
            && let Some(hit) = first_hit(reference, PROCESS_AUTHORITY)
            && !harmless(reference)
        {
            out.push(Diagnostic::new(
                Rule::ProcessAuthority,
                format!(
                    "{} names `{}`{}: process authority is allowed only in [purity] process_authority ({}), test-only and tool-only crates (plan section 3.4)",
                    facts.package,
                    hit,
                    written(reference, &hit),
                    allowed.join(", ")
                ),
                reference.site.at(),
            ));
        }
    }
    if pure {
        for call in &facts.method_calls {
            if call.site.context == Context::Production
                && PURE_FORBIDDEN_METHODS.contains(&call.name.as_str())
            {
                out.push(Diagnostic::new(
                    Rule::DomainPurity,
                    format!(
                        "{} calls `.{}()`: it reads the clock; time arrives as a value (plan section 5.1)",
                        facts.package, call.name
                    ),
                    call.site.at(),
                ));
            }
        }
        for (site, mutable) in &facts.statics {
            if *mutable && site.context == Context::Production {
                out.push(Diagnostic::new(
                    Rule::DomainPurity,
                    format!(
                        "{} declares a `static mut`: the pure crates hold no global mutable state",
                        facts.package
                    ),
                    site.at(),
                ));
            }
        }
    }
    for (site, what) in &facts.unsafe_sites {
        out.push(Diagnostic::new(
            Rule::UnsafeCode,
            format!(
                "{} contains {what}; `unsafe` is forbidden in owned code (foundation.md section 3.3)",
                facts.package
            ),
            site.at(),
        ));
    }
    out
}

fn context_word(context: Context) -> &'static str {
    match context {
        Context::Production => "production",
        Context::Test => "test",
    }
}

/// `ARCH-plane-vocabulary`: a crate of a plane, or a shared crate, names a word another plane
/// owns (`[planes.vocabulary]`), as a segment of what it writes or of any candidate (a
/// shared crate's re-export of `Journal` still names `Journal`), in production and test code.
fn vocabulary(facts: &Facts, policy: &Policy) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    let Some(plane) = policy.planes.of(&facts.package) else {
        return out;
    };
    let words: Vec<(&str, &str)> = plane
        .forbidden()
        .iter()
        .flat_map(|owner| {
            policy
                .planes
                .vocabulary
                .of(*owner)
                .iter()
                .map(|w| (w.as_str(), owner.key()))
        })
        .collect();
    if words.is_empty() {
        return out;
    }
    for reference in &facts.references {
        let segments = reference
            .written
            .split("::")
            .map(str::to_owned)
            .chain(reference.resolved.iter().flatten().cloned());
        let mut hit = None;
        for segment in segments {
            if let Some(found) = words.iter().find(|(w, _)| *w == segment) {
                hit = Some(*found);
                break;
            }
        }
        if let Some((word, owner)) = hit {
            out.push(Diagnostic::new(
                Rule::PlaneVocabulary,
                format!(
                    "{} crate {} names `{word}` (written `{}`) in {} code: `{word}` belongs to the {owner} plane alone",
                    plane.key(),
                    facts.package,
                    reference.written,
                    context_word(reference.site.context),
                ),
                reference.site.at(),
            ));
        }
    }
    out
}

/// Whether every candidate of a closed path lies under [`PROCESS_HARMLESS`]: `ExitCode` grants
/// no authority, `std::process` itself does.
fn harmless(reference: &Reference) -> bool {
    !reference.open_tail
        && !reference.open_head
        && reference.resolved.iter().all(|candidate| {
            PROCESS_HARMLESS.iter().any(|prefix| {
                candidate.len() >= prefix.len()
                    && candidate.iter().zip(*prefix).all(|(a, b)| a == b)
            })
        })
}

/// ` (written `x`)` when the source spells the hit differently.
fn written(reference: &Reference, hit: &str) -> String {
    if reference.written == hit {
        String::new()
    } else if reference.open_tail {
        format!(" (written `{}::$..` in a macro)", reference.written)
    } else if reference.open_head {
        format!(" (written `$..::{}` in a macro)", reference.written)
    } else {
        format!(" (written `{}`)", reference.written)
    }
}

#[cfg(test)]
mod tests {
    use super::super::testing::{facts_for, policy, rendered};
    use super::super::{RefKind, Site};
    use super::*;

    const DOMAIN: &str = "mayfly-domain";
    const PURE: &[&str] = &[
        "mayfly-domain",
        "mayfly-protocol",
        "mayfly-control-domain",
        "mayfly-data-domain",
    ];

    fn reference(written: &str, resolved: &[&str], open: (bool, bool)) -> Reference {
        Reference {
            site: Site {
                file: "crates/x/src/lib.rs".to_owned(),
                line: 3,
                context: Context::Production,
            },
            written: written.to_owned(),
            resolved: resolved
                .iter()
                .map(|p| p.split("::").map(str::to_owned).collect())
                .collect(),
            open_tail: open.0,
            open_head: open.1,
            kind: RefKind::Path,
            module: Vec::new(),
            through: Vec::new(),
            library: true,
        }
    }

    /// Diagnostics of these rules for `package` with `files` (the first is `src/lib.rs`).
    fn diagnostics(package: &str, files: &[(&str, &str)]) -> Vec<String> {
        rendered(&check(&facts_for(package, files), &policy()))
    }

    #[test]
    fn should_hit_when_candidate_is_under_forbidden_prefix() {
        // given
        let aliased = reference("f::read", &["std::fs::read"], (false, false));
        let allowed = reference("Instant", &["std::time::Instant"], (false, false));

        // when
        let hit = first_hit(&aliased, PURE_FORBIDDEN);
        let miss = first_hit(&allowed, PURE_FORBIDDEN);

        // then
        assert_eq!(hit.as_deref(), Some("std::fs::read"));
        assert_eq!(miss, None);
    }

    #[test]
    fn should_hit_when_macro_path_may_expand_below_prefix() {
        // given
        let open = reference("std", &["std"], (true, false));
        let closed = reference("std", &["std"], (false, false));

        // when
        let hit = first_hit(&open, PURE_FORBIDDEN);
        let miss = first_hit(&closed, PURE_FORBIDDEN);

        // then
        assert_eq!(hit.as_deref(), Some("std::fs"));
        assert_eq!(miss, None);
    }

    #[test]
    fn should_hit_when_macro_head_is_a_metavariable() {
        // given
        let tail = reference("fs::read", &["fs::read"], (false, true));
        let harmless = reference("new", &["new"], (false, true));

        // when
        let hit = first_hit(&tail, PURE_FORBIDDEN);
        let miss = first_hit(&harmless, PURE_FORBIDDEN);

        // then
        assert_eq!(hit.as_deref(), Some("std::fs"));
        assert_eq!(miss, None);
    }

    #[test]
    fn should_mention_written_form_when_it_differs() {
        // given
        let aliased = reference("f::read", &["std::fs::read"], (false, false));

        // when
        let text = written(&aliased, "std::fs::read");

        // then
        assert_eq!(text, " (written `f::read`)");
    }

    #[test]
    fn should_report_domain_when_reexport_is_cross_module_or_test_gated() {
        // given
        let files = [
            (
                "src/lib.rs",
                "mod a;\nmod b;\npub fn load() {\n    let _ = crate::b::g::read(\"x\");\n}\n",
            ),
            ("src/a.rs", "#[cfg(test)]\npub use std::fs as f;\n"),
            ("src/b.rs", "pub(crate) use crate::a::f as g;\n"),
        ];

        // when
        let got = diagnostics(DOMAIN, &files);

        // then
        assert!(
            got.iter().any(|d| d.starts_with("ARCH-domain-purity")
                && d.contains("`std::fs::read` (written `crate::b::g::read`)")
                && d.ends_with("src/lib.rs:4)")),
            "{got:?}"
        );
    }

    #[test]
    fn should_report_domain_when_console_env_macros_or_random_state_are_used() {
        // given
        let files = [(
            "src/lib.rs",
            "use std::collections::hash_map::RandomState;\npub fn f() {\n    println!(\"x\");\n    let _ = env!(\"HOME\");\n}\n",
        )];

        // when
        let got = diagnostics(DOMAIN, &files);

        // then
        let lines: Vec<&str> = got
            .iter()
            .filter(|d| d.starts_with("ARCH-domain-purity"))
            .map(|d| d.rsplit(':').next().unwrap_or(""))
            .collect();
        assert_eq!(lines, ["1)", "3)", "4)"], "{got:?}");
    }

    #[test]
    fn should_hold_every_pure_crate_to_purity_when_it_names_the_clock_sql_or_randomness() {
        // given
        let source = "pub fn f() {\n    let _ = std::time::Instant::now();\n    let _ = rusqlite::Connection::open_in_memory();\n    let _ = uuid::Uuid::new_v4();\n    let _ = uuid::Uuid::parse_str(\"x\");\n}\n";

        // when
        let got: Vec<Vec<String>> = PURE
            .iter()
            .map(|package| {
                diagnostics(package, &[("src/lib.rs", source)])
                    .into_iter()
                    .filter(|d| d.starts_with("ARCH-domain-purity"))
                    .collect()
            })
            .collect();

        // then
        for (package, found) in PURE.iter().zip(&got) {
            let lines: Vec<&str> = found
                .iter()
                .map(|d| d.rsplit(':').next().unwrap_or(""))
                .collect();
            assert_eq!(lines, ["2)", "3)", "4)"], "{package}: {found:?}");
        }
    }

    #[test]
    fn should_report_domain_when_global_state_runtime_or_qualified_clock_is_named() {
        // given
        let source = "use std::sync::Mutex;\nstatic mut COUNT: u32 = 0;\npub fn f() {\n    let _ = <std::time::Instant as Clone>::clone;\n    let _ = <std::time::Instant>::now();\n    let _ = std::collections::HashSet::<u8>::new();\n    let _ = rustix::process::getpid();\n    let _ = nix::unistd::getpid();\n    futures::executor::block_on(async {});\n    let _ = std::sync::Arc::new(1);\n    let _ = std::collections::BTreeMap::<u8, u8>::new();\n}\n";

        // when
        let got = diagnostics(DOMAIN, &[("src/lib.rs", source)]);

        // then
        let mut lines: Vec<&str> = got
            .iter()
            .filter(|d| d.starts_with("ARCH-domain-purity"))
            .map(|d| d.rsplit(':').next().unwrap_or(""))
            .collect();
        lines.sort_unstable();
        lines.dedup();
        assert_eq!(lines, ["1)", "2)", "5)", "6)", "7)", "8)", "9)"], "{got:?}");
    }

    #[test]
    fn should_report_isolation_in_tests_when_a_test_names_another_plane_but_not_test_kits() {
        // given
        let source = "#[cfg(test)]\nmod tests {\n    use mayfly_control::Bus;\n    use mayfly_testkit_shared::poll;\n}\n";

        // when
        let data = diagnostics("mayfly-data", &[("src/lib.rs", source)]);
        let cli = diagnostics("mayfly-cli", &[("src/lib.rs", source)]);

        // then
        let isolation = |found: &[String]| -> Vec<String> {
            found
                .iter()
                .filter(|d| d.starts_with("ARCH-layer-isolation"))
                .cloned()
                .collect()
        };
        assert_eq!(isolation(&data).len(), 1, "{data:?}");
        assert!(isolation(&data)[0].contains("in test code"), "{data:?}");
        assert!(
            isolation(&cli)
                .iter()
                .all(|d| !d.contains("mayfly_testkit")),
            "{cli:?}"
        );
    }

    #[test]
    fn should_report_vocabulary_when_a_plane_names_the_other_planes_types() {
        // given
        let data = "pub fn f(_: &dyn mayfly_protocol::Journal) {}\n#[cfg(test)]\nmod tests {\n    use mayfly_testkit::AggregateStore;\n}\n";
        let control = "pub fn g<P: mayfly_domain::VmProvider>(_: P) {}\npub fn h(_: &dyn mayfly_control::Journal) {}\n";

        // when
        let in_data = diagnostics("mayfly-data", &[("src/lib.rs", data)]);
        let in_control = diagnostics("mayfly-control", &[("src/lib.rs", control)]);
        let in_shared = diagnostics("mayfly-protocol", &[("src/lib.rs", control)]);

        // then
        let count = |found: &[String]| {
            found
                .iter()
                .filter(|d| d.starts_with("ARCH-plane-vocabulary"))
                .count()
        };
        assert_eq!(count(&in_data), 2, "{in_data:?}");
        assert_eq!(count(&in_control), 1, "{in_control:?}");
        assert_eq!(count(&in_shared), 2, "{in_shared:?}");
    }

    #[test]
    fn should_report_process_authority_when_a_production_crate_outside_the_allowlist_spawns() {
        // given
        let source = "use std::process::ExitCode;\npub fn f() -> ExitCode {\n    let _ = std::process::Command::new(\"tart\");\n    let _ = tokio::process::Command::new(\"tart\");\n    ExitCode::SUCCESS\n}\n";

        // when
        let control = diagnostics("mayfly-control", &[("src/lib.rs", source)]);
        let tart = diagnostics("mayfly-adapter-tart", &[("src/lib.rs", source)]);
        let testkit = diagnostics("mayfly-testkit", &[("src/lib.rs", source)]);

        // then
        let lines: Vec<&str> = control
            .iter()
            .filter(|d| d.starts_with("ARCH-process-authority"))
            .map(|d| d.rsplit(':').next().unwrap_or(""))
            .collect();
        assert_eq!(lines, ["3)", "4)"], "{control:?}");
        assert!(
            tart.iter()
                .all(|d| !d.starts_with("ARCH-process-authority")),
            "{tart:?}"
        );
        assert!(
            testkit
                .iter()
                .all(|d| !d.starts_with("ARCH-process-authority")),
            "{testkit:?}"
        );
    }

    #[test]
    fn should_report_process_authority_when_std_process_itself_is_imported() {
        // given
        let source = "use std::process;\npub fn f() -> process::ExitCode {\n    process::ExitCode::SUCCESS\n}\n";

        // when
        let got = diagnostics("mayfly-cli", &[("src/lib.rs", source)]);

        // then
        let lines: Vec<&str> = got
            .iter()
            .filter(|d| d.starts_with("ARCH-process-authority"))
            .map(|d| d.rsplit(':').next().unwrap_or(""))
            .collect();
        assert_eq!(lines, ["1)"], "{got:?}");
    }

    #[test]
    fn should_report_isolation_when_a_crate_names_the_other_plane_or_a_sibling_adapter() {
        // given
        let cases = [
            ("mayfly-control", "pub fn f(_: mayfly_data::Executor) {}\n"),
            ("mayfly-data", "pub use mayfly_control::Journal;\n"),
            (
                "mayfly-data-domain",
                "pub use mayfly_control_domain::Attempt;\n",
            ),
            (
                "mayfly-control-domain",
                "pub use mayfly_data_domain::Directive;\n",
            ),
            (
                "mayfly-adapter-tart",
                "pub use mayfly_adapter_guest::Ssh;\n",
            ),
            (
                "mayfly-adapter-sqlite",
                "pub use mayfly_composition::Daemon;\n",
            ),
            ("mayfly-cli", "pub use mayfly_domain::VmName;\n"),
        ];

        // when
        let got: Vec<Vec<String>> = cases
            .iter()
            .map(|(package, source)| diagnostics(package, &[("src/lib.rs", source)]))
            .collect();

        // then
        for (case, found) in cases.iter().zip(&got) {
            assert!(
                found.iter().any(|d| d.starts_with("ARCH-layer-isolation")),
                "{case:?}: {found:?}"
            );
        }
    }

    #[test]
    fn should_allow_composition_when_it_names_both_planes_and_spawns() {
        // given
        let files = [(
            "src/lib.rs",
            "pub fn start() {\n    let _ = mayfly_control::Bus::new();\n    let _ = mayfly_data::Executor::new();\n    let _ = std::process::Command::new(\"tart\");\n}\n",
        )];

        // when
        let got = diagnostics("mayfly-composition", &files);

        // then
        assert_eq!(got, Vec::<String>::new());
    }

    #[test]
    fn should_report_each_kind_when_code_is_unsafe() {
        // given
        let files = [(
            "src/lib.rs",
            "pub unsafe fn a() {}\nstruct S;\nunsafe impl Send for S {}\nunsafe extern \"C\" {}\n#[unsafe(no_mangle)]\npub fn b() {}\nunsafe trait T {}\n",
        )];

        // when
        let got = diagnostics("mayfly-adapter-host", &files);

        // then
        let kinds: Vec<&str> = got
            .iter()
            .filter(|d| d.starts_with("ARCH-unsafe-code"))
            .filter_map(|d| d.split(" contains ").nth(1))
            .filter_map(|d| d.split(';').next())
            .collect();
        assert_eq!(
            kinds,
            [
                "an `unsafe fn`",
                "an `unsafe impl`",
                "an `extern` block",
                "an `#[unsafe(..)]` attribute",
                "an `unsafe trait`"
            ],
            "{got:?}"
        );
    }

    #[test]
    fn should_report_domain_when_macro_passes_a_crate_to_a_metavariable_head() {
        // given
        let files = [(
            "src/lib.rs",
            "macro_rules! via {\n    ($k:ident) => {\n        $k::fs::read(\"x\")\n    };\n}\npub fn f() {\n    let _ = via!(std);\n}\n",
        )];

        // when
        let got = diagnostics(DOMAIN, &files);

        // then
        assert!(
            got.iter()
                .any(|d| d.starts_with("ARCH-domain-purity") && d.ends_with("src/lib.rs:3)")),
            "{got:?}"
        );
    }

    #[test]
    fn should_report_domain_when_macro_path_continues_with_metavariable() {
        // given
        let files = [(
            "src/lib.rs",
            "macro_rules! call {\n    ($m:ident) => {\n        std::$m::read(\"x\")\n    };\n}\n",
        )];

        // when
        let got = diagnostics(DOMAIN, &files);

        // then
        assert!(
            got.iter().any(|d| d.starts_with("ARCH-domain-purity")
                && d.contains("(written `std::$..` in a macro)")),
            "{got:?}"
        );
    }
}
