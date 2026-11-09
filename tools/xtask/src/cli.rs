//! Command-line surface of `cargo xtask` (foundation.md section 9), as far as the foundation
//! tasks have built it; later tasks add their commands here and to the catalog together.

use std::path::PathBuf;

use clap::{ArgGroup, Parser, Subcommand};

/// Repository automation: gate, architecture checker and tool management.
#[derive(Debug, Parser)]
#[command(name = "xtask", version)]
pub struct Cli {
    /// The command to run.
    #[command(subcommand)]
    pub command: Command,
}

/// Every command of the surface. Names are the kebab-case variant names.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Apply formatting.
    Fmt,
    /// Formatter check, architecture policy, Clippy and fast tests. Not equivalent to `check`.
    Quick,
    /// Crate and module policy plus negative fixtures.
    Architecture {
        /// Check only the workspace with this root manifest (no fixtures); its root holds
        /// `architecture.toml` and `Cargo.lock`.
        #[arg(long, value_name = "PATH")]
        manifest_path: Option<PathBuf>,
    },
    /// The ordinary test levels: unit, integration and doctests.
    Test {
        /// Run only this stage (a `test` stage id such as `browser`), not the others.
        #[arg(long, value_name = "STAGE")]
        only: Option<String>,
    },
    /// Dependency policy, secret scan and action pins.
    Security,
    /// The full blocking PR-quality gate.
    Check {
        /// Run only these stages (catalog ids such as `clippy`), in the gate's order and
        /// through the same stage code as the full run. Repeat or separate with commas. An
        /// unknown name exits 2 and lists the stage names.
        #[arg(long, value_name = "STAGE", action = clap::ArgAction::Append, value_delimiter = ',')]
        only: Vec<String>,
    },
    /// Print the check catalog as JSON.
    ListChecks,
    /// Report every pinned tool a `check` stage needs: found, missing or at the wrong version,
    /// with its install hint. Exits 1 when any is missing or wrong.
    Doctor {
        /// Print the report as a JSON array instead of one line per tool.
        #[arg(long)]
        json: bool,
    },
    /// Install pinned dev tools.
    InstallTools {
        /// Tools to install; all when empty.
        tools: Vec<String>,
    },
    /// Architecture decision records: allocate a new one, or check and fix the index.
    Adr {
        /// What to do.
        #[command(subcommand)]
        action: AdrAction,
    },
    /// Cherry-pick commits onto a clean, current `main`, run `check`, push only when green.
    #[command(group(ArgGroup::new("target").required(true).args(["push", "no_push"])))]
    Land {
        /// Commits to land, in order; `a..b` lands a range.
        #[arg(required = true, value_name = "REF")]
        refs: Vec<String>,
        /// Push the landed branch to this remote and branch (the checkout must be on BRANCH).
        #[arg(long, num_args = 2, value_names = ["REMOTE", "BRANCH"])]
        push: Option<Vec<String>>,
        /// Also set this remote branch to the landed commit. Repeatable.
        #[arg(
            long,
            value_name = "BRANCH",
            requires = "push",
            conflicts_with = "no_push"
        )]
        also: Vec<String>,
        /// Land on the local `main` only (its base is `origin/main`).
        #[arg(long)]
        no_push: bool,
        /// Write the output of `cargo xtask check` to this file instead of the terminal.
        #[arg(long, value_name = "PATH")]
        log: Option<PathBuf>,
    },
}

/// `cargo xtask adr` actions.
#[derive(Debug, Subcommand)]
pub enum AdrAction {
    /// Create `doc/adr/NNNN-<slug>.md` from the template and insert its index row.
    New {
        /// The decision's title.
        title: String,
        /// Use this number (reserved by an orchestrator) instead of the next free one.
        #[arg(long, value_name = "N", value_parser = clap::value_parser!(u16).range(1..=9999))]
        number: Option<u16>,
        /// The record's date, YYYY-MM-DD (default: today, UTC).
        #[arg(long, value_name = "DATE")]
        date: Option<String>,
        /// The status section's text.
        #[arg(long, value_name = "TEXT", default_value = "Proposed")]
        status: String,
    },
    /// Check that the index rows match the ADR files; `--fix` rewrites them.
    Index {
        /// Rewrite the rows: sorted, one per file, titles and statuses from the files.
        #[arg(long)]
        fix: bool,
    },
}

impl Command {
    /// The subcommand name, which is also the catalog id (tests).
    #[cfg(test)]
    pub fn id(&self) -> &'static str {
        match self {
            Self::Fmt => "fmt",
            Self::Quick => "quick",
            Self::Architecture { .. } => "architecture",
            Self::Test { .. } => "test",
            Self::Security => "security",
            Self::Check { .. } => "check",
            Self::ListChecks => "list-checks",
            Self::Doctor { .. } => "doctor",
            Self::InstallTools { .. } => "install-tools",
            Self::Adr { .. } => "adr",
            Self::Land { .. } => "land",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn should_match_clap_names_when_ids_are_derived() {
        // given
        let parsed = Cli::try_parse_from(["xtask", "list-checks"]).unwrap();

        // when
        let id = parsed.command.id();

        // then
        assert_eq!(id, "list-checks");
    }

    #[test]
    fn should_require_an_explicit_target_when_land_is_parsed() {
        // given
        let parse = |args: &[&str]| {
            Cli::try_parse_from(["xtask", "land"].iter().chain(args)).map(|c| c.command)
        };

        // when
        let none = parse(&["abc"]);
        let both = parse(&["abc", "--no-push", "--push", "origin", "main"]);
        let also_alone = parse(&["abc", "--no-push", "--also", "lane"]);
        let one_value = parse(&["abc", "--push", "origin"]);
        let full = parse(&[
            "a", "b", "--push", "origin", "main", "--also", "x", "--also", "y",
        ]);

        // then
        assert!(none.is_err() && both.is_err() && also_alone.is_err() && one_value.is_err());
        assert!(matches!(
            full.unwrap(),
            Command::Land { refs, push: Some(push), also, no_push: false, log: None }
                if refs == ["a", "b"] && push == ["origin", "main"] && also == ["x", "y"]
        ));
    }

    #[test]
    fn should_parse_adr_actions_when_new_and_index_are_given() {
        // given
        let new = [
            "xtask",
            "adr",
            "new",
            "A title",
            "--number",
            "81",
            "--date",
            "2026-10-14",
        ];
        let fix = ["xtask", "adr", "index", "--fix"];

        // when
        let (new, fix) = (
            Cli::try_parse_from(new).unwrap(),
            Cli::try_parse_from(fix).unwrap(),
        );

        // then
        assert!(matches!(
            new.command,
            Command::Adr { action: AdrAction::New { ref title, number: Some(81), date: Some(ref d), ref status } }
                if title == "A title" && d == "2026-10-14" && status == "Proposed"
        ));
        assert!(matches!(
            fix.command,
            Command::Adr {
                action: AdrAction::Index { fix: true }
            }
        ));
        assert!(Cli::try_parse_from(["xtask", "adr", "new", "T", "--number", "0"]).is_err());
    }

    #[test]
    fn should_have_valid_definition_when_clap_asserts_it() {
        // given
        let command = Cli::command();

        // when
        let name = command.get_name().to_owned();

        // then
        command.debug_assert();
        assert_eq!(name, "xtask");
    }
}
