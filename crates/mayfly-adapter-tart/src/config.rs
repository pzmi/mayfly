//! What the adapter is given by composition: programs, directories, the child environment's
//! allowlist and every call's deadline.

use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

/// The variable that stops Tart from pruning its caches on its own (ADR 0039, plan §8.2); set
/// on every call.
pub const NO_AUTO_PRUNE: &str = "TART_NO_AUTO_PRUNE";

/// The variable naming Tart's home (plan §8.2: a dedicated `TART_HOME` per deployment).
pub const TART_HOME: &str = "TART_HOME";

/// The variables a child may receive besides `TART_HOME` and [`NO_AUTO_PRUNE`] (ADR 0058 rule
/// 2): the names composition may pass in [`TartConfig::environment`], with values it read from
/// the daemon's environment less every `env:` secret. A name outside this list is dropped.
/// `LLVM_PROFILE_FILE` is instrumentation only: a coverage build's children write their profiles
/// where the run collects them instead of into their working directory, `/`; a release daemon
/// never has it.
pub const ALLOWED_VARIABLES: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "LANG",
    "TMPDIR",
    "LLVM_PROFILE_FILE",
];

/// The program that runs as a VM's shim: `mayfly vm-shim` in production (ADR 0037).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShimProgram {
    /// The executable, absolute.
    pub program: PathBuf,
    /// The arguments before the shim's own (`["vm-shim"]` for the `mayfly` binary).
    pub args: Vec<OsString>,
}

/// Each call's own deadline (plan §8.2): every `tart` child is stopped at it, so every
/// provider future completes (`mayfly_data::ports`, the executor never drops an effect).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Deadlines {
    /// `tart --version`.
    pub probe: Duration,
    /// `tart list`.
    pub list: Duration,
    /// One `tart get`.
    pub get: Duration,
    /// `tart clone`.
    pub clone: Duration,
    /// One `tart set`.
    pub set: Duration,
    /// From the shim's spawn until it is settled: the VM runs under it, or its run ended.
    pub start: Duration,
    /// `tart stop`'s own margin beyond the guest's grace.
    pub stop: Duration,
    /// `tart delete`.
    pub delete: Duration,
    /// After a `start` passed its deadline and `tart stop` was issued: how long the shim gets to
    /// report the run's end before `start` answers `Ambiguous` regardless.
    pub settle: Duration,
}

impl Deadlines {
    /// Defaults sized for a real host: a macOS guest can take a minute to boot.
    #[must_use]
    pub const fn host() -> Self {
        Self {
            probe: Duration::from_secs(10),
            list: Duration::from_secs(30),
            get: Duration::from_secs(10),
            clone: Duration::from_secs(300),
            set: Duration::from_secs(10),
            start: Duration::from_secs(120),
            stop: Duration::from_secs(30),
            delete: Duration::from_secs(60),
            settle: Duration::from_secs(10),
        }
    }
}

/// The adapter's configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TartConfig {
    /// The `tart` executable, absolute.
    pub tart: PathBuf,
    /// The deployment's own `TART_HOME`, absolute (plan §8.2).
    pub tart_home: PathBuf,
    /// Where each VM's shim keeps its lock, records and stdio files: `<shims>/<vm>/`.
    pub shims: PathBuf,
    /// The shim program.
    pub shim: ShimProgram,
    /// Allowlisted variables and their values (see [`ALLOWED_VARIABLES`]).
    pub environment: Vec<(OsString, OsString)>,
    /// Each call's deadline.
    pub deadlines: Deadlines,
    /// The most bytes of a `tart` call's stdout kept; more is an output that does not parse.
    pub output_limit: usize,
    /// The most bytes a shim lets each of `tart run`'s stdio files grow to before it truncates
    /// it.
    pub run_log_limit: u64,
    /// How often a waiting call re-reads what it waits for (a `start` settling).
    pub poll: Duration,
    /// Literal values masked in any stderr the adapter keeps (redaction, plan §8.2).
    pub redact: Vec<String>,
}

impl TartConfig {
    /// The environment every child gets: the allowlisted variables, `TART_HOME` and
    /// [`NO_AUTO_PRUNE`], nothing else (ADR 0058 rule 2).
    #[must_use]
    pub fn child_environment(&self) -> Vec<(OsString, OsString)> {
        let mut out: Vec<(OsString, OsString)> = self
            .environment
            .iter()
            .filter(|(name, _)| ALLOWED_VARIABLES.iter().any(|allowed| name == allowed))
            .cloned()
            .collect();
        out.push((TART_HOME.into(), self.tart_home.clone().into_os_string()));
        out.push((NO_AUTO_PRUNE.into(), "1".into()));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(environment: Vec<(OsString, OsString)>) -> TartConfig {
        TartConfig {
            tart: "/usr/local/bin/tart".into(),
            tart_home: "/var/mayfly/tart".into(),
            shims: "/var/mayfly/shims".into(),
            shim: ShimProgram {
                program: "/usr/local/bin/mayfly".into(),
                args: vec!["vm-shim".into()],
            },
            environment,
            deadlines: Deadlines::host(),
            output_limit: 1 << 20,
            run_log_limit: 1 << 20,
            poll: Duration::from_millis(100),
            redact: Vec::new(),
        }
    }

    #[test]
    fn should_give_children_only_the_allowlist_tart_home_and_no_auto_prune_when_built() {
        // given
        let given = config(vec![
            ("PATH".into(), "/usr/bin".into()),
            ("GITHUB_TOKEN".into(), "ghs_secret".into()),
            ("LANG".into(), "C".into()),
        ]);

        // when
        let environment = given.child_environment();

        // then
        let names: Vec<String> = environment
            .iter()
            .map(|(name, _)| name.to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["PATH", "LANG", "TART_HOME", "TART_NO_AUTO_PRUNE"]);
        assert!(
            environment
                .iter()
                .all(|(_, value)| value.to_string_lossy() != "ghs_secret")
        );
    }
}
