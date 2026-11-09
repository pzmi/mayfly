//! `cargo xtask check`: the full blocking gate (foundation.md section 9).
//!
//! It composes the registered stages: the `quick` stages other than its tests (format check,
//! architecture with its fixtures, Clippy), everything `test` runs, then `security`. A stage
//! that does not exist yet is absent, never skipped; one that cannot run because its pinned
//! tool is missing shows `UNAVAILABLE` and fails the gate.

use crate::process::{Stage, finish, run_report};
use crate::{quick, security, test, tools};

/// The stages `check` runs, in order. Their names are catalog ids with `in_check` set.
pub fn stages() -> Vec<Stage> {
    let mut stages: Vec<Stage> = quick::stages()
        .into_iter()
        .filter(|s| !matches!(s.name, "unit-tests" | "doctests"))
        .collect();
    stages.extend(test::stages());
    stages.extend(security::stages());
    stages
}

/// The stages to run: all, or those named by `--only`, in the gate's own order (so `clippy`
/// precedes `unit-tests` however the flags are ordered). An unknown name is an error that
/// lists the valid ones, never an empty green run.
fn select(all: Vec<Stage>, only: &[String]) -> Result<Vec<Stage>, String> {
    if only.is_empty() {
        return Ok(all);
    }
    let unknown: Vec<_> = only
        .iter()
        .filter(|name| !all.iter().any(|s| s.name == name.as_str()))
        .map(String::as_str)
        .collect();
    if !unknown.is_empty() {
        let names: Vec<_> = all.iter().map(|s| s.name).collect();
        return Err(format!(
            "no check stage named {}; choose from {}",
            unknown.join(", "),
            names.join(", ")
        ));
    }
    Ok(all
        .into_iter()
        .filter(|s| only.iter().any(|name| name == s.name))
        .collect())
}

