//! `cargo xtask security`: the supply-chain stages (foundation.md section 8, D15).
//!
//! Three stages: `deny` runs `cargo deny --locked check` against the root `deny.toml`
//! (advisories, licences, bans, sources), `secrets` runs Gitleaks over the whole git history
//! (D13), and `action-pins` checks that every `uses:` is pinned to a full commit SHA. A missing
//! pinned tool is `UNAVAILABLE`, never a pass (D15). FND-04 adds the `workflows` stage
//! (actionlint) with the first workflow; VER-06 adds the fuzz crate's own `deny` run.
//!
//! The secret scan refuses to run where it would silently see less than the whole history: outside a git work tree, in a work tree that is not this repository's root, in
//! a shallow clone, or in a repository without commits. It also fails when Gitleaks reports
//! no scanned commits, so an exit code of 0 over nothing is not a pass.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::action_pins;
use crate::process::{Stage, finish, run_report};
use crate::{repo, tools};

/// The stages `security` runs, in order. Their names are catalog ids.
pub fn stages() -> Vec<Stage> {
    vec![
        deny_stage(),
        Stage::in_process(
            "secrets",
            "gitleaks git --no-banner --redact over the full history (after a git preflight)",
            run_secrets,
        )
        .needs("gitleaks"),
        Stage::in_process(
            "action-pins",
            "check .github/workflows and .github/actions for unpinned uses:",
            action_pins::run,
        ),
    ]
}

fn deny_stage() -> Stage {
    Stage::new("deny", "cargo", &["deny", "--locked", "check"]).needs("cargo-deny")
}

/// Runs `git -C dir args` and returns its trimmed standard output.
fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .map_err(|err| format!("cannot run git: {err}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_owned())
    }
}

/// Checks that `dir` is the root of a complete, non-empty git history; returns its commit
/// count.
pub fn history_preflight(dir: &Path) -> Result<u64, String> {
    let not_a_clone = || {
        format!(
            "{} is not the root of a git work tree, so there is no history to scan; run the \
             security stage from a git clone of the repository, not an exported tree",
            dir.display()
        )
    };
    match git(dir, &["rev-parse", "--is-inside-work-tree"]) {
        Ok(answer) if answer == "true" => {}
        _ => return Err(not_a_clone()),
    }
    let top = git(dir, &["rev-parse", "--show-toplevel"]).map_err(|_| not_a_clone())?;
    let same = match (Path::new(&top).canonicalize(), dir.canonicalize()) {
        (Ok(top), Ok(dir)) => top == dir,
        _ => false,
    };
    if !same {
        return Err(not_a_clone());
    }
    match git(dir, &["rev-parse", "--is-shallow-repository"]) {
        Ok(answer) if answer == "false" => {}
        Ok(_) => {
            return Err(
                "the clone is shallow, so Gitleaks would scan only part of the \
                        history; run `git fetch --unshallow` (in GitHub Actions, check out \
                        with `fetch-depth: 0`)"
                    .into(),
            );
        }
        Err(err) => return Err(format!("cannot tell whether the clone is shallow: {err}")),
    }
    let count = git(dir, &["rev-list", "--count", "--all"])
        .ok()
        .and_then(|n| n.parse::<u64>().ok())
        .unwrap_or(0);
    if count == 0 {
        return Err("the repository has no commits, so there is no history to scan".into());
    }
    Ok(count)
}

/// The number Gitleaks prints in its `N commits scanned.` line, colour codes ignored.
pub fn commits_scanned(log: &str) -> Option<u64> {
    let mut plain = String::with_capacity(log.len());
    let mut chars = log.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            plain.push(c);
        }
    }
    plain.lines().find_map(|line| {
        let (before, _) = line.split_once(" commits scanned")?;
        before.split_whitespace().next_back()?.parse().ok()
    })
}

/// The verdict on one Gitleaks run: its exit status and its log.
pub fn judge_scan(success: bool, log: &str) -> Result<(), String> {
    if !success {
        return Err("Gitleaks reported findings or failed (see its output above)".into());
    }
    match commits_scanned(log) {
        Some(n) if n > 0 => Ok(()),
        Some(_) => Err("Gitleaks scanned 0 commits; the history scan checked nothing".into()),
        None => Err("Gitleaks did not report how many commits it scanned".into()),
    }
}

/// The Gitleaks arguments for the repository at `root`; every path is absolute.
fn gitleaks_args(root: &Path) -> Vec<PathBuf> {
    vec![
        "git".into(),
        "--no-banner".into(),
        "--redact".into(),
        "--config".into(),
        root.join(".gitleaks.toml"),
        root.to_path_buf(),
    ]
}

fn scan_secrets(root: &Path) -> Result<(), String> {
    let commits = history_preflight(root)?;
    eprintln!("xtask: secrets: scanning {commits} commits");
    let out = Command::new("gitleaks")
        .args(gitleaks_args(root))
        .output()
        .map_err(|err| format!("cannot run gitleaks: {err}"))?;
    let log = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    eprint!("{log}");
    judge_scan(out.status.success(), &log)
}

