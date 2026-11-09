//! `cargo xtask test`: the ordinary test levels of the workspace.
//!
//! Until FND-05 brings the nextest profiles, the runner is `cargo test`: `unit-tests` runs
//! every test compiled into a library or binary (`--lib --bins`), `integration-tests` every
//! `tests/*.rs` binary (`--test '*'`), `doctests` the documentation examples. Together they are
//! the whole workspace run. The stage names are the ones nextest will keep, so CI jobs that
//! name them (`check --only unit-tests`) survive the switch of runner.

use crate::process::{Stage, finish, run_report};
use crate::tools;

/// Generated cases per property in the gate (foundation.md section 5): the floor.
const PROPTEST_CASES_GATE: u32 = 1024;

/// The case count the test stages pass on: the developer's `PROPTEST_CASES` when it raises the
/// floor, otherwise the floor. A lower or unparsable value never weakens the gate. Pinned by
/// `should_keep_developer_proptest_cases_when_set_at_or_above_the_floor`.
pub(crate) fn proptest_cases(developer: Option<&str>) -> String {
    developer
        .and_then(|value| value.trim().parse::<u32>().ok())
        .filter(|cases| *cases >= PROPTEST_CASES_GATE)
        .unwrap_or(PROPTEST_CASES_GATE)
        .to_string()
}

/// The stages `test` runs, in order. Their names are catalog ids.
pub fn stages() -> Vec<Stage> {
    stages_with(std::env::var("PROPTEST_CASES").ok().as_deref())
}

/// A `cargo test` stage over the whole workspace, narrowed by `selection`, with the proptest
/// floor.
fn cargo_test_stage(
    name: &'static str,
    selection: &[&str],
    developer_cases: Option<&str>,
) -> Stage {
    let mut args = vec!["test", "--workspace", "--locked"];
    args.extend(selection);
    Stage::new(name, "cargo", &args).env("PROPTEST_CASES", &proptest_cases(developer_cases))
}

/// [`stages`] with the developer's `PROPTEST_CASES`, if any, given. Pinned by
/// `should_split_the_workspace_run_by_test_kind_when_test_is_built`.
fn stages_with(developer_cases: Option<&str>) -> Vec<Stage> {
    vec![
        cargo_test_stage("unit-tests", &["--lib", "--bins"], developer_cases),
        cargo_test_stage("integration-tests", &["--test", "*"], developer_cases),
        Stage::new(
            "doctests",
            "cargo",
            &["test", "--workspace", "--locked", "--doc"],
        ),
    ]
}

/// The `platform-tests` stage (FND-04): the product crates' unit and integration tests, run
/// natively on macOS arm64 by the `macos` CI job, because the code under test is the operating
/// system's (processes, sockets, signals).
///
/// It is `unit-tests` plus `integration-tests` minus what is not OS behaviour: `xtask`, whose
/// security fixtures need cargo-deny, Gitleaks and actionlint (only the Linux jobs install them)
/// and whose repository-policy tests read the Linux CI definition. It is not part of [`stages`]
/// or of `check`: on Linux the two stages it repeats already run, so a local `check` would run
/// everything twice. Reached only as `cargo xtask test --only platform-tests`. Pinned by
/// `should_run_the_product_crates_natively_when_platform_stage_is_built`.
fn platform_stage(developer_cases: Option<&str>) -> Stage {
    Stage::new(
        "platform-tests",
        "cargo",
        &[
            "test",
            "--workspace",
            "--exclude",
            "xtask",
            "--locked",
            "--tests",
        ],
    )
    .env("PROPTEST_CASES", &proptest_cases(developer_cases))
}

/// The stages to run: all, or only the one named by `--only`. An unknown name is an error
/// that lists the valid ones, never an empty green run.
fn select(all: Vec<Stage>, only: Option<&str>) -> Result<Vec<Stage>, String> {
    let Some(name) = only else { return Ok(all) };
    if !all.iter().any(|s| s.name == name) {
        let names: Vec<_> = all.iter().map(|s| s.name).collect();
        return Err(format!(
            "no test stage named {name}; choose one of {}",
            names.join(", ")
        ));
    }
    Ok(all.into_iter().filter(|s| s.name == name).collect())
}

