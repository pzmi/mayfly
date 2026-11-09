//! Command/query separation and observability (foundation.md sections 2.4 and 3.3; plan
//! section 5.4).
//!
//! - `ARCH-command-isolation`: production code in `mayfly-control`'s `commands` module names
//!   nothing under `crate::queries` and no HTTP type, also through re-exports.
//! - `ARCH-query-purity`: production code in its `queries` module names nothing under
//!   `crate::commands`, takes no `&mut` receiver and no `&mut` of a kernel or control-plane
//!   state type (or of a type it cannot tell apart from one).
//! - `ARCH-observability-isolation`: pure-crate production code names no instrumentation
//!   crate, and in pure and application production code no instrumentation value reaches a
//!   decision (see `flow`).
//!
//! Both command and query rules apply to whatever `commands` and `queries` modules
//! `mayfly-control` has. Their selectors fail closed (`ARCH-missing-selector`: a production
//! `mayfly-control` must have both modules, so a rename cannot turn the rules into no-ops)
//! only once [`CQRS_SELECTORS_REQUIRED`] is set: CP-01 creates the modules, sets it and adds
//! "queries take no write port". Ported from site-preview, whose `ReloadState` read allowlist
//! has no mayfly counterpart and was dropped.

use super::super::policy::Policy;
use super::super::report::{Diagnostic, Rule};
use super::flow::{INSTRUMENTATION_CRATES, INSTRUMENTATION_MODULES};
use super::rules_purity::{APPLICATIONS, PURE};
use super::{Context, Facts, Site, first_hit};

/// The crate whose modules the command and query rules select.
pub const APPLICATION: &str = "mayfly-control";

/// Whether a production [`APPLICATION`] must have both [`COMMANDS`] and [`QUERIES`]. Off until
/// CP-01 writes them (then pinned by the `missing-query-selector` fixture CP-01 adds).
pub const CQRS_SELECTORS_REQUIRED: bool = false;

/// The command module of [`APPLICATION`].
pub const COMMANDS: &str = "commands";

/// The query module of [`APPLICATION`].
pub const QUERIES: &str = "queries";

/// What a command must not name: HTTP DTOs, HTTP clients and the GitHub adapter.
const HTTP: &[&[&str]] = &[
    &["axum"],
    &["axum_core"],
    &["http"],
    &["http_body"],
    &["hyper"],
    &["reqwest"],
    &["tower"],
    &["tower_http"],
    &["mayfly_adapter_github"],
];

/// Crates a pure crate must not name: instrumentation belongs to the outer layers.
const PURE_INSTRUMENTATION: &[&[&str]] = &[
    &["tracing"],
    &["tracing_core"],
    &["tracing_subscriber"],
    &["tracing_log"],
    &["log"],
    &["metrics"],
    &["opentelemetry"],
];

/// Root crates of the types a query must not take by `&mut`: its own crate, the shared kernel
/// and the control-plane domain.
const STATE_ROOTS: &[&str] = &["crate", "mayfly_domain", "mayfly_control_domain"];

/// The diagnostics of these rules for one package.
pub fn check(facts: &Facts, policy: &Policy) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    if facts.package == APPLICATION {
        let production = policy
            .workspace
            .production
            .as_ref()
            .is_some_and(|p| p.contains(&facts.package));
        if production && CQRS_SELECTORS_REQUIRED {
            out.extend(selector_problems(facts));
        }
        out.extend(commands(facts));
        out.extend(queries(facts));
    }
    let pure = PURE.contains(&facts.package.as_str());
    if pure {
        for reference in production(&facts.references, |r| &r.site) {
            if let Some(hit) = first_hit(reference, PURE_INSTRUMENTATION) {
                out.push(Diagnostic::new(
                    Rule::ObservabilityIsolation,
                    format!(
                        "{} names `{hit}`: instrumentation stays outside the pure crates, so it cannot change a decision",
                        facts.package
                    ),
                    reference.site.at(),
                ));
            }
        }
    }
    if pure || APPLICATIONS.contains(&facts.package.as_str()) {
        for decision in production(&facts.decisions, |d| &d.site) {
            let inside_instrumentation = decision
                .module
                .iter()
                .any(|m| INSTRUMENTATION_MODULES.contains(&m.as_str()));
            if inside_instrumentation {
                continue;
            }
            out.push(Diagnostic::new(
                Rule::ObservabilityIsolation,
                format!(
                    "{} uses instrumentation {} in {}: a value from {} or a module named {} must not drive a branch or a returned decision",
                    facts.package,
                    decision.instrumentation,
                    decision.position,
                    INSTRUMENTATION_CRATES.join("/"),
                    INSTRUMENTATION_MODULES.join("/"),
                ),
                decision.site.at(),
            ));
        }
    }
    out
}

