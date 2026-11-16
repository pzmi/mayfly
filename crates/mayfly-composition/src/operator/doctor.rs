//! `mayfly doctor [--json]`, basic (RT-03): what an operator checks first, from the CLI's side.
//! OPS-01 adds the real host checks (launchd, Softnet, the App's permissions); RT-07 added the
//! reaper's last report (`reaper`: a sweep asked of the run, within P14's hourly budget, then
//! its runners of mayfly's name shape that no registration recorded, which mayfly never
//! removes). GH-08 added `github_trust` (the daemon's start-up trust checks: the fork-PR
//! setting, each pool's public repositories its runner group does not admit, and the App's
//! permissions against what the options need, ADR 0045).
//!
//! Each check is `ok`, `warn` or `fail`; any `fail` makes `mayfly doctor` exit 1. Pinned by
//! `crates/mayfly-cli/tests/operator.rs::should_report_every_check_when_doctor_runs_against_a_ready_daemon`.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::client;
use super::wire::{Reaper, Request, Status, Trust};
use crate::config::Config;

/// One check's verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// Nothing to do.
    Ok,
    /// Worth a look; nothing is broken.
    Warn,
    /// Broken.
    Fail,
}

/// One check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Check {
    /// What was checked, `snake_case`.
    pub name: String,
    /// Its verdict.
    pub verdict: Verdict,
    /// What was found, for a person.
    pub detail: String,
}

/// Every check, in a fixed order: `config`, `tart`, `daemon`, `ready`, `safe_mode`,
/// `breakers`, `desync`, `reaper`, `github_trust`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Report {
    /// The checks.
    pub checks: Vec<Check>,
}

impl Report {
    /// Whether a check failed.
    #[must_use]
    pub fn failed(&self) -> bool {
        self.checks.iter().any(|c| c.verdict == Verdict::Fail)
    }
}

fn check(name: &str, verdict: Verdict, detail: impl Into<String>) -> Check {
    Check {
        name: name.to_owned(),
        verdict,
        detail: detail.into(),
    }
}

/// The first executable `name` in the absolute directories of `path` (a `PATH` value).
#[must_use]
pub fn on_path(name: &str, path: &OsStr) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt as _;
    std::env::split_paths(path)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(name))
        .find(|candidate| {
            std::fs::metadata(candidate)
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
}

/// Where the socket checked came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SocketFrom {
    /// `--socket <path>`.
    Named,
    /// The configuration's `state_dir`, or the platform's default places.
    Derived,
}

/// Runs every check: the configuration at `config` (when given; an invalid one is a `fail`,
/// and the other checks still run), `tart` on `path` (the `PATH` value), and the daemon at
/// `socket` through `status`.
///
/// # Panics
/// When called from inside a Tokio runtime, as [`client::request`].
#[must_use]
pub fn run(config: Option<&Path>, socket: &Path, from: SocketFrom, path: Option<&OsStr>) -> Report {
    let mut checks = Vec::new();
    checks.push(match config.map(Config::load) {
        None if from == SocketFrom::Named => check(
            "config",
            Verdict::Warn,
            "no --config given: not checked; the socket is the one --socket names",
        ),
        None => check(
            "config",
            Verdict::Warn,
            "no --config given: not checked; the socket is looked for in the default places",
        ),
        Some(Ok(_)) => check("config", Verdict::Ok, "valid"),
        Some(Err(e)) => check("config", Verdict::Fail, e.to_string()),
    });
    checks.push(match path.and_then(|p| on_path("tart", p)) {
        Some(_) => check("tart", Verdict::Ok, "found on PATH"),
        None => check("tart", Verdict::Fail, "no executable `tart` on PATH"),
    });
    let status = match client::request(socket, &Request::Status) {
        Ok(answer) if answer.ok => answer
            .result
            .and_then(|v| serde_json::from_value::<Status>(v).ok())
            .ok_or_else(|| "mayflyd answered a status this mayfly cannot read".to_owned()),
        Ok(answer) => Err(answer
            .error
            .map_or_else(|| "refused".to_owned(), |e| e.message)),
        Err(e) => Err(e.to_string()),
    };
    let status = match status {
        Ok(status) => status,
        Err(why) => {
            checks.push(check("daemon", Verdict::Fail, why));
            return Report { checks };
        }
    };
    checks.push(check(
        "daemon",
        Verdict::Ok,
        format!(
            "mayflyd {} answers: run {}, epoch {}, {}",
            status.version, status.run.number, status.run.epoch, status.run.phase
        ),
    ));
    checks.push(if status.run.ready {
        check("ready", Verdict::Ok, "the run reported ready")
    } else {
        check("ready", Verdict::Warn, "the run has not reported ready yet")
    });
    checks.push(match &status.run.safe_mode {
        None => check("safe_mode", Verdict::Ok, "not in safe mode"),
        Some(reason) => check(
            "safe_mode",
            Verdict::Fail,
            format!("in safe mode ({reason}): nothing is created or destroyed"),
        ),
    });
    let tripped: Vec<&str> = status
        .pools
        .iter()
        .filter(|p| !p.tripped.is_empty())
        .map(|p| p.name.as_str())
        .collect();
    checks.push(if tripped.is_empty() {
        check("breakers", Verdict::Ok, "no breaker tripped")
    } else {
        check(
            "breakers",
            Verdict::Warn,
            format!(
                "tripped in {}: `mayfly pool reset-breaker <pool>` once fixed",
                tripped.join(", ")
            ),
        )
    });
    checks.push(if status.attempts.desync.is_empty() {
        check("desync", Verdict::Ok, "no VM vanished outside mayfly")
    } else {
        check(
            "desync",
            Verdict::Warn,
            format!(
                "{} VM(s) vanished outside mayfly: a desync; send the diagnostics bundle",
                status.attempts.desync.len()
            ),
        )
    });
    checks.push(reaper_check(socket));
    checks.push(trust_check(socket));
    Report { checks }
}

