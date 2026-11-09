//! The rule catalog: every rule id the checker can report, a one-line description and the
//! negative fixtures under `tools/xtask/tests/architecture-invalid/` that prove it (foundation.md
//! section 3.4: a rule without a verified negative case is rejected).

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use super::fixtures::Expected;

macro_rules! rules {
    ($($variant:ident => $id:literal, $owner:literal, $description:literal, [$($fixture:literal),+ $(,)?];)+) => {
        /// Every rule the checker enforces. The id is what a diagnostic line starts with and
        /// what a negative fixture's `expected.txt` names.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub enum Rule {
            $(#[doc = $description] $variant,)+
        }

        impl Rule {
            /// Every rule, in catalog order.
            pub const ALL: &[Rule] = &[$(Rule::$variant),+];

            /// The rule id, for example `ARCH-unknown-crate`.
            pub fn id(self) -> &'static str {
                match self { $(Rule::$variant => $id,)+ }
            }

            /// The task that introduced the rule.
            pub fn owner(self) -> &'static str {
                match self { $(Rule::$variant => $owner,)+ }
            }

            /// One line saying what the rule rejects.
            pub fn description(self) -> &'static str {
                match self { $(Rule::$variant => $description,)+ }
            }

            /// The negative fixtures whose `expected.txt` names this rule.
            pub fn fixtures(self) -> &'static [&'static str] {
                match self { $(Rule::$variant => &[$($fixture),+],)+ }
            }
        }
    };
}

