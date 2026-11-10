//! `exec:<absolute path>`: the stdout of an operator command (ADR 0033, ADR 0058).
//!
//! Before it runs, the command, its directory and every directory above it are checked
//! (`path.rs`): owned by root or the daemon's user and writable by neither group nor others, the
//! command and its directory no symlink. Pinned by the contract's EX-07.

use std::ffi::OsString;
use std::fmt;
use std::path::Path;
use std::time::Duration;

use mayfly_control::secrets::{
    MAX_SECRET_BYTES, Refusal, SecretError, SecretProvider, secret_from_bytes,
};
use mayfly_domain::{Secret, SecretRef};

use crate::command::{blocking, daemon_uid, run};
use crate::path::{
    Stat, command_directory_refusal, command_refusal, io_refusal, open_directory_of,
};

/// The deadline a secret command gets unless composition gives another.
pub const DEFAULT_DEADLINE: Duration = Duration::from_secs(10);

/// The daemon's variables a secret command may see, when present: what a command needs to
/// find its tools, its user's configuration and a temporary directory. Nothing else of the
/// daemon's environment reaches it.
pub const PASSED_VARIABLES: &[&str] = &["PATH", "HOME", "USER", "LOGNAME", "LANG", "TMPDIR"];

/// The variables a child gets. `Debug` prints their names only: values are paths and user
/// names that do not belong in a log or a bug report.
#[derive(Clone, Default)]
pub(crate) struct ChildEnvironment(pub(crate) Vec<(OsString, OsString)>);

impl ChildEnvironment {
    /// Drops every variable named in `names`.
    pub(crate) fn without(mut self, names: &[&str]) -> Self {
        self.0
            .retain(|(name, _)| !names.iter().any(|withheld| name == withheld));
        self
    }
}

impl fmt::Debug for ChildEnvironment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list()
            .entries(self.0.iter().map(|(name, _)| name))
            .finish()
    }
}

/// Runs the command a reference names, with no arguments, and reads its stdout.
#[derive(Debug, Clone)]
pub struct ExecProvider {
    pub(crate) owner: u32,
    pub(crate) environment: ChildEnvironment,
    pub(crate) deadline: Duration,
}

impl ExecProvider {
    /// A provider whose commands must belong to root or `owner`, and get exactly
    /// `environment` (less what [`crate::UriSecretProvider::new`] withholds) and `deadline`.
    #[must_use]
    pub const fn new(
        owner: u32,
        environment: Vec<(OsString, OsString)>,
        deadline: Duration,
    ) -> Self {
        Self {
            owner,
            environment: ChildEnvironment(environment),
            deadline,
        }
    }

    /// The daemon's provider: commands of root or the daemon's user, given the daemon's
    /// [`PASSED_VARIABLES`] less any in `withheld` (pinned by
    /// `tests/uri.rs::should_withhold_env_secrets_when_exec_runs_a_command`).
    #[must_use]
    pub fn for_daemon(withheld: &[&str], deadline: Duration) -> Self {
        let environment = PASSED_VARIABLES
            .iter()
            .filter_map(|name| std::env::var_os(name).map(|value| (OsString::from(name), value)))
            .collect();
        let provider = Self::new(daemon_uid(), environment, deadline);
        provider.withholding(withheld)
    }

    /// The same provider, its children never given a variable named in `names`.
    #[must_use]
    pub(crate) fn withholding(mut self, names: &[&str]) -> Self {
        self.environment = self.environment.without(names);
        self
    }
}

/// Checks the command `path` names and what is above it.
fn check_command(path: &Path, owner: u32) -> Result<(), Refusal> {
    let _directory = open_directory_of(path, owner, command_directory_refusal)?;
    let stat = std::fs::symlink_metadata(path).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => Refusal::NotFound,
        _ => io_refusal(&e),
    })?;
    command_refusal(owner, &Stat::of(&stat)).map_or(Ok(()), Err)
}

impl SecretProvider for ExecProvider {
    async fn resolve(&self, reference: &SecretRef) -> Result<Secret<String>, SecretError> {
        let SecretRef::Exec { path } = reference else {
            return Err(SecretError::new(reference, Refusal::Unsupported));
        };
        let (checked, owner) = (path.clone(), self.owner);
        blocking(reference, move || check_command(&checked, owner)).await?;
        let finished = run(
            reference,
            path,
            &[],
            &self.environment.0,
            self.deadline,
            MAX_SECRET_BYTES,
        )
        .await?;
        if !finished.status.success() {
            return Err(SecretError::new(
                reference,
                Refusal::Exited {
                    code: finished.status.code(),
                },
            ));
        }
        secret_from_bytes(reference, finished.stdout)
    }
}
