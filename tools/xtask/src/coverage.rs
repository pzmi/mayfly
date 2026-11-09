//! `cargo xtask coverage`: LLVM line coverage of the in-process suites and the floors that gate
//! it (verification.md section 10, foundation.md sections 5 and 9). Two `check` stages share
//! one measurement:
//!
//! - `coverage`: `cargo llvm-cov nextest` over the workspace under the `ci` profile, then the
//!   overall and per-crate floors of `coverage-floor.toml`;
//! - `changed-lines`: the lines a change adds or edits in `crates/*/src` against the merge base
//!   must be covered at `changed_line` percent, so a floor that the old code carries cannot
//!   hide new code nothing exercises.
//!
//! What is measured: every unit, component, contract and integration test of every workspace
//! member run instrumented, so a line is covered when some test executes it. Not measured:
//! doctests (stable `llvm-cov` cannot instrument them), files under `tests/` (cargo-llvm-cov
//! leaves them out), and the fault lab and the fake binaries' black-box runs. `tools/xtask`
//! is measured and printed on its own row but never floored: its tool-probe tests take
//! different branches on machines with different tools installed, so a floor over it would
//! depend on the machine, not on the code. Coverage measures execution; whether a test can
//! fail is the mutants' question (verification.md section 9).
//!
//! The report is `target/coverage/lcov.info`, stamped with a fingerprint of the tree it
//! measured. `changed-lines` reads the report only while the stamp matches, so it never joins
//! a diff with the LCOV of other file contents; with a stale or absent report it measures
//! again. Floors **ratchet only up** (`should_never_lower_a_floor_below_its_starting_value_when_the_floors_file_is_read`);
//! lowering one needs the owner.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::hash::{DefaultHasher, Hasher};
use std::path::Path;
use std::process::Command;

use serde::Deserialize;

use crate::process::{Stage, finish, run_report};
use crate::{repo, test, tools};

/// The floors file, from the repository root.
const FLOOR_FILE: &str = "coverage-floor.toml";

/// Where the LCOV report is written, from the repository root.
pub const LCOV_PATH: &str = "target/coverage/lcov.info";

/// The fingerprint of the tree the LCOV report measured, from the repository root.
const STAMP_PATH: &str = "target/coverage/lcov.stamp";

/// A sanity bound, not a target: a floor below it is a typo. The real floors are the starting
/// values pinned by `should_never_lower_a_floor_below_its_starting_value_when_the_floors_file_is_read`.
const MIN_FLOOR: u32 = 50;

/// The key under which files of `tools/xtask` are counted; report only, never floored.
const XTASK: &str = "xtask";

/// Environment variables that change what the instrumented build compiles; the stamp records
/// each one's value.
const BUILD_ENV: [&str; 4] = [
    "RUSTFLAGS",
    "CARGO_ENCODED_RUSTFLAGS",
    "CARGO_BUILD_RUSTFLAGS",
    "RUSTUP_TOOLCHAIN",
];

const COVERAGE_DISPLAY: &str = "cargo llvm-cov nextest --workspace --locked --profile ci --lcov --output-path target/coverage/lcov.info, then coverage-floor.toml";

const CHANGED_DISPLAY: &str = "changed lines of crates/*/src since the merge base, against coverage-floor.toml (reuses a fresh target/coverage/lcov.info)";

/// The `coverage` stage, as `check` registers it.
pub fn coverage_stage() -> Stage {
    Stage::in_process("coverage", COVERAGE_DISPLAY, run_coverage)
        .needs("cargo-llvm-cov")
        .and_needs("cargo-nextest")
}

/// The `changed-lines` stage, as `check` registers it.
pub fn changed_lines_stage() -> Stage {
    Stage::in_process("changed-lines", CHANGED_DISPLAY, run_changed_lines)
        .needs("cargo-llvm-cov")
        .and_needs("cargo-nextest")
}

