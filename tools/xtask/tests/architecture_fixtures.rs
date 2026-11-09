//! Black-box harness for the architecture checker (foundation.md section 3.4). The positive
//! fixture `tools/xtask/tests/architecture-valid/full-topology/` must pass, and every mini
//! workspace under `tools/xtask/tests/architecture-invalid/<case>/` must make `xtask architecture` exit
//! non-zero with a diagnostic line that names the rule and path in its `expected.txt`.
//!
//! `expected.txt` holds `rule: <id>`, `at: <path substring>` and optional
//! `contains: <message substring>` lines; one diagnostic line must satisfy all of them.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::thread;

fn repository_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("xtask lives two levels below the repository root")
        .to_path_buf()
}

fn fixtures_dir() -> PathBuf {
    repository_root().join("tools/xtask/tests/architecture-invalid")
}

fn xtask(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_xtask"))
        .args(args)
        .output()
        .expect("xtask runs")
}

fn cases() -> Vec<String> {
    let mut cases: Vec<String> = fs::read_dir(fixtures_dir())
        .expect("fixture directory exists")
        .map(|e| e.expect("readable entry"))
        .filter(|e| e.path().is_dir())
        .map(|e| e.file_name().into_string().expect("UTF-8 name"))
        .collect();
    cases.sort();
    cases
}

/// Whether one line of `output` is a diagnostic satisfying `expected`.
fn matches(output: &str, expected: &str) -> Result<bool, String> {
    let (mut rule, mut at, mut contains, mut only) = (None, None, Vec::new(), false);
    for line in expected.lines().filter(|l| !l.trim().is_empty()) {
        match line.split_once(':').map(|(k, v)| (k.trim(), v.trim())) {
            Some(("rule", v)) => rule = Some(v),
            Some(("at", v)) => at = Some(v),
            Some(("contains", v)) => contains.push(v),
            Some(("only", v)) => only = v == "true",
            _ => return Err(format!("bad expected.txt line: {line}")),
        }
    }
    let rule = rule.ok_or("expected.txt has no rule")?;
    let at = at.ok_or("expected.txt has no at")?;
    let prefix = format!("{rule}: ");
    if only
        && output
            .lines()
            .any(|line| line.starts_with("ARCH-") && !line.starts_with(&prefix))
    {
        return Ok(false);
    }
    Ok(output.lines().any(|line| {
        let Some((message, location)) = line.rsplit_once(" (at ") else {
            return false;
        };
        message.starts_with(&format!("{rule}: "))
            && location.contains(at)
            && contains.iter().all(|c| message.contains(c))
    }))
}

fn check_case(case: &str) -> Result<(), String> {
    let dir = fixtures_dir().join(case);
    let expected =
        fs::read_to_string(dir.join("expected.txt")).map_err(|e| format!("{case}: {e}"))?;
    let manifest = dir.join("Cargo.toml");
    let output = xtask(&[
        "architecture",
        "--manifest-path",
        manifest.to_str().unwrap(),
    ]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    match matches(&stderr, &expected) {
        Ok(true) if !output.status.success() => Ok(()),
        Ok(_) => Err(format!(
            "{case}: status {}, stderr:\n{stderr}",
            output.status
        )),
        Err(err) => Err(format!("{case}: {err}")),
    }
}

#[test]
fn should_fail_with_expected_rule_and_path_when_fixture_is_checked() {
    // given
    let cases = cases();
    assert!(!cases.is_empty(), "no negative fixtures");

    // when
    let failures: Vec<String> = thread::scope(|scope| {
        let handles: Vec<_> = cases
            .iter()
            .map(|case| scope.spawn(move || check_case(case)))
            .collect();
        handles
            .into_iter()
            .filter_map(|h| h.join().expect("fixture thread").err())
            .collect()
    });

    // then
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn should_have_fixture_when_task_names_it() {
    // given
    let cases = cases();

    // when
    let missing: Vec<_> = [
        "control-depends-on-data",
        "data-depends-on-control",
        "data-names-journal",
        "domain-std-fs",
        "control-domain-std-fs",
        "faultlab-imports-control-domain",
        "process-in-control",
        "test-without-should",
        "reversed-edge",
        "testkit-in-production",
        "domain-uses-tokio",
        "git-dependency",
        "ignored-test",
        "unjustified-allow",
        "sleep-in-test",
        "missing-given-when-then",
        "todo-in-production",
        "cfg-macro-platform-branch",
        "command-imports-query",
        "query-calls-command",
    ]
    .into_iter()
    .filter(|name| !cases.iter().any(|c| c == name))
    .collect();

    // then
    assert!(missing.is_empty(), "missing fixtures: {missing:?}");
}

#[test]
fn should_exit_zero_when_repository_and_fixtures_are_checked() {
    // given
    let args = ["architecture"];

    // when
    let output = xtask(&args);

    // then
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "stdout:\n{stdout}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("negative fixtures fail as expected"),
        "{stdout}"
    );
}

#[test]
fn should_reject_line_when_rule_matches_but_path_differs() {
    // given
    let output = "ARCH-unknown-crate: crate `x` (at crates/x/Cargo.toml)\n";
    let expected = "rule: ARCH-unknown-crate\nat: crates/y\n";

    // when
    let matched = matches(output, expected).unwrap();

    // then
    assert!(!matched);
}

#[test]
fn should_pass_when_positive_fixture_is_checked() {
    // given
    let manifest =
        repository_root().join("tools/xtask/tests/architecture-valid/full-topology/Cargo.toml");

    // when
    let output = xtask(&[
        "architecture",
        "--manifest-path",
        manifest.to_str().unwrap(),
    ]);

    // then
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(!stderr.contains("ARCH-"), "{stderr}");
}

/// Copies the directory tree `from` into `to`.
fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap().map(Result::unwrap) {
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), &target).unwrap();
        }
    }
}

#[test]
fn should_fail_with_process_authority_when_the_positive_data_plane_spawns_a_process() {
    // given
    let copy = tempfile::TempDir::new().unwrap();
    copy_tree(
        &repository_root().join("tools/xtask/tests/architecture-valid/full-topology"),
        copy.path(),
    );
    let lib = copy.path().join("crates/mayfly-data/src/lib.rs");
    let source = fs::read_to_string(&lib).unwrap();
    fs::write(
        &lib,
        format!(
            "{source}\npub fn spawn() {{\n    let _ = std::process::Command::new(\"tart\");\n}}\n"
        ),
    )
    .unwrap();
    let manifest = copy.path().join("Cargo.toml");

    // when
    let output = xtask(&[
        "architecture",
        "--manifest-path",
        manifest.to_str().unwrap(),
    ]);

    // then
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{stderr}");
    let diagnostics: Vec<&str> = stderr.lines().filter(|l| l.starts_with("ARCH-")).collect();
    assert_eq!(diagnostics.len(), 1, "{stderr}");
    assert!(
        diagnostics[0]
            .starts_with("ARCH-process-authority: mayfly-data names `std::process::Command::new`")
            && diagnostics[0].contains("(at crates/mayfly-data/src/lib.rs:"),
        "{stderr}"
    );
}
