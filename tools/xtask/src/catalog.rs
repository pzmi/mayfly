//! The check catalog: every gate stage and `xtask` command, with its level, owner and whether
//! `check` includes it. `list-checks` prints it; `main` reads owners from it.

/// One catalog row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Check {
    /// Unique id. Equals the subcommand name for a command row.
    pub id: &'static str,
    /// Test or gate level, as named in foundation.md section 5.
    pub level: &'static str,
    /// The command line that runs it.
    pub command: &'static str,
    /// The mayfly task that introduced the implementation.
    pub owner: &'static str,
    /// Whether `cargo xtask check` includes it. A command row (`quick`, `doctor`) is `false`;
    /// a stage that is not implemented yet has no row at all.
    pub in_check: bool,
}

const fn row(
    id: &'static str,
    level: &'static str,
    command: &'static str,
    owner: &'static str,
    in_check: bool,
) -> Check {
    Check {
        id,
        level,
        command,
        owner,
        in_check,
    }
}

/// All rows. `quick` runs the stage rows `fmt-check`, `architecture`, `clippy`, `unit-tests`
/// and `doctests`. `platform-tests` is a stage of `test` that `check` leaves out (the macOS CI
/// job runs it). `formal-verify` and its two stages are the scheduled Apalache lane, outside
/// `check`. Stages that later tasks bring (fuzz, mutation, release) are absent
/// until those tasks register them.
pub const CATALOG: &[Check] = &[
    row(
        "fmt-check",
        "static",
        "cargo fmt --all --check",
        "FND-02",
        true,
    ),
    row(
        "clippy",
        "static",
        "cargo clippy --locked --workspace --all-targets -- -D warnings",
        "FND-02",
        true,
    ),
    row(
        "cross-check",
        "static",
        "cargo clippy --locked --workspace --all-targets --target aarch64-apple-darwin -- -D warnings (after a rustup target preflight; the C sources of bundled SQLite and aws-lc not compiled)",
        "FND-04",
        true,
    ),
    row("docs", "static", "cargo xtask adr index", "ARCH-01", true),
    row(
        "unit-tests",
        "unit",
        "cargo nextest run --workspace --locked --profile ci -E not kind(test)",
        "FND-05",
        true,
    ),
    row(
        "integration-tests",
        "integration",
        "cargo nextest run --workspace --locked --profile ci -E kind(test)",
        "FND-05",
        true,
    ),
    row(
        "doctests",
        "unit",
        "cargo test --workspace --locked --doc",
        "FND-02",
        true,
    ),
    row("fmt", "tooling", "cargo xtask fmt", "FND-02", false),
    row("quick", "gate", "cargo xtask quick", "FND-02", false),
    row(
        "architecture",
        "architecture",
        "cargo xtask architecture",
        "FND-02",
        true,
    ),
    row("test", "integration", "cargo xtask test", "FND-02", false),
    row(
        "platform-tests",
        "integration",
        "cargo xtask test --only platform-tests",
        "FND-04",
        false,
    ),
    row(
        "real-tart",
        "integration",
        "cargo xtask test --only real-tart --real-tart-home <DIR>",
        "DP-04",
        false,
    ),
    row(
        "formal-typecheck",
        "formal",
        "npx --prefix formal --no -- quint typecheck <every formal/*.qnt>",
        "FND-06",
        true,
    ),
    row(
        "formal-test",
        "formal",
        "npx --prefix formal --no -- quint test --backend typescript <every model of MODELS>",
        "FND-06",
        true,
    ),
    row(
        "formal-freshness",
        "formal",
        "regenerate every trace of formal/traces/regression/manifest.toml with the locked Quint and compare it with the committed file",
        "VER-01",
        true,
    ),
    row(
        "conformance-replay",
        "conformance",
        "quint run --mbt: 10 traces of 25 steps per relation of each driven model under a printed seed, then cargo run -p mayfly-conformance over them and the regression manifest",
        "VER-01",
        true,
    ),
    row(
        "conformance",
        "conformance",
        "cargo xtask conformance [--model M] [--seed S] [--traces K] [--steps N]",
        "VER-01",
        false,
    ),
    row(
        "formal-verify",
        "formal",
        "cargo xtask formal-verify",
        "FND-06",
        false,
    ),
    row(
        "formal-verify-bounded",
        "formal",
        "npx --prefix formal --no -- quint verify --step <relation> --invariants <all> --max-steps <depth> --apalache-version <pinned> formal/<model>.qnt",
        "FND-06",
        false,
    ),
    row(
        "formal-verify-simulate",
        "formal",
        "npx --prefix formal --no -- quint run --backend typescript --step <relation> --invariants <all> --max-samples 5000 --max-steps 50 --seed <seed 1..3> formal/<model>.qnt",
        "FND-06",
        false,
    ),
    row(
        "coverage",
        "coverage",
        "cargo llvm-cov nextest --workspace --locked --profile ci --lcov --output-path target/coverage/lcov.info, then coverage-floor.toml",
        "FND-05",
        true,
    ),
    row(
        "changed-lines",
        "coverage",
        "changed lines of crates/*/src since the merge base, against coverage-floor.toml (reuses a fresh target/coverage/lcov.info)",
        "FND-05",
        true,
    ),
    row(
        "security",
        "security",
        "cargo xtask security",
        "FND-02",
        false,
    ),
    row(
        "deny",
        "security",
        "cargo deny --locked check",
        "FND-02",
        true,
    ),
    row(
        "secrets",
        "security",
        "gitleaks git --no-banner --redact over the full history (after a git preflight)",
        "FND-02",
        true,
    ),
    row("workflows", "security", "actionlint", "FND-04", true),
    row(
        "action-pins",
        "security",
        "check .github/workflows and .github/actions for unpinned uses: and container or service images",
        "FND-02",
        true,
    ),
    row("check", "gate", "cargo xtask check", "FND-02", false),
    row(
        "list-checks",
        "tooling",
        "cargo xtask list-checks",
        "FND-02",
        false,
    ),
    row("doctor", "tooling", "cargo xtask doctor", "FND-02", false),
    row(
        "install-tools",
        "tooling",
        "cargo xtask install-tools",
        "FND-02",
        false,
    ),
    row(
        "adr",
        "tooling",
        "cargo xtask adr new <title> [--number N] [--date YYYY-MM-DD] [--status TEXT] | cargo xtask adr index [--fix]",
        "FND-02",
        false,
    ),
    row(
        "land",
        "tooling",
        "cargo xtask land <ref>... (--push <remote> <branch> [--also <branch>]... | --no-push) [--log <path>]",
        "FND-02",
        false,
    ),
];