/// The floors in `coverage-floor.toml`.
#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Floors {
    /// Percent of the instrumented lines under `crates/`.
    pub overall_line: u32,
    /// Percent of the changed instrumented lines of `crates/*/src`.
    pub changed_line: u32,
    /// Percent per crate directory name under `crates/`.
    #[serde(default, rename = "crate")]
    pub crate_floors: BTreeMap<String, u32>,
}

/// Parses the floors file and refuses a floor outside `MIN_FLOOR..=100`, and a floor on
/// `xtask`, which is report only.
pub fn parse_floors(text: &str) -> Result<Floors, String> {
    let floors: Floors =
        toml::from_str(text).map_err(|e| format!("{FLOOR_FILE} does not parse: {e}"))?;
    if floors.crate_floors.contains_key(XTASK) {
        return Err(format!("{FLOOR_FILE}: {XTASK} is reported, never floored"));
    }
    let all = [
        ("overall_line", floors.overall_line),
        ("changed_line", floors.changed_line),
    ]
    .into_iter()
    .chain(floors.crate_floors.iter().map(|(k, v)| (k.as_str(), *v)));
    for (name, value) in all {
        if !(MIN_FLOOR..=100).contains(&value) {
            return Err(format!(
                "{FLOOR_FILE}: {name} = {value} is outside {MIN_FLOOR}..=100"
            ));
        }
    }
    Ok(floors)
}

/// Instrumented and covered line counts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    /// Lines with a `DA:` record.
    pub instrumented: u64,
    /// Of those, lines executed at least once.
    pub covered: u64,
}

impl Counts {
    /// Whether `covered / instrumented` reaches `floor` percent, in integers.
    fn meets(self, floor: u32) -> bool {
        self.covered * 100 >= self.instrumented * u64::from(floor)
    }

    /// The percentage to two decimals, for display only.
    fn percent(self) -> f64 {
        if self.instrumented == 0 {
            return 100.0;
        }
        let hundredths = self.covered * 10_000 / self.instrumented;
        f64::from(u32::try_from(hundredths).unwrap_or(10_000)) / 100.0
    }
}

/// Line counts of one LCOV report, overall and per crate.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Measured {
    /// Every file under `crates/`; `xtask` and files outside both are left out.
    pub overall: Counts,
    /// Per crate directory name (`mayfly-domain`, `xtask`, ...).
    pub per_crate: BTreeMap<String, Counts>,
}

/// The crate a source path belongs to: the directory after `crates/`, or `xtask` for
/// `tools/xtask/`.
fn crate_of(path: &str) -> Option<String> {
    let path = path.replace('\\', "/");
    if let Some((_, rest)) = path.rsplit_once("/crates/") {
        return rest.split('/').next().map(str::to_owned);
    }
    path.contains("/tools/xtask/").then(|| XTASK.to_owned())
}

/// One `DA:<line>,<hits>[,<checksum>]` record; `number` is its zero-based line in the report.
fn parse_da(record: &str, number: usize) -> Result<(u32, u64), String> {
    let mut fields = record.split(',');
    let line = fields.next().and_then(|l| l.parse::<u32>().ok());
    let hits = fields.next().and_then(|h| h.parse::<u64>().ok());
    match (line, hits) {
        (Some(line), Some(hits)) => Ok((line, hits)),
        _ => Err(format!(
            "lcov line {}: bad DA record {record:?}",
            number + 1
        )),
    }
}

/// Reads the `SF:` / `DA:` records of an LCOV report.
pub fn parse_lcov(text: &str) -> Result<Measured, String> {
    let mut measured = Measured::default();
    let mut current: Option<String> = None;
    for (number, line) in text.lines().enumerate() {
        let line = line.trim();
        if let Some(path) = line.strip_prefix("SF:") {
            current = crate_of(path);
        } else if let Some(record) = line.strip_prefix("DA:") {
            let (_, hits) = parse_da(record, number)?;
            let add = |counts: &mut Counts| {
                counts.instrumented += 1;
                counts.covered += u64::from(hits > 0);
            };
            if let Some(name) = &current {
                if name != XTASK {
                    add(&mut measured.overall);
                }
                add(measured.per_crate.entry(name.clone()).or_default());
            }
        }
    }
    if measured.overall.instrumented == 0 {
        return Err("the LCOV report has no instrumented line under crates/".to_owned());
    }
    Ok(measured)
}