/// The `github_trust` check: the daemon's answer to `trust` (GH-08). `fail` when the listeners
/// are stopped or held by the fork-PR setting (it is on, or GitHub denies its read) or the App
/// lacks a permission the options need; `warn` when a check is not done yet or could not be
/// made, a pool's runner group admits no public repository, or the App holds more than the
/// options need. Pinned by
/// `crates/mayfly-composition/tests/daemon.rs::github::should_refuse_every_pool_and_report_it_when_fork_workflows_run`,
/// `github::should_serve_no_pool_and_fail_doctor_when_the_fork_setting_is_denied`
/// and `doctor::tests::should_judge_each_trust_report_when_doctor_reads_it`.
fn trust_check(socket: &Path) -> Check {
    let answer = match client::request(socket, &Request::Trust) {
        Ok(answer) => answer,
        Err(e) => return check("github_trust", Verdict::Warn, e.to_string()),
    };
    if !answer.ok {
        return match answer.error {
            Some(e) if e.code == "unknown_command" => check(
                "github_trust",
                Verdict::Warn,
                "this mayflyd does not report its trust checks (it is older than this mayfly)",
            ),
            Some(e) => check("github_trust", Verdict::Warn, e.message),
            None => check("github_trust", Verdict::Warn, "refused"),
        };
    }
    match answer
        .result
        .and_then(|v| serde_json::from_value::<Trust>(v).ok())
    {
        Some(trust) => judge_trust(&trust),
        None => check(
            "github_trust",
            Verdict::Warn,
            "mayflyd answered a trust report this mayfly cannot read",
        ),
    }
}

/// The verdict on a [`Trust`] report (see [`trust_check`]).
#[must_use]
pub fn judge_trust(trust: &Trust) -> Check {
    let mut fails = Vec::new();
    let mut warns = Vec::new();
    match trust.fork_pr_setting.as_str() {
        "off" | "passed" => {}
        "refused" => fails.push(
            "the organisation runs workflows from fork pull requests in private repositories, so `check_fork_pr_setting` started no listener this run: no pool is served; turn that setting off, then restart mayflyd"
                .to_owned(),
        ),
        "denied" => fails.push(
            "GitHub refuses the fork-PR setting's read (403 or 404), so no listener starts while `check_fork_pr_setting` is on: grant the App \"Administration: Read\" in the organisation; it is read again"
                .to_owned(),
        ),
        "unread" => warns.push(
            "the fork-PR setting could not be read yet (no answer from GitHub); no listener starts until it is"
                .to_owned(),
        ),
        other => warns.push(format!("the fork-PR setting check is {other}")),
    }
    if !trust.missing_permissions.is_empty() {
        fails.push(format!(
            "the App lacks what the options need: {}",
            trust.missing_permissions.join(", ")
        ));
    }
    for pool in &trust.public_not_admitted {
        warns.push(format!(
            "pool {pool} has allowed repositories but its runner group admits no public repository: if one of them is public, GitHub routes its jobs nothing; allow public repositories in the group"
        ));
    }
    for (pool, code) in &trust.unreported {
        warns.push(format!(
            "pool {pool}: its runner group could not be read ({code})"
        ));
    }
    if !trust.excess_permissions.is_empty() {
        warns.push(format!(
            "the App holds more than the options need: {} (ADR 0045)",
            trust.excess_permissions.join(", ")
        ));
    }
    if !trust.permissions_read {
        warns.push("the App's permissions have not been read yet".to_owned());
    }
    let verdict = if !fails.is_empty() {
        Verdict::Fail
    } else if !warns.is_empty() {
        Verdict::Warn
    } else {
        Verdict::Ok
    };
    let detail = if fails.is_empty() && warns.is_empty() {
        "the start-up trust checks passed".to_owned()
    } else {
        fails
            .into_iter()
            .chain(warns)
            .collect::<Vec<_>>()
            .join("; ")
    };
    check("github_trust", verdict, detail)
}