/// The in-process `secrets` stage body.
fn run_secrets() -> i32 {
    match scan_secrets(&repo::root()) {
        Ok(()) => 0,
        Err(err) => {
            eprintln!("xtask: secrets: {err}");
            1
        }
    }
}

/// Runs `security` and returns the exit code.
pub fn run() -> i32 {
    finish(&run_report(&stages(), &tools::require))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog;

    #[test]
    fn should_run_locked_deny_check_when_security_is_built() {
        // given
        let stages = stages();

        // when
        let rendered = format!("{:?}", stages[0].action);

        // then
        assert_eq!(stages.len(), 3);
        assert_eq!(stages[0].name, "deny");
        assert_eq!(stages[0].tool, Some("cargo-deny"));
        for want in ["\"deny\"", "\"--locked\"", "\"check\""] {
            assert!(rendered.contains(want), "{want} missing from {rendered}");
        }
    }

    #[test]
    fn should_run_only_cataloged_stages_when_security_is_built() {
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

    fn sh_git(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .args([
                "-c",
                "user.name=fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "init.defaultBranch=main",
            ])
            .args(args)
            .current_dir(dir)
            .output()
            .expect("run git");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// A repository at `parent/name` with `commits` empty commits.
    fn repository(parent: &Path, name: &str, commits: usize) -> PathBuf {
        let dir = parent.join(name);
        std::fs::create_dir_all(&dir).expect("create repository");
        sh_git(&dir, &["init", "--quiet"]);
        for i in 0..commits {
            sh_git(
                &dir,
                &["commit", "--quiet", "--allow-empty", "-m", &format!("c{i}")],
            );
        }
        dir
    }

    #[test]
    fn should_refuse_scan_when_directory_is_not_a_git_work_tree() {
        // given
        let temp = tempfile::tempdir().expect("tempdir");

        // when
        let result = history_preflight(temp.path());

        // then
        let err = result.expect_err("no repository");
        assert!(err.contains("not the root of a git work tree"), "{err}");
    }

    #[test]
    fn should_refuse_scan_when_directory_is_below_the_work_tree_root() {
        // given
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = repository(temp.path(), "repo", 1);
        let below = dir.join("sub");
        std::fs::create_dir_all(&below).expect("create subdirectory");

        // when
        let result = history_preflight(&below);

        // then
        assert!(result.is_err(), "{result:?}");
    }

    #[test]
    fn should_refuse_scan_when_clone_is_shallow() {
        // given
        let temp = tempfile::tempdir().expect("tempdir");
        let source = repository(temp.path(), "source", 2);
        let url = format!("file://{}", source.display());
        sh_git(
            temp.path(),
            &["clone", "--quiet", "--depth", "1", &url, "shallow"],
        );

        // when
        let result = history_preflight(&temp.path().join("shallow"));

        // then
        let err = result.expect_err("shallow clone");
        assert!(err.contains("shallow"), "{err}");
    }

    #[test]
    fn should_refuse_scan_when_repository_has_no_commits() {
        // given
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = repository(temp.path(), "empty", 0);

        // when
        let result = history_preflight(&dir);

        // then
        let err = result.expect_err("no commits");
        assert!(err.contains("no commits"), "{err}");
    }

    #[test]
    fn should_count_commits_when_clone_is_complete() {
        // given
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = repository(temp.path(), "full", 2);

        // when
        let result = history_preflight(&dir);

        // then
        assert_eq!(result, Ok(2));
    }

    #[test]
    fn should_read_commit_count_when_log_is_coloured() {
        // given
        let log = "\u{1b}[90m7:49AM\u{1b}[0m \u{1b}[32mINF\u{1b}[0m \u{1b}[1m53 commits scanned.\u{1b}[0m\n";

        // when
        let count = commits_scanned(log);

        // then
        assert_eq!(count, Some(53));
    }

    #[test]
    fn should_fail_scan_when_gitleaks_passes_over_zero_commits() {
        // given
        let log = "INF 0 commits scanned.\nINF no leaks found\n";

        // when
        let verdict = judge_scan(true, log);

        // then
        let err = verdict.expect_err("nothing scanned");
        assert!(err.contains("0 commits"), "{err}");
    }

    #[test]
    fn should_fail_scan_when_gitleaks_does_not_report_a_count() {
        // given
        let log = "INF no leaks found\n";

        // when
        let verdict = judge_scan(true, log);

        // then
        assert!(verdict.is_err());
    }

    #[test]
    fn should_pass_scan_when_gitleaks_scanned_commits_without_findings() {
        // given
        let log = "INF 3 commits scanned.\nINF no leaks found\n";

        // when
        let verdict = judge_scan(true, log);

        // then
        assert_eq!(verdict, Ok(()));
    }

    #[test]
    fn should_use_absolute_config_and_repository_when_gitleaks_is_called() {
        // given
        let root = repo::root();

        // when
        let args = gitleaks_args(&root);

        // then
        assert!(args[4].is_absolute() && args[4].is_file(), "{args:?}");
        assert_eq!(args[5], root);
    }
}