/// Per repository-relative file (`crates/mayfly-domain/src/a.rs`), the hit count of each
/// instrumented line.
pub type FileLines = BTreeMap<String, BTreeMap<u32, u64>>;

/// The repository-relative, `/`-separated form of an LCOV `SF:` path, trying each root in
/// turn. `None` for a file outside every root.
fn relative_path(path: &str, roots: &[String]) -> Option<String> {
    let path = path.replace('\\', "/");
    roots.iter().find_map(|root| {
        let root = root.replace('\\', "/");
        path.strip_prefix(root.trim_end_matches('/'))?
            .strip_prefix('/')
            .map(str::to_owned)
    })
}

/// Reads an LCOV report into per-file line hits, keyed by repository-relative path. Files
/// outside `roots` are left out. A line recorded twice for one file (one record per
/// instantiation) adds its hits up, so a line any instantiation ran counts as covered.
pub fn parse_lcov_lines(text: &str, roots: &[String]) -> Result<FileLines, String> {
    let mut files = FileLines::new();
    let mut current: Option<String> = None;
    for (number, line) in text.lines().enumerate() {
        let line = line.trim();
        if let Some(path) = line.strip_prefix("SF:") {
            current = relative_path(path, roots);
        } else if line == "end_of_record" {
            current = None;
        } else if let Some(record) = line.strip_prefix("DA:") {
            let (at, hits) = parse_da(record, number)?;
            if let Some(file) = &current {
                let slot = files
                    .entry(file.clone())
                    .or_default()
                    .entry(at)
                    .or_default();
                *slot = slot.saturating_add(hits);
            }
        }
    }
    Ok(files)
}

/// Compares the measurement with the floors. Returns the report text and whether every floor
/// holds. A floor naming a crate absent from the report fails: it would otherwise pass
/// silently after a rename.
pub fn evaluate(measured: &Measured, floors: &Floors) -> (String, bool) {
    let mut out = String::new();
    let mut ok = true;
    let mut row = |name: &str, counts: Counts, floor: Option<u32>| {
        let verdict = match floor {
            None if name == XTASK => "report only".to_owned(),
            None => "no floor".to_owned(),
            Some(f) if counts.meets(f) => format!("floor {f}%: ok"),
            Some(f) => {
                ok = false;
                format!(
                    "BELOW floor {f}% by {:.2} points ({} uncovered lines)",
                    f64::from(f) - counts.percent(),
                    counts.instrumented - counts.covered
                )
            }
        };
        let _ = writeln!(
            out,
            "{name:<26} {:>6.2}%  {:>6}/{:<6} lines  {verdict}",
            counts.percent(),
            counts.covered,
            counts.instrumented
        );
    };
    row(
        "overall (crates/)",
        measured.overall,
        Some(floors.overall_line),
    );
    for (name, counts) in &measured.per_crate {
        row(name, *counts, floors.crate_floors.get(name).copied());
    }
    for name in floors.crate_floors.keys() {
        if !measured.per_crate.contains_key(name) {
            ok = false;
            let _ = writeln!(
                out,
                "{name:<26} FAILED: floor set but no such crate in the report"
            );
        }
    }
    (out, ok)
}

/// The lines a diff adds, per repository-relative path, read from `git diff -U0` output.
/// A file deleted (`+++ /dev/null`) adds none. Pinned by
/// `should_read_added_lines_when_a_zero_context_diff_is_parsed`.
pub fn parse_added_lines(diff: &str) -> BTreeMap<String, BTreeSet<u32>> {
    let mut out: BTreeMap<String, BTreeSet<u32>> = BTreeMap::new();
    let mut file: Option<String> = None;
    for line in diff.lines() {
        if let Some(path) = line.strip_prefix("+++ ") {
            file = path.strip_prefix("b/").map(str::to_owned);
        } else if let Some(hunk) = line.strip_prefix("@@ ") {
            let Some(file) = &file else { continue };
            let Some(added) = hunk.split_whitespace().find_map(|f| f.strip_prefix('+')) else {
                continue;
            };
            let (start, count) = added.split_once(',').unwrap_or((added, "1"));
            let (Ok(start), Ok(count)) = (start.parse::<u32>(), count.parse::<u32>()) else {
                continue;
            };
            out.entry(file.clone())
                .or_default()
                .extend(start..start + count);
        }
    }
    out
}

