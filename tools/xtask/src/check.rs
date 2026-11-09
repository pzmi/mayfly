//! `cargo xtask check`: the full blocking gate (foundation.md section 9).
//!
//! It composes the registered stages: the `quick` stages other than its tests (format check,
//! architecture with its fixtures, Clippy), the macOS cross check, everything `test` runs, then
//! `security`. A stage
//! that does not exist yet is absent, never skipped; one that cannot run because its pinned
//! tool is missing shows `UNAVAILABLE` and fails the gate.

use std::process::Command;

use crate::process::{Stage, finish, run_report};
use crate::{quick, repo, security, test, tools};

/// The product's target (D2): checked from Linux so a macOS-only compile error is caught before
/// the native `macos` job.
const DARWIN_TARGET: &str = "aarch64-apple-darwin";

/// Whether `rustup target list --installed` output names `target`.
fn target_installed(list: &str, target: &str) -> bool {
    list.lines().any(|line| line.trim() == target)
}

/// The `cross-check` stage body: `cargo check` of the whole workspace for [`DARWIN_TARGET`].
/// A missing rustup target fails with the command that installs it, never a skip (D15); this is
/// a preflight rather than cargo's own "can't find crate for `core`" so the fix is obvious.
/// Pinned by `should_detect_an_installed_target_when_rustup_lists_it`.
fn run_cross_check() -> i32 {
    let root = repo::root();
    let listed = Command::new("rustup")
        .args(["target", "list", "--installed"])
        .current_dir(&root)
        .output();
    match listed {
        Ok(out) if target_installed(&String::from_utf8_lossy(&out.stdout), DARWIN_TARGET) => {}
        Ok(_) => {
            eprintln!(
                "xtask: cross-check: target {DARWIN_TARGET} missing: run 'rustup target add {DARWIN_TARGET}'"
            );
            return 1;
        }
        Err(err) => {
            eprintln!("xtask: cross-check: cannot run rustup: {err}");
            return 1;
        }
    }
    let status = Command::new("cargo")
        .args([
            "check",
            "--locked",
            "--workspace",
            "--target",
            DARWIN_TARGET,
        ])
        .current_dir(&root)
        .status();
    match status {
        Ok(status) => status.code().unwrap_or(1),
        Err(err) => {
            eprintln!("xtask: cross-check: cannot run cargo: {err}");
            1
        }
    }
}

