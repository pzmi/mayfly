//! `mayfly-cli` (shared plane): Binaries `mayflyd` (the daemon) and `mayfly` (the operator CLI).
//!
//! The binaries parse and delegate to composition; nothing here decides anything.

use std::ffi::OsString;
use std::path::PathBuf;

use mayfly_composition::daemon::Options;

/// The exit code of a command line that does not parse (`EX_USAGE`).
pub const USAGE: u8 = 64;

/// The roles of plan §11: `all` runs both planes in this process; `control` and `data` run one
/// plane each over the socket link (LNK-01).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Both planes, joined by the in-process link.
    All,
    /// The control plane alone.
    Control,
    /// The data plane alone.
    Data,
}

/// What `mayflyd` was asked to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaemonArgs {
    /// `mayflyd --config <path> [--role all|control|data]`.
    Run {
        /// The configuration file.
        config: PathBuf,
        /// The role.
        role: Role,
    },
    /// `mayflyd --version`.
    Version,
}

impl DaemonArgs {
    /// Reads the arguments after the program name. `--config` is required: the service
    /// install (OPS-01) passes it, so no default path is guessed. Pinned by
    /// `tests::should_read_the_config_and_role_when_both_are_given`.
    ///
    /// # Errors
    /// What is wrong, for the usage message.
    pub fn parse(args: Vec<OsString>) -> Result<Self, String> {
        let mut config = None;
        let mut role = Role::All;
        let mut args = args.into_iter();
        while let Some(arg) = args.next() {
            match arg.to_str() {
                Some("--version") => return Ok(Self::Version),
                Some("--config") => {
                    config = Some(PathBuf::from(args.next().ok_or("--config needs a path")?));
                }
                Some("--role") => {
                    role = match args.next().as_deref().and_then(std::ffi::OsStr::to_str) {
                        Some("all") => Role::All,
                        Some("control") => Role::Control,
                        Some("data") => Role::Data,
                        _ => return Err("--role takes all, control or data".to_owned()),
                    };
                }
                _ => {
                    return Err(format!(
                        "unexpected argument {:?}; usage: mayflyd --config <path> [--role all]",
                        arg.to_string_lossy()
                    ));
                }
            }
        }
        let config = config.ok_or("--config <path> is required")?;
        Ok(Self::Run { config, role })
    }
}

/// The first executable `name` in the directories of `path` (a `PATH` value), absolute.
#[must_use]
pub fn find_on_path(name: &str, path: &std::ffi::OsStr) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt as _;
    std::env::split_paths(path)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(name))
        .find(|candidate| {
            std::fs::metadata(candidate)
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
}

/// The daemon's options from this process: `tart` from `PATH`, `mayfly` (for `mayfly
/// vm-shim`) beside this executable, and only the allowlisted names of this process's
/// environment (ADR 0058), so no secret value is held in the options.
///
/// # Errors
/// What could not be found.
pub fn daemon_options(
    config: PathBuf,
    version: &str,
    env: mayfly_composition::paths::PlatformEnv,
) -> Result<Options, String> {
    let tart = std::env::var_os("PATH")
        .and_then(|path| find_on_path("tart", &path))
        .ok_or("no executable `tart` on PATH")?;
    let exe = std::env::current_exe().map_err(|e| format!("cannot find this executable: {e}"))?;
    let shim_program = exe
        .parent()
        .map(|dir| dir.join("mayfly"))
        .filter(|p| p.is_file())
        .ok_or("no `mayfly` executable beside mayflyd")?;
    let config = if config.is_absolute() {
        config
    } else {
        std::env::current_dir()
            .map_err(|e| format!("cannot read the working directory: {e}"))?
            .join(config)
    };
    Ok(Options {
        config,
        tart,
        shim_program,
        environment: mayfly_composition::daemon::child_environment(std::env::vars_os()),
        version: version.to_owned(),
        env,
    })
}

/// The line a binary prints for `--version`-style output: `<name> <version>`.
pub fn version_line(name: &str, version: &str) -> String {
    format!("{name} {version}")
}

/// What `mayfly` was asked to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MayflyCommand {
    /// `mayfly vm-shim <args>`: run as a VM's shim (ADR 0037), the arguments after `vm-shim`.
    VmShim(Vec<OsString>),
    /// Anything else, for now: print the version.
    Version,
}

impl MayflyCommand {
    /// Reads the arguments after the program name.
    #[must_use]
    pub fn parse(args: Vec<OsString>) -> Self {
        let mut args = args.into_iter();
        match args.next() {
            Some(first) if first == "vm-shim" => Self::VmShim(args.collect()),
            _ => Self::Version,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{DaemonArgs, MayflyCommand, Role, find_on_path, version_line};

    #[test]
    fn should_join_name_and_version_when_formatting_the_version_line() {
        // given
        let (name, version) = ("mayfly", "1.2.3");

        // when
        let line = version_line(name, version);

        // then
        assert_eq!(line, "mayfly 1.2.3");
    }

    #[test]
    fn should_pass_the_rest_through_when_the_command_is_vm_shim() {
        // given
        let args = vec!["vm-shim".into(), "--vm".into(), "x".into()];

        // when
        let command = MayflyCommand::parse(args);

        // then
        assert_eq!(
            command,
            MayflyCommand::VmShim(vec!["--vm".into(), "x".into()])
        );
        assert_eq!(MayflyCommand::parse(Vec::new()), MayflyCommand::Version);
    }

    #[test]
    fn should_read_the_config_and_role_when_both_are_given() {
        // given
        let args = vec![
            "--role".into(),
            "all".into(),
            "--config".into(),
            "/etc/mayfly.toml".into(),
        ];

        // when
        let parsed = DaemonArgs::parse(args);

        // then
        assert_eq!(
            parsed,
            Ok(DaemonArgs::Run {
                config: "/etc/mayfly.toml".into(),
                role: Role::All
            })
        );
        assert!(DaemonArgs::parse(Vec::new()).is_err());
        assert!(DaemonArgs::parse(vec!["--role".into(), "both".into()]).is_err());
        assert_eq!(
            DaemonArgs::parse(vec!["--version".into()]),
            Ok(DaemonArgs::Version)
        );
    }

    #[test]
    fn should_find_the_first_executable_when_path_lists_several() {
        // given
        let path = std::env::join_paths(["bin", "/no-such-directory", "/bin"])
            .unwrap_or_else(|e| panic!("{e}"));

        // when
        let found = find_on_path("sh", &path);

        // then
        assert_eq!(found, Some("/bin/sh".into()));
        assert_eq!(find_on_path("no-such-program-anywhere", &path), None);
    }
}