rules! {
    PolicySchema => "ARCH-policy-schema", "FND-02",
        "architecture.toml is missing, malformed, has an unknown field or another schema version.",
        ["policy-schema-version", "policy-unknown-field"];
    Metadata => "ARCH-metadata", "FND-02",
        "cargo metadata or a crate manifest cannot be read, including a cyclic workspace.",
        ["normal-cycle", "unreadable-member-manifest"];
    MissingMember => "ARCH-missing-member", "FND-02",
        "A [workspace] members entry has no Cargo.toml on disk.",
        ["missing-member"];
    NotAMember => "ARCH-not-a-member", "FND-02",
        "A crate under crates/ or tools/ is not a workspace member.",
        ["not-a-member"];
    UnknownCrate => "ARCH-unknown-crate", "FND-02",
        "A workspace crate is in no policy category.",
        ["unknown-crate"];
    MissingCrate => "ARCH-missing-crate", "FND-02",
        "The policy names a crate that is not in the workspace.",
        ["missing-crate"];
    EmptySelector => "ARCH-empty-selector", "FND-02",
        "A policy category is present but empty.",
        ["empty-selector"];
    DuplicateCategory => "ARCH-duplicate-category", "FND-02",
        "A crate is listed in more than one category, or twice in one.",
        ["duplicate-category"];
    FuzzCrate => "ARCH-fuzz-crate", "FND-02",
        "The fuzz crate is not listed in fuzz_only, is a workspace member or lacks its own [workspace] table, fuzz_only names a crate that is not fuzz/Cargo.toml's package, or the fuzz crate depends on a crate its policy entries do not allow.",
        ["fuzz-crate-without-own-workspace", "fuzz-depends-on-unlisted-crate", "unlisted-fuzz-crate"];
    ForbiddenEdge => "ARCH-forbidden-edge", "FND-02",
        "A normal workspace edge is not in [dependencies.normal]: a reversed layer, or a control-plane crate reaching the data plane or the reverse.",
        ["cli-bypasses-composition", "control-depends-on-data", "data-depends-on-control", "reversed-edge"];
    IndependentOracle => "ARCH-independent-oracle", "FND-02",
        "An independent oracle ([purity] independent) has a normal, dev or build edge to a workspace crate.",
        ["faultlab-dev-depends-on-testkit", "faultlab-imports-control-domain"];
    UndeclaredBuildDep => "ARCH-undeclared-build-dep", "FND-02",
        "A build edge to a workspace crate is not in [dependencies.build].",
        ["undeclared-build-dep"];
    ForbiddenDevEdge => "ARCH-forbidden-dev-edge", "FND-02",
        "A dev edge targets a crate that is neither test_only nor in the normal allowlist.",
        ["forbidden-dev-edge"];
    ProductionClosure => "ARCH-production-closure", "FND-02",
        "A production crate reaches a test_only or tool_only crate over normal or build edges.",
        ["testkit-in-production", "transitive-test-only"];
    PolicyCycle => "ARCH-policy-cycle", "FND-02",
        "The [dependencies] allowlist contains a cycle.",
        ["policy-cycle"];
    UnlistedExternal => "ARCH-unlisted-external", "FND-02",
        "A direct third-party dependency is not in [external.<kind>] for its crate.",
        ["domain-uses-tokio", "target-specific-external", "unlisted-build-external", "unlisted-external"];
    ExternalFeature => "ARCH-external-feature", "FND-02",
        "A third-party edge enables a feature outside its [external.<kind>] list.",
        ["control-tokio-rt-feature"];
    ExternalSource => "ARCH-external-source", "FND-02",
        "A package is not from crates.io and has no [exceptions.sources] entry citing an existing ADR.",
        ["git-dependency", "path-outside-workspace", "source-exception-missing-adr"];
    SourceParse => "ARCH-source-parse", "FND-02",
        "A reachable source file cannot be analysed: it does not parse, a module file is missing, or an include!/#[path] is not a literal.",
        ["include-non-literal", "unparseable-source"];
    ForeignSource => "ARCH-foreign-source", "ARCH-01",
        "A target root, #[path] module or include! resolves outside the owning package's directory, so one crate would compile another's source under its own policy; a production crate's include_str!/include_bytes! reads a file outside it or through a path that is not one string literal.",
        ["control-path-into-data-domain", "faultlab-includes-adapter-source", "include-str-non-literal", "include-str-outside-package"];
    IncludeMacro => "ARCH-include-macro", "ARCH-02",
        "An include-family macro (include!, include_str!, include_bytes!) is reached under another name (a use of std/core::include*, an alias), or an include! is written inside a macro token stream, so the file it compiles is never analysed.",
        ["include-in-macro-body", "include-through-alias"];
    DomainPurity => "ARCH-domain-purity", "FND-02",
        "A [purity] pure crate names I/O (also std::path's file-system functions), a process, env, a thread, the clock (also through <T>::now, a type alias, Self, UNIX_EPOCH or elapsed), randomness (also a std HashMap/HashSet), a backtrace, a process-global hook or OS error, global mutable state (atomics, locks, lazy statics, thread_local!, static mut), a runtime or executor, HTTP, SQL, the console or an outer layer in production code; or its test code calls env!/option_env! for anything but CARGO_MANIFEST_DIR and CARGO_PKG_*.",
        ["cfg-attr-path-default", "control-domain-std-fs", "domain-aliased-fs", "domain-backtrace", "domain-clock-through-type-alias", "domain-fs-via-macro-head", "domain-hashmap-iteration", "domain-path-fs", "domain-qself-clock", "domain-static-counter", "domain-std-fs", "domain-std-net", "domain-test-reads-env", "domain-thread-local", "domain-tokio-via-macro-path", "domain-unix-epoch-elapsed"];
    PureClippyConfig => "ARCH-pure-clippy-config", "ARCH-02",
        "A [purity] pure crate has no clippy.toml, has one shadowed by a .clippy.toml, lacks a required disallowed-methods, disallowed-types or disallowed-macros entry or sets allow-invalid on one, or a .cargo/config.toml [env] sets CLIPPY_CONF_DIR: the semantic layer of the purity rules would be off.",
        ["pure-clippy-config-missing-entry", "pure-crate-without-clippy-config"];
    ProcessAuthority => "ARCH-process-authority", "FND-02",
        "Production code of a production crate outside [purity] process_authority names a process API (std::process beyond ExitCode, tokio::process, ..), also through another workspace crate's re-export or type alias.",
        ["cli-uses-composition-type-alias", "cli-uses-reexported-command", "process-in-control"];
    LayerIsolation => "ARCH-layer-isolation", "FND-02",
        "A crate names a crate it must not reach, in production or test code (test code may name test-only crates): the other plane's crates, a sibling adapter, composition or the CLI from an adapter, adapter technology from an application, anything but composition from the CLI.",
        ["adapter-names-sibling-adapter", "cli-names-adapter-through-alias", "data-names-journal", "data-test-names-sqlite-adapter"];
    UnsafeCode => "ARCH-unsafe-code", "FND-02",
        "Owned code contains an unsafe block, fn, impl, trait, extern block or unsafe attribute.",
        ["unsafe-block"];
    MissingSelector => "ARCH-missing-selector", "FND-02",
        "A crate the semantic rules name is missing and not listed in [semantic] absent, or absent lists a member or a crate no rule names, or the shared test kit lacks the poll helper file the test-sleep exemption names.",
        ["missing-selector", "poll-helper-missing"];
    LintConfig => "ARCH-lint-config", "FND-02",
        "A lint or source policy is relaxed outside source: a member's [lints] is not workspace = true, a workspace lint is allowed, rustflags allow or cap lints or pass -Z, clippy.toml allows, or any .cargo/config.toml in the repository replaces a package source, swaps or wraps rustc, sets a build-steering [env] variable (CLIPPY_*, RUSTC_*, RUSTFLAGS, CARGO_BUILD_*, ..), has [unstable], paths or a target runner, or an alias other than the pinned xtask one.",
        ["cargo-config-alias-shadows-clippy", "cargo-config-rustc-wrapper", "cargo-config-steering-env", "cargo-config-z-flag", "cargo-source-replacement", "clippy-toml-allow", "member-lints-override", "rustflags-allow"];
    UnfinishedCode => "ARCH-unfinished-code", "FND-02",
        "Production code calls todo!, unimplemented! or dbg! without an [exceptions.lints] entry.",
        ["todo-in-production"];
    IgnoredTest => "ARCH-ignored-test", "FND-02",
        "Code carries an #[ignore] attribute.",
        ["ignored-test"];
    LintSuppression => "ARCH-lint-suppression", "FND-02",
        "An allow, expect or coverage(off) attribute lacks an [exceptions.lints] entry citing an existing ADR, an entry matches nothing, or an attribute in a macro is named by a metavariable (#[$level(lint)]), which no entry can approve.",
        ["allow-with-missing-adr", "lint-level-through-metavariable", "stale-lint-exception", "unjustified-allow"];
    TestSleep => "ARCH-test-sleep", "FND-02",
        "Test code sleeps outside the bounded poll helper mayfly-testkit-shared/src/poll.rs, or a test-only crate waits in real time in a file no [[exceptions.lints]] entry (lint = \"test_sleep\") approves.",
        ["sleep-in-test", "test-sleep-exception-in-test-file"];
    TestName => "ARCH-test-name", "FND-02",
        "A test function's name does not match should_<outcome>_when_<condition>, or a macro hides a test.",
        ["badly-named-proptest", "test-without-should"];
    TestStructure => "ARCH-test-structure", "FND-02",
        "A test function's body lacks the // given, // when and // then comments in that order.",
        ["missing-given-when-then"];
    PlatformBranch => "ARCH-platform-branch", "FND-02",
        "Code under crates/*/src branches at run time on cfg!(target_os, target_family, target_vendor, target_env, unix or windows) instead of #[cfg] items or blocks.",
        ["cfg-macro-platform-branch"];
    CommandIsolation => "ARCH-command-isolation", "FND-02",
        "mayfly-control command code names crate::queries or an HTTP type (axum, http, hyper, reqwest, tower, mayfly_adapter_github), also through re-exports.",
        ["command-imports-query", "command-names-http-dto"];
    QueryPurity => "ARCH-query-purity", "FND-02",
        "mayfly-control query code names crate::commands, or takes &mut self or &mut of kernel or control-plane state.",
        ["query-calls-command", "query-mutates-state"];
    ObservabilityIsolation => "ARCH-observability-isolation", "FND-02",
        "A pure crate names an instrumentation crate, or a tracing/log/observability/metrics value reaches a branch condition or returned value in pure or application code.",
        ["instrumentation-drives-decision"];
    HiddenSeam => "ARCH-hidden-seam", "FND-02",
        "A crate other than mayfly-composition and mayfly-conformance names a #[doc(hidden)] item of mayfly-composition.",
        ["cli-calls-hidden-seam", "cli-names-hidden-trait"];
    PlaneSeparation => "ARCH-plane-separation", "ARCH-01",
        "A production crate is in no [planes] list or in two, a control, data or shared crate reaches a crate of a forbidden plane (in the [dependencies] allowlist, or over the resolved graph with its own dev edges as the first hop), or a shared crate dev-depends on a crate outside the shared plane (ADR 0019).",
        ["data-dev-depends-on-conformance", "policy-lets-data-reach-control", "shared-dev-depends-on-testkit"];
    PlaneVocabulary => "ARCH-plane-vocabulary", "ARCH-01",
        "A crate names or declares a word of another plane's [planes.vocabulary] (a CP-only type such as Journal in a data or shared crate, a DP-only type such as VmProvider in a control or shared crate), in any context and through any crate.",
        ["control-names-vm-provider", "data-declares-journal", "data-names-journal-via-shared", "shared-declares-vm-provider"];
    AnyhowInLibrary => "ARCH-anyhow-in-library", "ARCH-01",
        "Production code of a library target names anyhow; libraries return typed errors.",
        ["anyhow-in-library"];
    UnboundedChannel => "ARCH-unbounded-channel", "ARCH-01",
        "Production code of a production crate creates or names an unbounded channel or queue (tokio unbounded_channel, std::sync::mpsc::channel, crossbeam/flume/async-channel/futures unbounded).",
        ["unbounded-channel-in-data"];
    DetachedSpawn => "ARCH-detached-spawn", "ARCH-01",
        "Production code of a production crate spawns a detached task or thread (tokio::spawn, spawn_blocking, spawn_local, std::thread::spawn) outside the listed supervision, writer, transport and process modules.",
        ["detached-spawn-in-control"];
    SecretSerialize => "ARCH-secret-serialize", "ARCH-01",
        "Production code derives or implements Serialize for Secret or a type that contains it, derives Debug on Secret, or implements Debug/Display for Secret outside the file that defines it.",
        ["secret-container-serializes", "secret-derives-debug"];
    AggregateRegistered => "ARCH-aggregate-registered", "ARCH-01",
        "An `impl Aggregate for X` in mayfly-control-domain production code has no visitor.visit::<X>() in ControlAggregates::accept, so the golden and determinism harnesses would skip it.",
        ["unregistered-aggregate"];
    ExhaustiveTransition => "ARCH-exhaustive-transition", "KER-04",
        "Production code of a [purity] pure crate falls through a catch-all (`_`, a bare binding, or `..` among a tuple's elements): in any match inside a transition function (decide, evolve, step, or a decide_/evolve_/step_ helper), in any match over a pure crate's enum, or implicitly through matches!, if let, while let or let..else on such an enum's variant, so a new state or stimulus would be swallowed instead of failing to compile.",
        ["wildcard-transition-arm", "helper-wildcard-on-domain-enum", "matches-on-domain-enum"];
    MacroExport => "ARCH-macro-export", "ARCH-02",
        "A production crate declares a #[macro_export] macro without an [[exceptions.lints]] entry (lint = \"macro_export\") citing an ADR: its body would expand in other crates, outside the rules that hold where it is written.",
        ["composition-exports-process-macro"];
    NightlyFeatures => "ARCH-nightly-features", "ARCH-03",
        "Code writes #![feature(..)] (also through cfg_attr or inside a macro's tokens) or a macro 2.0 item (`macro name`): nightly-only constructs the pinned stable toolchain builds only under RUSTC_BOOTSTRAP.",
        ["feature-attribute", "macro-two-point-zero"];
    EventPayloadSerde => "ARCH-event-payload-serde", "ARCH-04",
        "Production code of mayfly-control-domain or mayfly-domain writes #[serde(..)] with try_from, from, into, with, deserialize_with, serialize_with, alias, other, untagged, tag, content, flatten or unparsable arguments (container, field or variant, also in macro tokens; a mayfly-domain struct may convert, an enum may not), or implements Serialize or Deserialize by hand: the event shape tracer would pass what it cannot see.",
        ["event-payload-alias-field", "event-payload-flatten", "event-payload-try-from-enum"];
    SampleCoverage => "ARCH-sample-coverage", "ARCH-04",
        "A mayfly-domain type decoded through a non-integer #[serde(try_from)] (also one a macro declares) does not implement Sample, is not listed in texts() of mayfly-control-domain's src/shape.rs, or cannot be resolved from its macro.",
        ["validated-type-without-sample"];
    EventSetMacro => "ARCH-event-set-macro", "ARCH-04",
        "Production code implements EventSet anywhere but the body of macro_rules! event_set in mayfly-control-domain/src/event.rs, so a set would lack the generated shape() and codec.",
        ["event-set-by-hand"];
    StoreSql => "ARCH-store-sql", "ES-02",
        "A string literal or .sql file under mayfly-adapter-sqlite's src/ uses a form only SQLite has: INSERT OR ..., REPLACE INTO, AUTOINCREMENT, rowid, _rowid_, oid, a json...() function or the -> and ->> operators (WITHOUT ROWID is allowed): the PostgreSQL adapter would have to fake it (doc/event-store.md section 4.3).",
        ["store-sql-insert-or-ignore"];
    SingleAppend => "ARCH-single-append", "ES-03",
        "A control-plane or both-planes crate calls Journal::append (`.append(x)` with a by-value, non-literal argument, also in macro tokens, or the path Journal::append) outside AggregateStore's save and settle in mayfly-control, an impl Journal block, the testkit contract suite or an adapter's own test code.",
        ["second-append-caller"];
}

