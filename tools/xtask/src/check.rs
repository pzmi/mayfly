//! `cargo xtask check`: the full blocking gate (foundation.md section 9).
//!
//! It composes the registered stages: the `quick` stages other than its tests (format check,
//! architecture with its fixtures, Clippy), the macOS cross lint, the docs checks, everything
//! `test` runs, the formal models (typecheck and Quint tests), then `security`. A stage
//! that does not exist yet is absent, never skipped; one that cannot run because its pinned
//! tool is missing shows `UNAVAILABLE` and fails the gate.

use std::process::Command;

use crate::process::{Stage, finish, run_report};
use crate::{adr, coverage, formal, quick, repo, security, test, tools};

/// The product's target (D2): linted from Linux so a macOS-only compile error or warning is
/// caught before the native `macos` job.
const DARWIN_TARGET: &str = "aarch64-apple-darwin";

/// Whether `rustup target list --installed` output names `target`.
fn target_installed(list: &str, target: &str) -> bool {
    list.lines().any(|line| line.trim() == target)
}

/// The arguments of the `cross-check` stage: Clippy with warnings denied over every target of
/// the workspace for [`DARWIN_TARGET`], so `#[cfg(target_os = "macos")]` code, which the Linux
/// `clippy` stage never compiles, is linted too (a plain `cargo check` let its warnings
/// through). Pinned by `should_lint_every_target_for_darwin_when_cross_check_is_built`.
const CROSS_CHECK_ARGS: &[&str] = &[
    "clippy",
    "--locked",
    "--workspace",
    "--all-targets",
    "--target",
    DARWIN_TARGET,
    "--",
    "-D",
    "warnings",
];

/// The `cross-check` stage body: `cargo` with [`CROSS_CHECK_ARGS`]. A missing rustup target
/// fails with the command that installs it, never a skip (D15); this is a preflight rather
/// than cargo's own "can't find crate for `core`" so the fix is obvious. Pinned by
/// `should_detect_an_installed_target_when_rustup_lists_it`.
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
        .args(CROSS_CHECK_ARGS)
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

/// The `docs` stage body: the ADR index matches the ADR files (foundation.md section 10.3),
/// through the same code as `cargo xtask adr index`.
fn run_docs() -> i32 {
    adr::run_index(false)
}