/// Whether `path` is product code the changed-line gate covers: `crates/<c>/src/**/*.rs`.
fn is_gated(path: &str) -> bool {
    let mut parts = path.split('/');
    parts.next() == Some("crates")
        && parts.next().is_some()
        && parts.next() == Some("src")
        && path.ends_with(".rs")
}

/// Counts the changed lines that have a `DA:` record, in the gated files, and lists the
/// uncovered ones. A changed line with no record (a comment, a signature the compiler folds)
/// is not instrumented and counts for nothing either way.
pub fn changed_counts(
    added: &BTreeMap<String, BTreeSet<u32>>,
    lines: &FileLines,
) -> (Counts, Vec<String>) {
    let mut counts = Counts::default();
    let mut missed = Vec::new();
    for (path, changed) in added.iter().filter(|(p, _)| is_gated(p)) {
        let Some(hits) = lines.get(path) else {
            continue;
        };
        for line in changed {
            let Some(hit) = hits.get(line) else { continue };
            counts.instrumented += 1;
            if *hit > 0 {
                counts.covered += 1;
            } else {
                missed.push(format!("{path}:{line}"));
            }
        }
    }
    (counts, missed)
}

fn git(root: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .map_err(|e| format!("cannot run git: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// The revision the change under review starts from, when `--changed-since` names none: the
/// merge base of `HEAD` with `origin/main` (else `main`). When `HEAD` is that base itself, the
/// change under review is the uncommitted work of a dirty tree, else the last commit (a
/// squash-merged push to `main`, a commit just landed).
fn default_base(root: &Path) -> Result<String, String> {
    let base = ["origin/main", "main"]
        .iter()
        .find_map(|branch| git(root, &["merge-base", "HEAD", branch]).ok())
        .map(|s| s.trim().to_owned())
        .ok_or("no origin/main or main to find the merge base with: pass --changed-since <rev>")?;
    let head = git(root, &["rev-parse", "HEAD"])?.trim().to_owned();
    if base == head && git(root, &["status", "--porcelain"])?.trim().is_empty() {
        return git(root, &["rev-parse", "HEAD~1"])
            .map(|s| s.trim().to_owned())
            .map_err(|e| format!("the last commit has no parent to diff against: {e}"));
    }
    Ok(base)
}

/// The lines changed since `base`, working tree included: tracked changes from `git diff`, and
/// every line of an untracked file (a new file is all change).
fn changed_since(root: &Path, base: &str) -> Result<BTreeMap<String, BTreeSet<u32>>, String> {
    let diff = git(
        root,
        &["diff", "-U0", "--no-color", "--no-ext-diff", base, "--"],
    )?;
    let mut added = parse_added_lines(&diff);
    for path in git(root, &["ls-files", "--others", "--exclude-standard"])?.lines() {
        let text = std::fs::read_to_string(root.join(path)).unwrap_or_default();
        let count = u32::try_from(text.lines().count()).unwrap_or(u32::MAX);
        added.entry(path.to_owned()).or_default().extend(1..=count);
    }
    Ok(added)
}

/// A fingerprint of the tree as it is now: `HEAD`, the tracked diff and every untracked file.
fn tree_fingerprint(root: &Path) -> Result<String, String> {
    let mut hasher = DefaultHasher::new();
    hasher.write(git(root, &["rev-parse", "HEAD"])?.as_bytes());
    hasher.write(git(root, &["diff", "HEAD", "--binary", "--no-ext-diff"])?.as_bytes());
    for path in git(root, &["ls-files", "--others", "--exclude-standard"])?.lines() {
        hasher.write(path.as_bytes());
        hasher.write(&std::fs::read(root.join(path)).unwrap_or_default());
    }
    Ok(format!("tree {:016x}", hasher.finish()))
}

/// The stamp text: the tree fingerprint, `rustc -vV` of the toolchain the build uses (run in
/// `root`, so `rust-toolchain.toml` applies) and the [`BUILD_ENV`] values.
fn stamp_of(root: &Path, fingerprint: &str) -> String {
    let rustc = Command::new("rustc")
        .arg("-vV")
        .current_dir(root)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map_or_else(
            || "rustc: unavailable".to_owned(),
            |o| String::from_utf8_lossy(&o.stdout).into_owned(),
        );
    stamp_text(fingerprint, &rustc, |name| std::env::var(name).ok())
}

/// [`stamp_of`] with the toolchain description and the environment given. Pinned by
/// `should_change_the_stamp_when_the_toolchain_or_rustflags_change`.
fn stamp_text(fingerprint: &str, rustc: &str, var: impl Fn(&str) -> Option<String>) -> String {
    let mut out = format!("{fingerprint}\n{}\n", rustc.trim_end());
    for name in BUILD_ENV {
        let _ = match var(name) {
            Some(value) => writeln!(out, "{name}={value:?}"),
            None => writeln!(out, "{name} unset"),
        };
    }
    out
}

/// The LCOV report, when its stamp matches the tree as it is now; `None` when there is no
/// report or stamp, or the tree changed since the report was measured.
fn fresh_lcov(root: &Path) -> Result<Option<String>, String> {
    let Ok(stamp) = std::fs::read_to_string(root.join(STAMP_PATH)) else {
        return Ok(None);
    };
    if stamp.trim_end() != stamp_of(root, &tree_fingerprint(root)?).trim_end() {
        return Ok(None);
    }
    std::fs::read_to_string(root.join(LCOV_PATH))
        .map(Some)
        .map_err(|e| format!("cannot read {LCOV_PATH}: {e}"))
}

/// Runs the instrumented workspace tests and writes the LCOV report and its stamp. The
/// fingerprint is taken before the run, so an edit made during it leaves the report stale.
fn measure(root: &Path) -> Result<String, String> {
    tools::require("cargo-nextest")?;
    tools::require("cargo-llvm-cov")?;
    std::fs::create_dir_all(root.join("target/coverage"))
        .map_err(|e| format!("cannot create target/coverage: {e}"))?;
    match std::fs::remove_file(root.join(STAMP_PATH)) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("cannot remove {STAMP_PATH}: {e}")),
    }
    let stamp = stamp_of(root, &tree_fingerprint(root)?);
    let status = Command::new("cargo")
        .args([
            "llvm-cov",
            "nextest",
            "--workspace",
            "--locked",
            "--profile",
            "ci",
            "--lcov",
            "--output-path",
            LCOV_PATH,
        ])
        .current_dir(root)
        .env(
            "PROPTEST_CASES",
            test::proptest_cases(std::env::var("PROPTEST_CASES").ok().as_deref()),
        )
        .status()
        .map_err(|e| format!("cannot run cargo llvm-cov: {e}"))?;
    if !status.success() {
        return Err(format!("cargo llvm-cov failed with {status}"));
    }
    std::fs::write(root.join(STAMP_PATH), stamp)
        .map_err(|e| format!("cannot write {STAMP_PATH}: {e}"))?;
    std::fs::read_to_string(root.join(LCOV_PATH))
        .map_err(|e| format!("cannot read {LCOV_PATH}: {e}"))
}

fn read_floors(root: &Path) -> Result<Floors, String> {
    parse_floors(
        &std::fs::read_to_string(root.join(FLOOR_FILE))
            .map_err(|e| format!("cannot read {FLOOR_FILE}: {e}"))?,
    )
}

/// Reports a stage body's result as an exit code.
fn conclude(result: Result<(String, bool), String>, failure: &str) -> i32 {
    match result {
        Ok((report, true)) => {
            print!("{report}");
            0
        }
        Ok((report, false)) => {
            print!("{report}");
            eprintln!("xtask: {failure}");
            1
        }
        Err(err) => {
            eprintln!("xtask: {err}");
            1
        }
    }
}

/// The `coverage` stage body: measure, then the overall and per-crate floors.
fn run_coverage() -> i32 {
    let root = repo::root();
    let result = (|| {
        let floors = read_floors(&root)?;
        let measured = parse_lcov(&measure(&root)?)?;
        let (mut report, ok) = evaluate(&measured, &floors);
        let _ = writeln!(report, "report at {LCOV_PATH}");
        Ok((report, ok))
    })();
    conclude(
        result,
        &format!("coverage is below a floor in {FLOOR_FILE}; add tests, never lower it"),
    )
}

/// The `changed-lines` stage body: the default base.
fn run_changed_lines() -> i32 {
    changed_lines(None)
}

/// The changed-line gate against `since`, or the default base.
fn changed_lines(since: Option<&str>) -> i32 {
    let root = repo::root();
    let result = (|| {
        let floors = read_floors(&root)?;
        let base = match since {
            Some(rev) => rev.to_owned(),
            None => default_base(&root)?,
        };
        let lcov = match fresh_lcov(&root)? {
            Some(text) => text,
            None => measure(&root)?,
        };
        let roots = [
            root.to_string_lossy().into_owned(),
            root.canonicalize()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default(),
        ];
        let lines = parse_lcov_lines(&lcov, &roots)?;
        let (counts, missed) = changed_counts(&changed_since(&root, &base)?, &lines);
        let ok = counts.meets(floors.changed_line);
        let mut report = format!(
            "changed lines since {base}: {:.2}% ({}/{} instrumented lines), floor {}%\n",
            counts.percent(),
            counts.covered,
            counts.instrumented,
            floors.changed_line
        );
        if !ok {
            for line in &missed {
                let _ = writeln!(report, "  not covered: {line}");
            }
        }
        Ok((report, ok))
    })();
    conclude(
        result,
        "changed lines are below the floor; cover them with tests, never lower it",
    )
}

/// `cargo xtask coverage [--changed-since <rev>]`: the floors, then the changed-line gate; both
/// run even when the first fails, so one run shows every problem.
pub fn command(changed_since: Option<&str>) -> i32 {
    let outcome = run_report(&[coverage_stage()], &tools::require);
    let floors_code = finish(&outcome);
    let changed_code = changed_lines(changed_since);
    i32::from(floors_code != 0 || changed_code != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const LCOV: &str = "\
SF:/r/crates/mayfly-domain/src/a.rs
DA:1,3
DA:2,0
DA:3,1
end_of_record
SF:/r/tools/xtask/src/b.rs
DA:1,0
DA:2,5
end_of_record
SF:/r/examples/c.rs
DA:1,0
end_of_record
";

    fn floors(overall: u32, domain: u32) -> Floors {
        Floors {
            overall_line: overall,
            changed_line: 90,
            crate_floors: BTreeMap::from([("mayfly-domain".to_owned(), domain)]),
        }
    }

    #[test]
    fn should_change_the_stamp_when_the_toolchain_or_rustflags_change() {
        // given
        let unset = |_: &str| None;
        let flagged = |name: &str| (name == "RUSTFLAGS").then(|| "-C opt-level=1".to_owned());

        // when
        let base = stamp_text("tree", "rustc 1.97.0", unset);
        let same = stamp_text("tree", "rustc 1.97.0", unset);
        let other_toolchain = stamp_text("tree", "rustc 1.98.0", unset);
        let other_flags = stamp_text("tree", "rustc 1.97.0", flagged);
        let other_tree = stamp_text("tree2", "rustc 1.97.0", unset);

        // then
        assert_eq!(base, same);
        for other in [&other_toolchain, &other_flags, &other_tree] {
            assert_ne!(&base, other);
        }
    }

    #[test]
    fn should_count_only_crates_in_the_overall_figure_when_lcov_is_parsed() {
        // given: xtask and a file outside crates/ each have an uncovered line
        let text = LCOV;

        // when
        let measured = parse_lcov(text).unwrap();

        // then
        let domain = Counts {
            instrumented: 3,
            covered: 2,
        };
        assert_eq!(measured.overall, domain);
        assert_eq!(measured.per_crate["mayfly-domain"], domain);
        assert_eq!(measured.per_crate[XTASK].covered, 1);
        assert!(!measured.per_crate.contains_key("c"));
    }

    #[test]
    fn should_pass_when_every_floor_is_met() {
        // given
        let measured = parse_lcov(LCOV).unwrap();

        // when: 2 of 3 lines is 66.66%
        let (report, ok) = evaluate(&measured, &floors(60, 66));

        // then
        assert!(ok, "{report}");
        assert!(report.contains("overall"), "{report}");
    }

    #[test]
    fn should_fail_when_a_crate_is_below_its_floor() {
        // given
        let measured = parse_lcov(LCOV).unwrap();

        // when
        let (report, ok) = evaluate(&measured, &floors(60, 67));

        // then
        assert!(!ok);
        assert!(report.contains("BELOW floor 67%"), "{report}");
    }

    #[test]
    fn should_fail_when_the_overall_figure_is_below_its_floor() {
        // given
        let measured = parse_lcov(LCOV).unwrap();

        // when
        let (report, ok) = evaluate(&measured, &floors(67, 66));

        // then
        assert!(!ok);
        assert!(report.contains("BELOW floor 67%"), "{report}");
    }

    #[test]
    fn should_fail_when_a_floor_names_a_crate_absent_from_the_report() {
        // given
        let measured = parse_lcov(LCOV).unwrap();
        let mut renamed = floors(60, 60);
        renamed.crate_floors.insert("mayfly-gone".to_owned(), 90);

        // when
        let (report, ok) = evaluate(&measured, &renamed);

        // then
        assert!(!ok);
        assert!(report.contains("mayfly-gone"), "{report}");
    }

    #[test]
    fn should_refuse_a_report_with_no_instrumented_line_when_lcov_is_parsed() {
        // given
        let text = "SF:/r/examples/c.rs\nDA:1,0\nend_of_record\n";

        // when
        let err = parse_lcov(text).unwrap_err();

        // then
        assert!(err.contains("no instrumented line"), "{err}");
    }

    #[test]
    fn should_refuse_a_malformed_record_when_lcov_is_parsed() {
        // given
        let text = "SF:/r/crates/mayfly-domain/src/a.rs\nDA:x,1\n";

        // when
        let err = parse_lcov(text).unwrap_err();

        // then
        assert!(err.contains("bad DA record"), "{err}");
    }

    #[test]
    fn should_refuse_floors_when_one_is_out_of_range_or_names_xtask() {
        // given
        let low = "overall_line = 49\nchanged_line = 90\n";
        let xtask = "overall_line = 85\nchanged_line = 90\n[crate]\nxtask = 90\n";
        let unknown = "overall_line = 85\nchanged_line = 90\nbranch = 1\n";
        let good = "overall_line = 85\nchanged_line = 90\n[crate]\nmayfly-domain = 95\n";

        // when
        let results = [low, xtask, unknown, good].map(parse_floors);

        // then
        assert!(results[0].as_ref().unwrap_err().contains("outside"));
        assert!(results[1].as_ref().unwrap_err().contains("never floored"));
        assert!(results[2].is_err());
        assert_eq!(
            results[3].as_ref().unwrap().crate_floors["mayfly-domain"],
            95
        );
    }

    #[test]
    fn should_read_added_lines_when_a_zero_context_diff_is_parsed() {
        // given
        let diff = "\
diff --git a/crates/mayfly-domain/src/a.rs b/crates/mayfly-domain/src/a.rs
--- a/crates/mayfly-domain/src/a.rs
+++ b/crates/mayfly-domain/src/a.rs
@@ -3 +3,2 @@ fn a()
-old
+new
+newer
@@ -10,2 +11,0 @@
-gone
-gone
@@ -20,0 +22 @@
+one
diff --git a/crates/mayfly-domain/src/old.rs b/crates/mayfly-domain/src/old.rs
--- a/crates/mayfly-domain/src/old.rs
+++ /dev/null
@@ -1,2 +0,0 @@
-x
-y
";

        // when
        let added = parse_added_lines(diff);

        // then
        assert_eq!(
            added["crates/mayfly-domain/src/a.rs"],
            BTreeSet::from([3, 4, 22])
        );
        assert!(!added.contains_key("crates/mayfly-domain/src/old.rs"));
    }

    #[test]
    fn should_gate_only_product_sources_when_changed_lines_are_counted() {
        // given
        let lines = FileLines::from([
            (
                "crates/mayfly-domain/src/a.rs".to_owned(),
                BTreeMap::from([(1, 1), (2, 0), (3, 4)]),
            ),
            (
                "crates/mayfly-domain/tests/t.rs".to_owned(),
                BTreeMap::from([(1, 0)]),
            ),
            ("tools/xtask/src/x.rs".to_owned(), BTreeMap::from([(1, 0)])),
        ]);
        let added = BTreeMap::from([
            (
                "crates/mayfly-domain/src/a.rs".to_owned(),
                BTreeSet::from([1, 2, 9]),
            ),
            (
                "crates/mayfly-domain/tests/t.rs".to_owned(),
                BTreeSet::from([1]),
            ),
            ("tools/xtask/src/x.rs".to_owned(), BTreeSet::from([1])),
            (
                "crates/mayfly-domain/src/gone.rs".to_owned(),
                BTreeSet::from([1]),
            ),
        ]);

        // when
        let (counts, missed) = changed_counts(&added, &lines);

        // then: line 9 has no record and the other files are not gated
        assert_eq!(
            counts,
            Counts {
                instrumented: 2,
                covered: 1
            }
        );
        assert_eq!(missed, ["crates/mayfly-domain/src/a.rs:2"]);
    }

    #[test]
    fn should_add_up_hits_when_a_line_is_recorded_twice() {
        // given
        let text = "SF:/r/crates/c/src/a.rs\nDA:1,0\nend_of_record\nSF:/r/crates/c/src/a.rs\nDA:1,2\nend_of_record\nSF:/elsewhere/a.rs\nDA:1,1\n";

        // when
        let lines = parse_lcov_lines(text, &["/r".to_owned()]).unwrap();

        // then
        assert_eq!(lines["crates/c/src/a.rs"][&1], 2);
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn should_pass_vacuously_when_no_changed_line_is_instrumented() {
        // given
        let counts = Counts::default();

        // when
        let ok = counts.meets(90);

        // then
        assert!(ok);
    }

    #[test]
    fn should_never_lower_a_floor_below_its_starting_value_when_the_floors_file_is_read() {
        // given: the starting floors of verification.md section 10; they only ratchet up
        let text = std::fs::read_to_string(repo::root().join(FLOOR_FILE)).expect("floors file");

        // when
        let floors = parse_floors(&text).expect("valid floors");

        // then
        assert!(floors.overall_line >= STARTING_OVERALL, "{floors:?}");
        assert!(floors.changed_line >= STARTING_CHANGED, "{floors:?}");
        for (name, start) in STARTING_CRATES {
            assert!(
                floors.crate_floors.get(*name).is_some_and(|f| f >= start),
                "{name} must keep a floor of at least {start}: {floors:?}"
            );
        }
    }

    /// The starting floors committed with FND-05; raise a floor in the file and its value here
    /// together, never lower either (lowering needs the owner).
    const STARTING_OVERALL: u32 = 85;
    const STARTING_CHANGED: u32 = 90;
    const STARTING_CRATES: &[(&str, u32)] = &[("mayfly-domain", 84), ("mayfly-control-domain", 96)];
}