/// Compares the catalog with the negative fixtures in `dir`: every rule has fixtures, each
/// exists and names its rule, and every fixture belongs to a rule. Returns the problems.
pub fn completeness_problems(dir: &Path) -> Vec<String> {
    let mut problems = Vec::new();
    let mut named: BTreeMap<String, String> = BTreeMap::new();
    match fs::read_dir(dir) {
        Ok(entries) => {
            for entry in entries.filter_map(Result::ok).filter(|e| e.path().is_dir()) {
                let case = entry.file_name().to_string_lossy().into_owned();
                let parsed = fs::read_to_string(entry.path().join("expected.txt"))
                    .map_err(|e| e.to_string())
                    .and_then(|text| Expected::parse(&text));
                match parsed {
                    Ok(expected) => {
                        named.insert(case, expected.rule);
                    }
                    Err(err) => problems.push(format!("fixture `{case}`: expected.txt: {err}")),
                }
            }
        }
        Err(err) => problems.push(format!("cannot list negative fixtures: {err}")),
    }
    let mut claimed = BTreeSet::new();
    for &rule in Rule::ALL {
        for &fixture in rule.fixtures() {
            claimed.insert(fixture);
            match named.get(fixture) {
                None => problems.push(format!(
                    "rule {} ({}) has no negative fixture `{fixture}`",
                    rule.id(),
                    rule.description()
                )),
                Some(id) if id != rule.id() => problems.push(format!(
                    "fixture `{fixture}` is cataloged for {} but its expected.txt names {id}",
                    rule.id()
                )),
                Some(_) => {}
            }
        }
    }
    for case in named.keys() {
        if !claimed.contains(case.as_str()) {
            problems.push(format!("fixture `{case}` is not in the rule catalog"));
        }
    }
    problems
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::architecture::{FIXTURES_DIR, repository_root};

    fn fixtures_dir() -> std::path::PathBuf {
        repository_root().join(FIXTURES_DIR)
    }

    /// Copies only the `expected.txt` files, which is all the completeness check reads.
    fn copy_without(source: &Path, target: &Path, skip: &str) {
        for entry in fs::read_dir(source).unwrap().map(Result::unwrap) {
            let name = entry.file_name();
            if !entry.path().is_dir() || name == skip {
                continue;
            }
            fs::create_dir(target.join(&name)).unwrap();
            fs::copy(
                entry.path().join("expected.txt"),
                target.join(&name).join("expected.txt"),
            )
            .unwrap();
        }
    }

    #[test]
    fn should_have_negative_fixture_when_rule_is_declared() {
        // given
        let dir = fixtures_dir();

        // when
        let problems = completeness_problems(&dir);

        // then
        assert!(problems.is_empty(), "{}", problems.join("\n"));
    }

    #[test]
    fn should_fail_completeness_when_any_negative_fixture_is_removed() {
        // given
        let source = fixtures_dir();
        let cases: Vec<&str> = Rule::ALL
            .iter()
            .flat_map(|r| r.fixtures())
            .copied()
            .collect();

        // when
        let passing: Vec<&str> = cases
            .iter()
            .copied()
            .filter(|case| {
                let copy = tempfile::TempDir::new().unwrap();
                copy_without(&source, copy.path(), case);
                completeness_problems(copy.path()).is_empty()
            })
            .collect();

        // then
        assert!(cases.len() >= 25, "{}", cases.len());
        assert!(passing.is_empty(), "removal not detected: {passing:?}");
    }

    #[test]
    fn should_report_orphan_when_fixture_is_not_cataloged() {
        // given
        let copy = tempfile::TempDir::new().unwrap();
        copy_without(&fixtures_dir(), copy.path(), "");
        fs::create_dir(copy.path().join("stray")).unwrap();
        fs::write(
            copy.path().join("stray/expected.txt"),
            "rule: ARCH-metadata\nat: Cargo.toml\n",
        )
        .unwrap();

        // when
        let problems = completeness_problems(copy.path());

        // then
        assert_eq!(problems, ["fixture `stray` is not in the rule catalog"]);
    }

    #[test]
    fn should_have_unique_ids_when_catalog_is_listed() {
        // given
        let ids: Vec<&str> = Rule::ALL.iter().map(|r| r.id()).collect();

        // when
        let unique: BTreeSet<&str> = ids.iter().copied().collect();

        // then
        assert_eq!(unique.len(), ids.len(), "{ids:?}");
        assert!(ids.iter().all(|id| id.starts_with("ARCH-")));
        assert!(
            Rule::ALL
                .iter()
                .all(|r| r.description().ends_with('.') && !r.description().contains('\n'))
        );
    }
}