/// Production items of `items`, by their site.
fn production<T>(items: &[T], site: impl Fn(&T) -> &Site) -> impl Iterator<Item = &T> {
    items
        .iter()
        .filter(move |item| site(item).context == Context::Production)
}

/// Whether code at `site` in module `module` belongs to the application module `name`: by
/// module path in its target, or by file (`src/<name>.rs`, `src/<name>/..`). Either counts.
fn in_module(facts: &Facts, site: &Site, module: &[String], name: &str) -> bool {
    let file = facts.in_package(site);
    module.first().is_some_and(|m| m == name)
        || file == format!("src/{name}.rs")
        || file.starts_with(&format!("src/{name}/"))
}

/// Whether `path` lies under `crate::<module>`.
fn under_crate_module(path: &[String], module: &str) -> bool {
    path.len() >= 2 && path[0] == "crate" && path[1] == module
}

/// The first candidate or traversed module of `reference` under `crate::<module>`, rendered.
fn names_module(reference: &super::Reference, module: &str) -> Option<String> {
    reference
        .resolved
        .iter()
        .chain(&reference.through)
        .find(|p| under_crate_module(p, module))
        .map(|p| p.join("::"))
}

fn written_note(written: &str, hit: &str) -> String {
    if written == hit {
        String::new()
    } else {
        format!(" (written `{written}`)")
    }
}

fn commands(facts: &Facts) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    for reference in production(&facts.references, |r| &r.site) {
        if !in_module(facts, &reference.site, &reference.module, COMMANDS) {
            continue;
        }
        let hit = names_module(reference, QUERIES)
            .map(|h| (h, "the read side"))
            .or_else(|| first_hit(reference, HTTP).map(|h| (h, "an HTTP type")));
        if let Some((hit, what)) = hit {
            out.push(Diagnostic::new(
                Rule::CommandIsolation,
                format!(
                    "{APPLICATION} command code names `{hit}`{}, {what}: commands import no query handler, read-side implementation or HTTP DTO (foundation.md section 3.3)",
                    written_note(&reference.written, &hit)
                ),
                reference.site.at(),
            ));
        }
    }
    out
}

fn queries(facts: &Facts) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    let mut push = |site: &Site, message: String| {
        out.push(Diagnostic::new(
            Rule::QueryPurity,
            format!("{APPLICATION} query code {message} (foundation.md section 3.3)"),
            site.at(),
        ));
    };
    for reference in production(&facts.references, |r| &r.site) {
        if !in_module(facts, &reference.site, &reference.module, QUERIES) {
            continue;
        }
        if let Some(hit) = names_module(reference, COMMANDS) {
            push(
                &reference.site,
                format!(
                    "names `{hit}`{}: a query never calls a command handler to implement a read",
                    written_note(&reference.written, &hit)
                ),
            );
        }
    }
    for param in production(&facts.mut_params, |p| &p.site) {
        if !in_module(facts, &param.site, &param.module, QUERIES) {
            continue;
        }
        let state = param
            .resolved
            .iter()
            .find(|c| c.first().is_some_and(|s| STATE_ROOTS.contains(&s.as_str())));
        let why = if param.receiver {
            Some("a `&mut self` receiver".to_owned())
        } else if param.generic {
            Some(format!(
                "`{}` by `&mut` of a generic or `Self` type, which may be state",
                param.written
            ))
        } else {
            state.map(|c| format!("`{}` as `&mut {}`", param.written, c.join("::")))
        };
        if let Some(why) = why {
            push(
                &param.site,
                format!(
                    "fn `{}` takes {why}: a query observes state and never mutates it",
                    param.function
                ),
            );
        }
    }
    out
}