/// The stages `check` runs, in order. Their names are catalog ids with `in_check` set.
pub fn stages() -> Vec<Stage> {
    let mut stages: Vec<Stage> = quick::stages()
        .into_iter()
        .filter(|s| !matches!(s.name, "unit-tests" | "doctests"))
        .collect();
    stages.push(Stage::in_process(
        "cross-check",
        "cargo check --locked --workspace --target aarch64-apple-darwin (after a rustup target preflight)",
        run_cross_check,
    ));
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
            "cross-check",
            "unit-tests",
            "integration-tests",
            "doctests",
            "deny",
            "secrets",
            "workflows",
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

    /// Reads the job table of a workflow without a YAML parser (the tests below read
    /// `.github/workflows/ci.yml` through it and keep its jobs in lockstep with the catalog):
    /// job ids at two spaces, steps as `      - ` items. Comment lines are ignored, so prose naming the command counts for
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

    fn read_repo_file(path: &str) -> String {
        std::fs::read_to_string(repo::root().join(path)).unwrap_or_else(|e| panic!("{path}: {e}"))
    }

    fn ci_text() -> String {
        read_repo_file(".github/workflows/ci.yml")
    }

    /// The lines of job `id` in a workflow, up to the next job header.
    fn job_block(text: &str, id: &str) -> String {
        let header = format!("  {id}:");
        let mut lines = text.lines().skip_while(|l| *l != header);
        let mut block = vec![lines.next().unwrap_or_else(|| panic!("no job {id}"))];
        for line in lines {
            let is_header = line
                .strip_prefix("  ")
                .is_some_and(|l| !l.starts_with([' ', '#']) && l.ends_with(':'));
            if is_header {
                break;
            }
            block.push(line);
        }
        block.join("\n")
    }

    #[test]
    fn should_detect_an_installed_target_when_rustup_lists_it() {
        // given
        let list = "x86_64-unknown-linux-gnu\naarch64-apple-darwin\n";

        // when
        let (found, missing) = (
            target_installed(list, DARWIN_TARGET),
            target_installed("x86_64-unknown-linux-gnu\n", DARWIN_TARGET),
        );

        // then
        assert!(found);
        assert!(!missing);
    }

    #[test]
    fn should_cover_each_in_check_stage_once_when_ci_jobs_are_read() {
        // given
        let parsed = parse_ci(&ci_text());
        let mut count: BTreeMap<&str, usize> = catalog::CATALOG
            .iter()
            .filter(|c| c.in_check)
            .map(|c| (c.id, 0))
            .collect();

        // when
        let mut unknown = Vec::new();
        for (job, names) in &parsed.stages {
            for name in names {
                match count.get_mut(name.as_str()) {
                    Some(n) => *n += 1,
                    None => unknown.push(format!("{job}: {name}")),
                }
            }
        }

        // then
        assert!(unknown.is_empty(), "not in_check stages: {unknown:?}");
        let wrong: Vec<_> = count.iter().filter(|(_, n)| **n != 1).collect();
        assert!(
            wrong.is_empty(),
            "stages not in exactly one ci.yml job: {wrong:?}"
        );
    }

    #[test]
    fn should_need_every_other_job_when_required_job_is_read() {
        // given
        let parsed = parse_ci(&ci_text());

        // when
        let mut want: Vec<_> = parsed
            .stages
            .keys()
            .filter(|j| *j != "required")
            .cloned()
            .collect();
        let mut got = parsed.needs.get("required").cloned().unwrap_or_default();
        want.sort();
        got.sort();

        // then
        assert!(
            parsed.stages.contains_key("required"),
            "{:?}",
            parsed.stages
        );
        assert_eq!(got, want);
    }

    #[test]
    fn should_run_every_gate_step_unconditionally_when_ci_is_read() {
        // given
        let text = ci_text();

        // when
        let parsed = parse_ci(&text);

        // then
        assert!(parsed.softened.is_empty(), "{:?}", parsed.softened);
    }

    #[test]
    fn should_fail_the_required_job_when_a_needed_job_is_not_a_success() {
        // given
        let text = ci_text();

        // when
        let required = job_block(&text, "required");

        // then
        assert!(required.contains("if: ${{ always() }}"), "{required}");
        assert!(
            required.contains("all(.[]; .result == \"success\")"),
            "{required}"
        );
    }

    #[test]
    fn should_run_native_macos_arm64_and_the_darwin_cross_check_when_ci_is_read() {
        // given
        let text = ci_text();
        let parsed = parse_ci(&text);

        // when
        let macos = job_block(&text, "macos");

        // then
        assert!(macos.contains("runs-on: macos-latest"), "{macos}");
        assert!(text.contains("cargo xtask test --only platform-tests"));
        assert_eq!(parsed.stages["cross"], ["cross-check"]);
        assert!(text.contains("rustup target add aarch64-apple-darwin"));
        assert!(
            !text.to_ascii_lowercase().contains("windows"),
            "D2: no Windows"
        );
    }

    #[test]
    fn should_keep_required_checks_doc_in_step_when_ci_jobs_change() {
        // given
        let parsed = parse_ci(&ci_text());
        let doc = read_repo_file("doc/required-checks.md");

        // when
        let rows: BTreeMap<String, String> = doc
            .lines()
            .filter_map(|line| {
                let cells: Vec<_> = line.split('|').map(str::trim).collect();
                let id = cells.get(1)?.strip_prefix('`')?.strip_suffix('`')?;
                Some((id.to_owned(), (*cells.get(3)?).to_owned()))
            })
            .collect();

        // then
        let doc_jobs: Vec<_> = rows.keys().cloned().collect();
        let ci_jobs: Vec<_> = parsed.stages.keys().cloned().collect();
        assert_eq!(doc_jobs, ci_jobs, "doc/required-checks.md job table");
        for (job, stages) in &parsed.stages {
            if stages.is_empty() {
                continue;
            }
            let want = format!("`check --only {}`", stages.join(","));
            assert!(
                rows[job].contains(&want),
                "{job}: want {want}, doc says {}",
                rows[job]
            );
        }
    }

    #[test]
    fn should_follow_workflow_hygiene_when_workflows_are_read() {
        // given
        let dir = repo::root().join(".github/workflows");
        let mut files: Vec<_> = std::fs::read_dir(&dir)
            .expect("workflows directory")
            .map(|e| e.expect("entry").path())
            .filter(|p| p.extension().is_some_and(|e| e == "yml"))
            .collect();
        files.sort();

        // when
        let mut problems = Vec::new();
        for file in &files {
            let name = file.file_name().unwrap().to_string_lossy().into_owned();
            let text = std::fs::read_to_string(file).unwrap();
            let lines: Vec<_> = text.lines().collect();
            if !lines
                .windows(2)
                .any(|w| w[0] == "permissions:" && w[1] == "  contents: read")
            {
                problems.push(format!(
                    "{name}: no top-level `permissions: contents: read`"
                ));
            }
            if text.contains("pull_request_target") {
                problems.push(format!("{name}: pull_request_target"));
            }
            for (i, line) in lines.iter().enumerate() {
                if line.contains("uses: actions/checkout@") {
                    let step_end = lines[i + 1..]
                        .iter()
                        .position(|l| l.starts_with("      - "))
                        .map_or(lines.len(), |n| i + 1 + n);
                    if !lines[i..step_end]
                        .iter()
                        .any(|l| l.trim() == "persist-credentials: false")
                    {
                        problems.push(format!("{name}:{}: checkout keeps credentials", i + 1));
                    }
                }
            }
        }

        // then
        assert!(!files.is_empty(), "no workflows read");
        assert!(problems.is_empty(), "{problems:?}");
    }
}
