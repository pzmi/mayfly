//! The include family (`include!`, `include_str!`, `include_bytes!`; foundation.md section
//! 3.2). The loader follows an `include!` written as an item or expression with a literal
//! path inside the package and analyses the file like a module. Everything else that could
//! compile code the checker never reads is refused (`ARCH-include-macro`), in every workspace
//! crate and context:
//!
//! - an include-family macro reached under another name: a `use` of `std::include`,
//!   `core::include`, `include_str` or `include_bytes` (with or without `as`), or an
//!   invocation whose path resolves to one but does not end in its name (`inc!`);
//! - an `include!` inside a macro token stream (a `macro_rules!` body, another macro's
//!   arguments): its path is relative to wherever the macro expands, so it is refused rather
//!   than followed.
//!
//! The data includes compile nothing, so they stay legal in token streams; in a production
//! crate a literal path that resolves outside the package is `ARCH-foreign-source`, like an
//! `include!` (a crate's source and fixtures stay in its own directory). Tools may read
//! repository files (`xtask` embeds `tools/versions.toml`).
//!
//! Pinned by `should_report_include_macro_when_it_is_renamed_or_imported`,
//! `should_report_include_macro_when_include_is_inside_macro_tokens` and
//! `should_report_foreign_source_when_production_crate_includes_data_from_outside`.

use super::super::policy::Policy;
use super::super::report::{Diagnostic, Rule};
use super::{Facts, INCLUDE_FAMILY, IncludeKind, RefKind};

/// The diagnostics of these rules for one package.
pub fn check(facts: &Facts, policy: &Policy) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    let production = policy
        .workspace
        .production
        .as_ref()
        .is_some_and(|p| p.contains(&facts.package));
    for reference in &facts.references {
        let written_last = reference.written.rsplit("::").next().unwrap_or_default();
        let renamed = reference.resolved.iter().find(|candidate| {
            let (Some(first), Some(last)) = (candidate.first(), candidate.last()) else {
                return false;
            };
            candidate.len() >= 2
                && matches!(first.as_str(), "std" | "core")
                && INCLUDE_FAMILY.contains(&last.as_str())
                && (reference.kind == RefKind::Path || written_last != last)
        });
        if let Some(candidate) = renamed {
            out.push(Diagnostic::new(
                Rule::IncludeMacro,
                format!(
                    "{} names `{}` (written `{}`): an include-family macro is invoked only by its own name, so the checker sees the file it reads",
                    facts.package,
                    candidate.join("::"),
                    reference.written
                ),
                reference.site.at(),
            ));
        }
    }
    for include in &facts.includes {
        if include.kind == IncludeKind::InTokens && include.name == "include" {
            out.push(Diagnostic::new(
                Rule::IncludeMacro,
                format!(
                    "{} writes `include!` inside a macro's tokens: the file it compiles depends on where the macro expands and is never analysed; write `include!` as an item or expression, or a `mod`",
                    facts.package
                ),
                include.site.at(),
            ));
        }
        if production
            && include.name != "include"
            && let Some(message) = &include.outside
        {
            out.push(Diagnostic::new(
                Rule::ForeignSource,
                format!("{} (written `{}!`)", message, include.written),
                include.site.at(),
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::super::testing::{facts_for, policy, rendered};
    use super::*;

    fn diagnostics(package: &str, files: &[(&str, &str)]) -> Vec<String> {
        rendered(&check(&facts_for(package, files), &policy()))
    }

    #[test]
    fn should_report_include_macro_when_it_is_renamed_or_imported() {
        // given
        let source = "use std::include as inc;\nuse core::include_str;\ninc!(\"../../other/src/lib.rs\");\npub fn f() -> &'static str {\n    std::include_str!(\"lib.rs\")\n}\n";

        // when
        let got = diagnostics("mayfly-domain", &[("src/lib.rs", source)]);

        // then
        let lines: Vec<&str> = got
            .iter()
            .filter(|d| d.starts_with("ARCH-include-macro"))
            .map(|d| d.rsplit(':').next().unwrap_or(""))
            .collect();
        assert_eq!(lines, ["1)", "2)", "3)"], "{got:?}");
    }

    #[test]
    fn should_report_include_macro_when_include_is_inside_macro_tokens() {
        // given
        let source = "macro_rules! m {\n    () => {\n        include!(\"x.rs\");\n    };\n}\nm!();\npub fn f() {\n    assert_eq!(include_str!(\"x.rs\").len(), 1);\n}\n";
        let files = [("src/lib.rs", source), ("src/x.rs", "fn g() {}\n")];

        // when
        let got = diagnostics("mayfly-domain", &files);

        // then
        let include: Vec<&String> = got
            .iter()
            .filter(|d| d.starts_with("ARCH-include-macro"))
            .collect();
        assert_eq!(include.len(), 1, "{got:?}");
        assert!(include[0].ends_with("src/lib.rs:3)"), "{got:?}");
    }

    #[test]
    fn should_report_foreign_source_when_production_crate_includes_data_from_outside() {
        // given
        let source = "pub fn f() -> usize {\n    include_str!(\"../../other/src/lib.rs\").len()\n        + include_bytes!(\"lib.rs\").len()\n}\n";
        let files = [("src/lib.rs", source), ("../other/src/lib.rs", "")];

        // when
        let production = diagnostics("mayfly-domain", &files);
        let tool = diagnostics("xtask", &files);

        // then
        let foreign: Vec<&String> = production
            .iter()
            .filter(|d| d.starts_with("ARCH-foreign-source"))
            .collect();
        assert_eq!(foreign.len(), 1, "{production:?}");
        assert!(
            foreign[0].contains("`crates/other/src/lib.rs`"),
            "{foreign:?}"
        );
        assert!(foreign[0].ends_with("src/lib.rs:2)"), "{foreign:?}");
        assert!(tool.is_empty(), "{tool:?}");
    }
}