/// `ARCH-missing-selector` for the module selectors of these rules.
fn selector_problems(facts: &Facts) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    for name in [COMMANDS, QUERIES] {
        let present = facts
            .modules
            .iter()
            .any(|m| m.first().is_some_and(|f| f == name));
        if !present {
            out.push(Diagnostic::new(
                Rule::MissingSelector,
                format!(
                    "the command/query rules select module `{name}` of {APPLICATION}, which its production code does not have: a rename would make them pass unchecked; rename the selector in rules_cqrs.rs too"
                ),
                format!("{}/src/{name}", facts.dir),
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::super::testing::{facts_for, policy, rendered};
    use super::*;

    const STATE: (&str, &str) = (
        "src/state.rs",
        "pub struct Pools;\nimpl Pools {\n    pub fn current(&self) -> u64 { 0 }\n    pub fn drain(&self) {}\n}\n",
    );

    /// Diagnostics of these rules for the control plane with `files` (the first is the root).
    fn diagnostics(files: &[(&str, &str)]) -> Vec<String> {
        rendered(&check(&facts_for(APPLICATION, files), &policy()))
    }

    fn of(rule: Rule, got: &[String]) -> Vec<String> {
        got.iter()
            .filter(|d| d.starts_with(&format!("{}:", rule.id())))
            .cloned()
            .collect()
    }

    const ROOT: &str = "pub mod commands;\npub mod queries;\npub mod state;\npub use commands::Drain;\npub use queries::Status;\n";

    #[test]
    fn should_pass_when_command_and_query_keep_their_sides() {
        // given
        let files = [
            ("src/lib.rs", ROOT),
            (
                "src/commands/mod.rs",
                "use crate::state::Pools;\npub struct Drain;\nimpl Drain {\n    pub fn run(&mut self, s: &Pools) { s.drain(); }\n}\n",
            ),
            (
                "src/queries/mod.rs",
                "use core::fmt;\nuse crate::state::Pools;\npub struct Status;\nimpl Status {\n    pub fn run(&self, s: &Pools) -> u64 { s.current() }\n}\nimpl fmt::Debug for Status {\n    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str(\"r\") }\n}\n",
            ),
            STATE,
        ];

        // when
        let got = diagnostics(&files);

        // then
        assert_eq!(got, Vec::<String>::new());
    }

    #[test]
    fn should_report_command_when_it_reaches_queries_through_a_reexport() {
        // given
        let files = [
            ("src/lib.rs", ROOT),
            (
                "src/commands/mod.rs",
                "pub struct Drain;\npub fn f(_: &crate::Status) {}\npub fn g(_: http::Request<()>) {}\n",
            ),
            ("src/queries/mod.rs", "pub struct Status;\n"),
            STATE,
        ];

        // when
        let got = of(Rule::CommandIsolation, &diagnostics(&files));

        // then
        assert_eq!(got.len(), 2, "{got:?}");
        assert!(
            got[0].contains("`crate::queries::Status` (written `crate::Status`)"),
            "{got:?}"
        );
        assert!(got[1].contains("`http::Request`"), "{got:?}");
    }

    #[test]
    fn should_report_query_when_it_mutates_or_calls_a_command() {
        // given
        let files = [
            ("src/lib.rs", ROOT),
            ("src/commands/mod.rs", "pub struct Drain;\n"),
            (
                "src/queries/mod.rs",
                "use mayfly_control_domain::Pool;\npub struct Status;\nimpl Status {\n    pub fn a(&mut self) {}\n    pub fn b(&self, s: &mut Pool) {}\n    pub fn c<T>(&self, t: &mut T) {}\n    pub fn d(&self, s: &mut crate::state::Pools) {}\n    pub fn e(&self) { let _ = crate::Drain; }\n    pub fn g(&self, buf: &mut String) { buf.push('x'); }\n}\n",
            ),
            STATE,
        ];

        // when
        let got = of(Rule::QueryPurity, &diagnostics(&files));

        // then
        let lines: Vec<&str> = got
            .iter()
            .map(|d| d.rsplit(':').next().unwrap_or(""))
            .collect();
        assert_eq!(lines, ["8)", "4)", "5)", "6)", "7)"], "{got:?}");
    }

    #[test]
    fn should_report_missing_selector_when_queries_module_is_renamed() {
        // given
        let files = [
            ("src/lib.rs", "pub mod commands;\npub mod reads;\n"),
            ("src/commands/mod.rs", ""),
            ("src/reads/mod.rs", ""),
        ];

        // when
        let got = of(
            Rule::MissingSelector,
            &rendered(&selector_problems(&facts_for(APPLICATION, &files))),
        );

        // then
        assert_eq!(got.len(), 1, "{got:?}");
        assert!(got[0].contains("module `queries`"), "{got:?}");
    }

    #[test]
    fn should_not_require_the_modules_when_cp_01_has_not_written_them() {
        // given
        let files = [("src/lib.rs", "pub fn stub() {}\n")];

        // when
        let got = of(Rule::MissingSelector, &diagnostics(&files));

        // then
        assert_eq!(got, Vec::<String>::new());
    }

    #[test]
    fn should_report_observability_when_a_pure_crate_names_tracing_or_value_drives_decision() {
        // given
        let pure = facts_for(
            "mayfly-control-domain",
            &[("src/lib.rs", "pub fn f() { tracing::info!(\"x\"); }\n")],
        );
        let root = format!("{ROOT}pub mod observability;\n");
        let application = [
            ("src/lib.rs", root.as_str()),
            (
                "src/commands/mod.rs",
                "pub struct Drain;\npub fn run() -> bool {\n    let busy = crate::observability::depth() > 3;\n    if busy { return false; }\n    true\n}\n",
            ),
            ("src/queries/mod.rs", "pub struct Status;\n"),
            (
                "src/observability.rs",
                "pub fn depth() -> u8 { tracing::enabled!(tracing::Level::INFO) as u8 }\n",
            ),
            STATE,
        ];

        // when
        let from_pure = of(
            Rule::ObservabilityIsolation,
            &rendered(&check(&pure, &policy())),
        );
        let from_application = of(Rule::ObservabilityIsolation, &diagnostics(&application));

        // then
        assert_eq!(from_pure.len(), 1, "{from_pure:?}");
        assert!(from_pure[0].contains("`tracing::info`"), "{from_pure:?}");
        assert_eq!(from_application.len(), 1, "{from_application:?}");
        assert!(
            from_application[0].contains("`busy` (bound from instrumentation) in an if condition")
                && from_application[0].ends_with("src/commands/mod.rs:4)"),
            "{from_application:?}"
        );
    }

    #[test]
    fn should_report_when_query_evades_through_glob_typed_receiver_or_inline_module() {
        // given
        let files = [
            (
                "src/lib.rs",
                "pub mod commands;\npub mod state;\npub use commands::Drain;\npub mod queries {\n    use crate::*;\n    pub struct Status;\n    impl Status {\n        pub fn a(&self) { let _ = Drain; }\n        pub fn b(&self, s: &mut state::Pools) { let _ = s; }\n        pub fn c(self: &mut Self) {}\n    }\n}\n",
            ),
            ("src/commands/mod.rs", "pub struct Drain;\n"),
            STATE,
        ];

        // when
        let got = of(Rule::QueryPurity, &diagnostics(&files));

        // then
        let lines: Vec<&str> = got
            .iter()
            .map(|d| d.rsplit(':').next().unwrap_or(""))
            .collect();
        assert_eq!(lines, ["8)", "9)", "10)"], "{got:?}");
    }
}