/// Every catalog row, then one row per architecture rule id: each runs in the `architecture`
/// stage, owned by the task that introduced it. `list-checks` prints these.
pub fn all_rows() -> Vec<Check> {
    let mut rows = CATALOG.to_vec();
    rows.extend(crate::architecture::Rule::ALL.iter().map(|rule| {
        row(
            rule.id(),
            "architecture",
            "cargo xtask architecture",
            rule.owner(),
            true,
        )
    }));
    rows
}

/// Finds a row by id (tests: every command is implemented, so `main` no longer looks owners up).
#[cfg(test)]
pub fn find(id: &str) -> Option<&'static Check> {
    CATALOG.iter().find(|c| c.id == id)
}

/// Renders the catalog as a JSON array of objects.
pub fn to_json(checks: &[Check]) -> String {
    let rows: Vec<String> = checks
        .iter()
        .map(|c| {
            format!(
                "  {{\"id\": {}, \"level\": {}, \"command\": {}, \"owner\": {}, \"in_check\": {}}}",
                json_str(c.id),
                json_str(c.level),
                json_str(c.command),
                json_str(c.owner),
                c.in_check
            )
        })
        .collect();
    format!("[\n{}\n]\n", rows.join(",\n"))
}

pub(crate) fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::Cli;
    use crate::process::Action;
    use clap::CommandFactory;
    use std::collections::HashSet;

    #[test]
    fn should_have_unique_ids_when_catalog_is_listed() {
        // given
        let mut seen = HashSet::new();

        // when
        let duplicates: Vec<_> = CATALOG.iter().filter(|c| !seen.insert(c.id)).collect();

        // then
        assert!(
            duplicates.is_empty(),
            "duplicate catalog ids: {duplicates:?}"
        );
    }

    #[test]
    fn should_list_every_subcommand_when_catalog_is_checked() {
        // given
        let cmd = Cli::command();

        // when
        let missing: Vec<_> = cmd
            .get_subcommands()
            .map(|s| s.get_name())
            .filter(|n| find(n).is_none())
            .collect();

        // then
        assert!(
            missing.is_empty(),
            "subcommands missing from catalog: {missing:?}"
        );
    }

    #[test]
    fn should_list_every_architecture_rule_when_checks_are_listed() {
        // given
        let rules = crate::architecture::Rule::ALL;

        // when
        let json = to_json(&all_rows());

        // then
        let mut seen = HashSet::new();
        assert!(all_rows().iter().all(|c| seen.insert(c.id)), "{json}");
        for rule in rules {
            let line = format!("\"id\": \"{}\"", rule.id());
            assert!(json.contains(&line), "{line} missing from {json}");
            assert!(
                ["FND-", "ARCH-", "KER-", "CP-", "ES-", "SEC-"]
                    .iter()
                    .any(|task| rule.owner().starts_with(task)),
                "{}",
                rule.owner()
            );
        }
    }

    #[test]
    fn should_escape_quotes_when_rendering_json() {
        // given
        let check = row("a", "b", "say \"hi\"\\", "T-1", true);

        // when
        let json = to_json(&[check]);

        // then
        assert!(json.contains(r#""command": "say \"hi\"\\""#), "{json}");
    }

    #[test]
    fn should_show_the_command_check_runs_when_a_stage_is_listed() {
        // given
        let stages = crate::check::stages();

        // when
        let mismatched: Vec<_> = stages
            .iter()
            .filter_map(|stage| {
                let shown = match &stage.action {
                    Action::Command { program, args, .. } => std::iter::once(program.as_str())
                        .chain(args.iter().map(String::as_str))
                        .collect::<Vec<_>>()
                        .join(" "),
                    Action::InProcess { display, .. } => (*display).to_owned(),
                };
                let listed = find(stage.name).map(|c| c.command);
                (listed != Some(shown.as_str())).then_some((stage.name, listed, shown))
            })
            .collect();

        // then
        assert!(mismatched.is_empty(), "{mismatched:?}");
    }
}
