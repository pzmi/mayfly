//! `mayfly doctor [--json]`, basic (RT-03): what an operator checks first, from the CLI's side.
//! OPS-01 adds the real host checks (launchd, Softnet, the App's permissions) and RT-07 the
//! reaper's last report.
//!
//! Each check is `ok`, `warn` or `fail`; any `fail` makes `mayfly doctor` exit 1. Pinned by
//! `crates/mayfly-cli/tests/operator.rs::should_report_every_check_when_doctor_runs_against_a_ready_daemon`.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::client;
use super::wire::{Request, Status};
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
/// `breakers`, `desync`.
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
    Report { checks }
}