/// Runs `check` (or the stages named by `--only`), prints the stage table and returns the
/// exit code: 2 for an unknown stage name.
pub fn run(only: &[String]) -> i32 {
    match select(stages(), only) {
        Ok(stages) => finish(&run_report(&stages, &tools::require)),
        Err(err) => {
            eprintln!("xtask: {err}");
            2
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog;
    use std::collections::{BTreeMap, BTreeSet};

    #[test]
    fn should_run_exactly_the_in_check_stages_when_check_is_built() {
        // given
        let built: BTreeSet<_> = stages().iter().map(|s| s.name).collect();

        // when
        let registered: BTreeSet<_> = catalog::CATALOG
            .iter()
            .filter(|c| c.in_check)
            .map(|c| c.id)
            .collect();

        // then
        assert_eq!(built, registered);
    }

    #[test]
    fn should_run_each_stage_once_when_check_is_built() {
        // given
        let stages = stages();

        // when
        let mut names: Vec<_> = stages.iter().map(|s| s.name).collect();
        let total = names.len();
        names.sort_unstable();
        names.dedup();

        // then
        assert_eq!(names.len(), total);
    }

    #[test]
    fn should_include_architecture_and_tests_when_check_is_built() {
        // given
        let stages = stages();

        // when
        let names: Vec<_> = stages.iter().map(|s| s.name).collect();

        // then
        for want in [
            "fmt-check",
            "architecture",
            "clippy",
            "unit-tests",
            "integration-tests",
            "doctests",
            "deny",
            "secrets",
            "action-pins",
        ] {
            assert!(names.contains(&want), "{want} missing from {names:?}");
        }
    }

    #[test]
    fn should_keep_every_stage_in_check_order_when_only_names_some() {
        // given
        let only = ["unit-tests".to_owned(), "clippy".to_owned()];

        // when
        let picked = select(stages(), &only).unwrap();

        // then
        let names: Vec<_> = picked.iter().map(|s| s.name).collect();
        assert_eq!(names, ["clippy", "unit-tests"]);
    }

    #[test]
    fn should_run_every_stage_when_only_is_empty() {
        // given
        let total = stages().len();

        // when
        let picked = select(stages(), &[]).unwrap();

        // then
        assert_eq!(picked.len(), total);
    }

    #[test]
    fn should_list_valid_stages_when_only_names_an_unknown_one() {
        // given
        let only = ["deny".to_owned(), "nope".to_owned()];

        // when
        let err = select(stages(), &only).unwrap_err();

        // then
        assert!(
            err.contains("nope") && err.contains("clippy") && err.contains("deny"),
            "{err}"
        );
    }

    #[test]
    fn should_exit_two_when_only_names_an_unknown_stage() {
        // given
        let only = ["nope".to_owned()];

        // when
        let code = run(&only);

        // then
        assert_eq!(code, 2);
    }

    #[test]
    fn should_parse_only_flags_repeated_and_comma_separated_when_check_is_invoked() {
        use crate::cli::{Cli, Command};
        use clap::Parser;

        // given
        let args = ["xtask", "check", "--only", "a,b", "--only", "c"];

        // when
        let cli = Cli::try_parse_from(args).unwrap();

        // then
        let Command::Check { only } = cli.command else {
            panic!("not check")
        };
        assert_eq!(only, ["a", "b", "c"]);
    }

    type JobMap = BTreeMap<String, Vec<String>>;

    #[derive(Default)]
    struct ParsedCi {
        stages: JobMap,
        needs: JobMap,
        /// Gate steps (or their jobs) that can be red and still count as green.
        softened: Vec<String>,
    }

    /// Reads the job table of a workflow without a YAML parser (FND-04 adds the tests that read
    /// `.github/workflows/ci.yml` through it and keep its jobs in lockstep with the catalog): job ids at two spaces, steps as
    /// `      - ` items. Comment lines are ignored, so prose naming the command counts for
    /// nothing. A step that runs `cargo xtask check --only` with `continue-on-error` or its own
    /// `if:`, or in a job that has `continue-on-error` or a job-level `if:` (four spaces), is listed in `softened`: the gate would
    /// pass without having run (or passed) that stage.
    fn parse_ci(text: &str) -> ParsedCi {
        let mut out = ParsedCi::default();
        let mut in_jobs = false;
        let mut job = String::new();
        let mut in_needs = false;
        let mut job_soft = false;
        let mut job_if = false;
        let mut job_gate = false;
        let mut step_soft = false;
        let mut step_gate = false;
        for line in text.lines() {
            if line.trim_start().starts_with('#') {
                continue;
            }
            if line == "jobs:" {
                in_jobs = true;
            } else if in_jobs && !line.starts_with(' ') && !line.is_empty() {
                in_jobs = false;
            }
            if !in_jobs {
                continue;
            }
            if let Some(id) = line
                .strip_prefix("  ")
                .and_then(|l| l.strip_suffix(':'))
                .filter(|l| !l.starts_with([' ', '#']))
            {
                job = id.to_owned();
                out.stages.entry(job.clone()).or_default();
                out.needs.entry(job.clone()).or_default();
                in_needs = false;
                job_soft = false;
                job_if = false;
                job_gate = false;
                step_soft = false;
                step_gate = false;
                continue;
            }
            let is_step_start = line.starts_with("      - ");
            if is_step_start {
                step_soft = false;
                step_gate = false;
            }
            // A step's own keys: the item line itself (`      - if:`) or eight spaces in.
            let key = if is_step_start {
                line.trim_start().trim_start_matches("- ")
            } else if line.starts_with("        ") && !line.starts_with("         ") {
                line.trim_start()
            } else {
                ""
            };
            let soft_key = key.starts_with("continue-on-error:") || key.starts_with("if:");
            if soft_key {
                step_soft = true;
            }
            if line.starts_with("    continue-on-error:") {
                job_soft = true;
            }
            if line.starts_with("    if:") {
                job_if = true;
            }
            if let Some(args) = line.split("cargo xtask check --only ").nth(1) {
                step_gate = true;
                job_gate = true;
                // `--only a,b` and `--only a --only b` both name stages
                let args = args.replace("--only ", "");
                let list = out.stages.get_mut(&job).unwrap();
                list.extend(
                    args.split(|c: char| c == ',' || c.is_whitespace())
                        .filter(|n| !n.is_empty())
                        .map(str::to_owned),
                );
            } else if line.trim() == "needs:" {
                in_needs = true;
            } else if in_needs && let Some(name) = line.trim().strip_prefix("- ") {
                out.needs.get_mut(&job).unwrap().push(name.to_owned());
            } else {
                in_needs = false;
            }
            let note = format!("{job}: a step running `check --only`");
            // A job-level `if:` can skip the whole job (a skipped job satisfies `needs`), wherever
            // in the job it is written relative to the gate step.
            let job_note = format!("{job}: a job running `check --only` has a job-level `if:`");
            if job_gate && job_if && !out.softened.contains(&job_note) {
                out.softened.push(job_note);
            }
            if step_gate && (step_soft || job_soft) && !out.softened.contains(&note) {
                out.softened.push(note);
            }
        }
        out
    }

    #[test]
    fn should_ignore_comment_lines_when_ci_jobs_are_parsed() {
        // given
        let text = "jobs:\n  a:\n    steps:\n      # run: cargo xtask check --only fmt-check\n      - run: cargo xtask check --only clippy\n";

        // when
        let parsed = parse_ci(text);

        // then
        assert_eq!(parsed.stages["a"], ["clippy"]);
    }

    #[test]
    fn should_flag_a_gate_step_when_it_has_continue_on_error() {
        // given
        let text = "jobs:\n  a:\n    steps:\n      - name: Gate\n        continue-on-error: true\n        run: cargo xtask check --only clippy\n";

        // when
        let parsed = parse_ci(text);

        // then
        assert_eq!(parsed.softened.len(), 1, "{:?}", parsed.softened);
    }

    #[test]
    fn should_flag_a_gate_step_when_its_own_if_follows_the_run() {
        // given
        let text = "jobs:\n  a:\n    steps:\n      - run: cargo xtask check --only clippy\n        if: github.event_name == 'push'\n";

        // when
        let parsed = parse_ci(text);

        // then
        assert_eq!(parsed.softened.len(), 1, "{:?}", parsed.softened);
    }

    #[test]
    fn should_flag_a_gate_step_when_the_if_is_on_the_item_line() {
        // given
        let text = "jobs:\n  a:\n    steps:\n      - if: always()\n        run: cargo xtask check --only clippy\n";

        // when
        let parsed = parse_ci(text);

        // then
        assert_eq!(parsed.softened.len(), 1, "{:?}", parsed.softened);
    }

    #[test]
    fn should_flag_a_gate_step_when_its_job_has_continue_on_error() {
        // given
        let text = "jobs:\n  a:\n    continue-on-error: true\n    steps:\n      - run: cargo xtask check --only clippy\n";

        // when
        let parsed = parse_ci(text);

        // then
        assert_eq!(parsed.softened.len(), 1, "{:?}", parsed.softened);
    }

    #[test]
    fn should_flag_a_gate_job_when_it_has_a_job_level_if() {
        // given
        let before = "jobs:\n  a:\n    if: github.event_name == 'push'\n    steps:\n      - run: cargo xtask check --only clippy\n";
        let after = "jobs:\n  a:\n    steps:\n      - run: cargo xtask check --only clippy\n  b:\n    if: always()\n    steps:\n      - run: cargo xtask check --only fmt\n";
        let no_gate = "jobs:\n  required:\n    if: always()\n    steps:\n      - run: echo ok\n";

        // when
        let (a, b, c) = (parse_ci(before), parse_ci(after), parse_ci(no_gate));

        // then
        assert_eq!(
            a.softened,
            ["a: a job running `check --only` has a job-level `if:`"]
        );
        assert_eq!(
            b.softened,
            ["b: a job running `check --only` has a job-level `if:`"]
        );
        assert!(c.softened.is_empty(), "{:?}", c.softened);
    }

    #[test]
    fn should_not_flag_other_steps_when_a_neighbour_has_an_if() {
        // given
        let text = "jobs:\n  a:\n    steps:\n      - name: Keep\n        if: failure()\n        run: echo\n      - run: cargo xtask check --only clippy\n";

        // when
        let parsed = parse_ci(text);

        // then
        assert!(parsed.softened.is_empty(), "{:?}", parsed.softened);
    }

    #[test]
    fn should_read_needs_when_the_required_job_lists_them() {
        // given
        let text = "jobs:\n  a:\n    steps:\n      - run: cargo xtask check --only clippy\n  required:\n    needs:\n      - a\n    steps:\n      - run: echo ok\n";

        // when
        let parsed = parse_ci(text);

        // then
        assert_eq!(parsed.needs["required"], ["a"]);
        assert_eq!(parsed.stages["a"], ["clippy"]);
    }
}
