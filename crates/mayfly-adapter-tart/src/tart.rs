//! The `tart` commands the adapter runs (plan §8.2, domain-model §9.2), each a bounded
//! [`runner`](crate::process::runner) call with the deployment's environment.
//!
//! Exit codes follow Tart's error handling: 2 for "does not exist", "is not running" and "is
//! already running", 1 for other runtime errors, 64 for a usage error. A non-zero exit is a
//! refusal, a call that ran out of time or whose process was lost is not.

use std::ffi::OsString;
use std::time::Duration;

use crate::config::TartConfig;
use crate::parse::{self, ListEntry, VmInfo};
use crate::process::runner::{self, Call, Exited, Ran};

/// Why a call did not give an answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallError {
    /// Tart exited non-zero: it refused, so nothing happened.
    Refused {
        /// The exit code.
        code: i32,
        /// Its stderr, bounded and redacted.
        stderr: String,
    },
    /// The call did not finish by its deadline; it was killed and reaped.
    TimedOut,
    /// The process was killed by a signal it was not sent by us, or its output could not be
    /// read: what it did is not known.
    Lost,
    /// The program could not be started: nothing ran.
    SpawnFailed,
    /// It succeeded, but its output did not have the pinned shape (or passed the limit).
    Unparseable,
}

/// The `tart` CLI of one deployment.
#[derive(Debug, Clone)]
pub struct TartCli {
    config: TartConfig,
    environment: Vec<(OsString, OsString)>,
}

fn args(list: &[&str]) -> Vec<OsString> {
    list.iter().map(OsString::from).collect()
}

impl TartCli {
    /// The CLI described by `config`.
    #[must_use]
    pub fn new(config: TartConfig) -> Self {
        let environment = config.child_environment();
        Self {
            config,
            environment,
        }
    }

    /// The configuration.
    #[must_use]
    pub const fn config(&self) -> &TartConfig {
        &self.config
    }

    /// The environment each child gets.
    #[must_use]
    pub fn environment(&self) -> &[(OsString, OsString)] {
        &self.environment
    }

    async fn call(&self, args: Vec<OsString>, deadline: Duration) -> Result<Exited, CallError> {
        let ran = runner::run(Call {
            program: &self.config.tart,
            args,
            environment: &self.environment,
            deadline,
            output_limit: self.config.output_limit,
            redact: &self.config.redact,
        })
        .await;
        match ran {
            Ran::Exited(exited) if exited.success() => Ok(exited),
            Ran::Exited(Exited {
                code: Some(code),
                stderr,
                ..
            }) => Err(CallError::Refused { code, stderr }),
            Ran::Exited(_) | Ran::Lost(_) => Err(CallError::Lost),
            Ran::TimedOut => Err(CallError::TimedOut),
            Ran::SpawnFailed(_) => Err(CallError::SpawnFailed),
        }
    }

    /// `tart --version`.
    ///
    /// # Errors
    /// [`CallError`].
    pub async fn version(&self) -> Result<String, CallError> {
        let out = self
            .call(args(&["--version"]), self.config.deadlines.probe)
            .await?;
        parse::version(&out.stdout).map_err(|_| CallError::Unparseable)
    }

    /// `tart list --format json --source local`: the local VMs, OCI cache entries left out.
    ///
    /// # Errors
    /// [`CallError`]; a truncated output is [`CallError::Unparseable`].
    pub async fn list(&self) -> Result<Vec<ListEntry>, CallError> {
        let out = self
            .call(
                args(&["list", "--format", "json", "--source", "local"]),
                self.config.deadlines.list,
            )
            .await?;
        if out.truncated {
            return Err(CallError::Unparseable);
        }
        let entries = parse::list(&out.stdout).map_err(|_| CallError::Unparseable)?;
        Ok(entries.into_iter().filter(ListEntry::is_local).collect())
    }

    /// The seconds `tart stop --timeout` is given for `grace`. Pinned by
    /// `tests::should_round_a_grace_up_to_whole_seconds_when_stopping`.
    #[must_use]
    pub fn stop_seconds(grace: Duration) -> u64 {
        grace.as_secs() + u64::from(grace.subsec_nanos() > 0)
    }

    /// `tart get <name> --format json`; `None` when Tart says the VM does not exist.
    ///
    /// # Errors
    /// [`CallError`].
    pub async fn get(&self, name: &str) -> Result<Option<VmInfo>, CallError> {
        let got = self
            .call(
                args(&["get", name, "--format", "json"]),
                self.config.deadlines.get,
            )
            .await;
        match got {
            Ok(out) if !out.truncated => parse::get(&out.stdout)
                .map(Some)
                .map_err(|_| CallError::Unparseable),
            Ok(_) => Err(CallError::Unparseable),
            Err(CallError::Refused { code: 2, stderr }) if stderr.contains("does not exist") => {
                Ok(None)
            }
            Err(err) => Err(err),
        }
    }

    /// `tart clone <base> <name>`, never `--overwrite`: cloning onto an existing name is
    /// refused, as the port requires.
    ///
    /// # Errors
    /// [`CallError`].
    pub async fn clone(&self, base: &str, name: &str) -> Result<(), CallError> {
        self.call(args(&["clone", base, name]), self.config.deadlines.clone)
            .await
            .map(drop)
    }

    /// `tart set <name> --random-mac` (ADR 0026).
    ///
    /// # Errors
    /// [`CallError`].
    pub async fn set_random_mac(&self, name: &str) -> Result<(), CallError> {
        self.call(
            args(&["set", name, "--random-mac"]),
            self.config.deadlines.set,
        )
        .await
        .map(drop)
    }

    /// `tart set <name> --memory <mib>`.
    ///
    /// # Errors
    /// [`CallError`].
    pub async fn set_memory(&self, name: &str, mib: u64) -> Result<(), CallError> {
        self.call(
            args(&["set", name, "--memory", &mib.to_string()]),
            self.config.deadlines.set,
        )
        .await
        .map(drop)
    }

    /// `tart stop <name> --timeout <grace>`: Tart interrupts its own `tart run` and kills it
    /// after the grace; mayfly never signals it (ADR 0037).
    ///
    /// # Errors
    /// [`CallError`].
    pub async fn stop(&self, name: &str, grace: Duration) -> Result<(), CallError> {
        // Rounded up: a sub-second grace is one second, never `--timeout 0` (an immediate kill).
        let seconds = Self::stop_seconds(grace);
        let deadline = grace.saturating_add(self.config.deadlines.stop);
        self.call(
            args(&["stop", name, "--timeout", &seconds.to_string()]),
            deadline,
        )
        .await
        .map(drop)
    }

    /// `tart delete <name>`: one exact name, never a pattern.
    ///
    /// # Errors
    /// [`CallError`].
    pub async fn delete(&self, name: &str) -> Result<(), CallError> {
        self.call(args(&["delete", name]), self.config.deadlines.delete)
            .await
            .map(drop)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_round_a_grace_up_to_whole_seconds_when_stopping() {
        // given
        let graces = [0, 1, 999, 1_000, 1_001, 30_000].map(Duration::from_millis);

        // when
        let seconds = graces.map(TartCli::stop_seconds);

        // then
        assert_eq!(seconds, [0, 1, 1, 1, 2, 30]);
    }
}