/// The `reaper` check: the daemon's answer to `reaper` (RT-07). Pinned by
/// `crates/mayfly-cli/src/operate_tests.rs::should_report_each_check_when_doctor_meets_a_daemon_in_trouble`.
fn reaper_check(socket: &Path) -> Check {
    let answer = match client::request(socket, &Request::Reaper) {
        Ok(answer) => answer,
        Err(e) => return check("reaper", Verdict::Warn, e.to_string()),
    };
    if !answer.ok {
        return match answer.error {
            Some(e) if e.code == "unknown_command" => check(
                "reaper",
                Verdict::Warn,
                "this mayflyd does not report the reaper (it is older than this mayfly)",
            ),
            Some(e) => check("reaper", Verdict::Warn, e.message),
            None => check("reaper", Verdict::Warn, "refused"),
        };
    }
    let Some(reaper) = answer
        .result
        .and_then(|v| serde_json::from_value::<Reaper>(v).ok())
    else {
        return check(
            "reaper",
            Verdict::Warn,
            "mayflyd answered a reaper report this mayfly cannot read",
        );
    };
    let failed = reaper
        .failure
        .as_deref()
        .map(|code| format!("; its last listing failed ({code})"))
        .unwrap_or_default();
    match reaper.last {
        None => check(
            "reaper",
            Verdict::Warn,
            format!("GitHub's runners have not been listed yet{failed}"),
        ),
        Some(sweep) if sweep.unrecognised.is_empty() => check(
            "reaper",
            if failed.is_empty() {
                Verdict::Ok
            } else {
                Verdict::Warn
            },
            format!(
                "the last sweep listed {} runner(s), none unrecognised{failed}",
                sweep.listed
            ),
        ),
        Some(sweep) => {
            let named: Vec<String> = sweep
                .unrecognised
                .iter()
                .map(|r| format!("{} (id {})", r.name, r.id))
                .collect();
            check(
                "reaper",
                Verdict::Warn,
                format!(
                    "{} runner(s) of mayfly's name shape that no registration recorded: {}; mayfly never removes them: check each and remove it by hand{failed}",
                    named.len(),
                    named.join(", ")
                ),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use super::*;

    #[test]
    fn should_judge_each_trust_report_when_doctor_reads_it() {
        // given
        let passed = Trust {
            fork_pr_setting: "off".to_owned(),
            permissions_read: true,
            ..Trust::default()
        };
        let refused = Trust {
            fork_pr_setting: "refused".to_owned(),
            ..passed.clone()
        };
        let missing = Trust {
            missing_permissions: vec!["actions: write".to_owned()],
            ..passed.clone()
        };
        let denied = Trust {
            fork_pr_setting: "denied".to_owned(),
            ..passed.clone()
        };
        let unrouted = Trust {
            public_not_admitted: BTreeSet::from(["mac".to_owned()]),
            ..passed.clone()
        };
        let excess = Trust {
            excess_permissions: vec!["contents: read".to_owned()],
            ..passed.clone()
        };

        // when
        let verdicts: Vec<Verdict> = [&passed, &refused, &denied, &missing, &unrouted, &excess]
            .into_iter()
            .map(|t| judge_trust(t).verdict)
            .collect();

        // then
        assert_eq!(
            verdicts,
            [
                Verdict::Ok,
                Verdict::Fail,
                Verdict::Fail,
                Verdict::Fail,
                Verdict::Warn,
                Verdict::Warn
            ]
        );
        let unrouted = judge_trust(&unrouted).detail;
        assert!(
            unrouted.contains(
                "pool mac has allowed repositories but its runner group admits no public repository"
            ),
            "{unrouted}"
        );
        assert!(
            judge_trust(&denied).detail.contains("Administration: Read"),
            "{}",
            judge_trust(&denied).detail
        );
    }

    #[test]
    fn should_warn_when_a_trust_check_is_not_done_or_could_not_be_made() {
        // given: the fork setting unread, then pending; a pool's report failed; permissions not
        // read
        let unread = Trust {
            fork_pr_setting: "unread".to_owned(),
            permissions_read: true,
            ..Trust::default()
        };
        let pending = Trust {
            fork_pr_setting: "pending".to_owned(),
            unreported: BTreeMap::from([("mac".to_owned(), "unavailable".to_owned())]),
            ..Trust::default()
        };

        // when
        let (unread, pending) = (judge_trust(&unread), judge_trust(&pending));

        // then
        assert_eq!(
            (unread.verdict, pending.verdict),
            (Verdict::Warn, Verdict::Warn)
        );
        assert!(
            unread.detail.contains("no listener starts"),
            "{}",
            unread.detail
        );
        for part in ["pending", "pool mac", "unavailable", "not been read"] {
            assert!(pending.detail.contains(part), "{}", pending.detail);
        }
    }
}