/// The stages `check` runs, in order. Their names are catalog ids with `in_check` set.
pub fn stages() -> Vec<Stage> {
    let mut stages: Vec<Stage> = quick::stages()
        .into_iter()
        .filter(|s| !matches!(s.name, "unit-tests" | "doctests"))
        .collect();
    stages.push(Stage::in_process(
        "cross-check",
        "cargo clippy --locked --workspace --all-targets --target aarch64-apple-darwin -- -D warnings (after a rustup target preflight)",
        run_cross_check,
    ));
    stages.push(Stage::in_process("docs", "cargo xtask adr index", run_docs));
    stages.extend(test::stages());
    stages.extend(formal::stages());
    stages.push(coverage::coverage_stage());
    stages.push(coverage::changed_lines_stage());
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
            "docs",
            "unit-tests",
            "integration-tests",
            "doctests",
            "formal-typecheck",
            "formal-test",
            "coverage",
            "changed-lines",
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

    /// Reads the job table of a workflow with the YAML parser `action_pins` uses (the tests
    /// below read `.github/workflows/ci.yml` through it and keep its jobs in lockstep with the
    /// catalog), so quoting, flow mappings and comments mean what they mean to GitHub. Every job
    /// but the `required` aggregate is a gate job, and every step in it is a gate step (the
    /// macOS build, `doctor`, a tool install, not only `cargo xtask check --only`): a step with
    /// `continue-on-error` or its own `if`, or a job with either key, is listed in `softened`,
    /// because the gate could then pass without that step having run or passed. A workflow that
    /// does not parse panics: these are tests.
    fn parse_ci(text: &str) -> ParsedCi {
        use yaml_rust2::{Yaml, YamlLoader};

        fn render(value: &Yaml) -> String {
            match value {
                Yaml::String(s) | Yaml::Real(s) => s.clone(),
                Yaml::Boolean(b) => b.to_string(),
                Yaml::Integer(i) => i.to_string(),
                other => format!("{other:?}"),
            }
        }
        fn soft_keys(map: &Yaml) -> Vec<String> {
            map.as_hash()
                .into_iter()
                .flatten()
                .filter_map(|(key, value)| {
                    let key = key.as_str()?;
                    matches!(key, "continue-on-error" | "if")
                        .then(|| format!("{key}: {}", render(value)))
                })
                .collect()
        }

        let mut out = ParsedCi::default();
        let soften = |out: &mut ParsedCi, note: String| {
            if !out.softened.contains(&note) {
                out.softened.push(note);
            }
        };
        let docs = YamlLoader::load_from_str(text).expect("workflow parses as YAML");
        let Some(jobs) = docs.first().and_then(|d| d["jobs"].as_hash()) else {
            return out;
        };
        for (id, body) in jobs {
            let job = id.as_str().expect("job ids are strings").to_owned();
            let gate_job = job != "required";
            let needs: Vec<String> = match &body["needs"] {
                Yaml::String(one) => vec![one.clone()],
                Yaml::Array(many) => many
                    .iter()
                    .filter_map(Yaml::as_str)
                    .map(str::to_owned)
                    .collect(),
                _ => Vec::new(),
            };
            out.needs.insert(job.clone(), needs);
            let mut stages = Vec::new();
            if gate_job {
                // A job-level `if:` can skip the whole job (a skipped job satisfies `needs`).
                for key in soft_keys(body) {
                    soften(&mut out, format!("{job}: a gate job has `{key}`"));
                }
            }
            for step in body["steps"].as_vec().into_iter().flatten() {
                if gate_job {
                    for key in soft_keys(step) {
                        soften(&mut out, format!("{job}: a gate step has `{key}`"));
                    }
                }
                let run = step["run"].as_str().unwrap_or_default();
                for line in run.lines() {
                    if let Some(args) = line.split("cargo xtask check --only ").nth(1) {
                        // `--only a,b` and `--only a --only b` both name stages
                        let args = args.replace("--only ", "");
                        stages.extend(
                            args.split(|c: char| c == ',' || c.is_whitespace())
                                .filter(|n| !n.is_empty())
                                .map(str::to_owned),
                        );
                    }
                }
            }
            out.stages.insert(job, stages);
        }
        out
    }

    #[test]
    fn should_flag_a_gate_step_when_its_keys_are_quoted() {
        // given
        let text = "jobs:\n  a:\n    steps:\n      - name: Gate\n        \"continue-on-error\": true\n        'run': cargo xtask check --only clippy\n      - 'if': always()\n        run: cargo xtask doctor\n";

        // when
        let parsed = parse_ci(text);

        // then
        assert_eq!(parsed.stages["a"], ["clippy"]);
        assert_eq!(
            parsed.softened,
            [
                "a: a gate step has `continue-on-error: true`",
                "a: a gate step has `if: always()`"
            ]
        );
    }

    #[test]
    fn should_flag_a_gate_step_when_it_is_a_flow_mapping() {
        // given
        let text = "jobs:\n  a: { continue-on-error: true, steps: [ { run: cargo xtask check --only clippy, if: failure() } ] }\n  required: { needs: [a], steps: [ { run: echo ok } ] }\n";

        // when
        let parsed = parse_ci(text);

        // then
        assert_eq!(parsed.stages["a"], ["clippy"]);
        assert_eq!(parsed.needs["required"], ["a"]);
        assert_eq!(
            parsed.softened,
            [
                "a: a gate job has `continue-on-error: true`",
                "a: a gate step has `if: failure()`"
            ]
        );
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
            ["a: a gate job has `if: github.event_name == 'push'`"]
        );
        assert_eq!(b.softened, ["b: a gate job has `if: always()`"]);
        assert!(c.softened.is_empty(), "{:?}", c.softened);
    }

    #[test]
    fn should_flag_every_step_of_a_gate_job_when_it_has_an_if_or_continue_on_error() {
        // given: the macOS build and `doctor` gate as much as `check --only` does
        let text = "jobs:\n  macos:\n    steps:\n      - name: Build\n        if: failure()\n        run: cargo build\n      - name: Pinned tools\n        continue-on-error: true\n        run: cargo xtask doctor\n      - run: cargo xtask test --only platform-tests\n";

        // when
        let parsed = parse_ci(text);

        // then
        assert_eq!(
            parsed.softened,
            [
                "macos: a gate step has `if: failure()`",
                "macos: a gate step has `continue-on-error: true`"
            ]
        );
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
    fn should_lint_every_target_for_darwin_when_cross_check_is_built() {
        // given
        let args = CROSS_CHECK_ARGS.join(" ");

        // when
        let listed = catalog::find("cross-check").map(|c| c.command);

        // then
        assert_eq!(
            args,
            "clippy --locked --workspace --all-targets --target aarch64-apple-darwin -- -D warnings"
        );
        assert!(listed.is_some_and(|l| l.contains(&args)), "{listed:?}");
    }

    #[test]
    fn should_keep_main_runs_when_ci_concurrency_is_read() {
        // given
        let text = ci_text();

        // when
        let block: Vec<&str> = text
            .lines()
            .skip_while(|l| *l != "concurrency:")
            .take(3)
            .collect();

        // then: every push to main is verified, so a newer push never cancels or replaces a
        // pending main run; pull requests share a group per ref and cancel stale runs
        assert_eq!(
            block,
            [
                "concurrency:",
                "  group: ci-${{ github.ref }}-${{ github.ref == 'refs/heads/main' && github.sha || 'latest' }}",
                "  cancel-in-progress: ${{ github.event_name == 'pull_request' }}",
            ]
        );
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
        assert!(macos.contains("RUSTFLAGS: \"-D warnings\""), "{macos}");
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