/// Runs `test` (or one of its stages) and returns the exit code.
pub fn run(only: Option<&str>) -> i32 {
    let all = if only == Some("platform-tests") {
        vec![platform_stage(
            std::env::var("PROPTEST_CASES").ok().as_deref(),
        )]
    } else {
        stages()
    };
    match select(all, only) {
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

    #[test]
    fn should_run_unit_integration_and_doctests_when_test_is_built() {
        // given
        let stages = stages();

        // when
        let names: Vec<_> = stages.iter().map(|s| s.name).collect();

        // then
        assert_eq!(names, ["unit-tests", "integration-tests", "doctests"]);
        assert!(format!("{:?}", stages[0].action).contains("\"--locked\""));
    }

    #[test]
    fn should_keep_developer_proptest_cases_when_set_at_or_above_the_floor() {
        // given
        let values = [None, Some("1024"), Some("4096"), Some("16"), Some("many")];

        // when
        let cases = values.map(proptest_cases);
        let stage = format!("{:?}", stages_with(Some("4096"))[0].action);

        // then
        assert_eq!(cases, ["1024", "1024", "4096", "1024", "1024"]);
        assert!(stage.contains("(\"PROPTEST_CASES\", \"4096\")"), "{stage}");
    }

    #[test]
    fn should_split_the_workspace_run_by_test_kind_when_test_is_built() {
        // given
        let stages = stages_with(None);

        // when
        let unit = format!("{:?}", stages[0].action);
        let integration = format!("{:?}", stages[1].action);

        // then
        assert!(unit.contains("\"--lib\", \"--bins\""), "{unit}");
        assert!(integration.contains("\"--test\", \"*\""), "{integration}");
        for debug in [&unit, &integration] {
            assert!(debug.contains("\"--workspace\""), "{debug}");
            assert!(debug.contains("(\"PROPTEST_CASES\", \"1024\")"), "{debug}");
        }
    }

    #[test]
    fn should_run_the_product_crates_natively_when_platform_stage_is_built() {
        // given
        let stage = platform_stage(None);

        // when
        let rendered = format!("{:?}", stage.action);

        // then
        assert_eq!(stage.name, "platform-tests");
        for want in [
            "\"--workspace\"",
            "\"--exclude\", \"xtask\"",
            "\"--locked\"",
            "\"--tests\"",
        ] {
            assert!(rendered.contains(want), "{want} missing from {rendered}");
        }
        assert!(
            rendered.contains("(\"PROPTEST_CASES\", \"1024\")"),
            "{rendered}"
        );
        assert!(
            stages().iter().all(|s| s.name != "platform-tests"),
            "check must not run it a second time"
        );
        assert!(catalog::find("platform-tests").is_some_and(|c| !c.in_check));
    }

    #[test]
    fn should_keep_only_the_named_stage_when_only_is_given() {
        // given
        let all = stages();

        // when
        let picked = select(all, Some("doctests")).unwrap();

        // then
        assert_eq!(picked.len(), 1);
        assert_eq!(picked[0].name, "doctests");
    }

    #[test]
    fn should_list_valid_stages_when_only_names_an_unknown_one() {
        // given
        let all = stages();

        // when
        let err = select(all, Some("nope")).unwrap_err();

        // then
        assert!(
            err.contains("unit-tests") && err.contains("doctests"),
            "{err}"
        );
    }

    #[test]
    fn should_run_only_cataloged_stages_when_test_is_built() {
        // given
        let stages = stages();

        // when
        let uncataloged: Vec<_> = stages
            .iter()
            .filter(|s| catalog::find(s.name).is_none())
            .map(|s| s.name)
            .collect();

        // then
        assert!(uncataloged.is_empty(), "{uncataloged:?}");
    }
}
