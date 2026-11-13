//! `mayfly-cli` (shared plane): Binaries `mayflyd` (the daemon) and `mayfly` (the operator CLI).
//!
//! The binaries parse and delegate to composition; nothing here decides anything. `mayfly`'s
//! operator commands (RT-03) go to `mayflyd` over its socket (`mayfly_composition::operator`);
//! [`render`] prints their answers for a person.

use std::ffi::OsString;
use std::io::Write;
use std::path::PathBuf;

use mayfly_composition::daemon::Options;
use mayfly_composition::operator::doctor::{SocketFrom, Verdict};
use mayfly_composition::operator::wire::{Failure, Request};
use mayfly_composition::operator::{self, client, client::ClientError};
use mayfly_composition::paths::PlatformEnv;

pub mod render;

#[cfg(test)]
mod operate_tests;

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

/// The first executable `name` in the absolute directories of `path` (a `PATH` value):
/// `doctor`'s own lookup, one rule for both binaries.
#[must_use]
pub fn find_on_path(name: &str, path: &std::ffi::OsStr) -> Option<PathBuf> {
    operator::doctor::on_path(name, path)
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

/// The exit code when the daemon refused a request, or `doctor` found a failure.
pub const REFUSED: u8 = 1;

/// The exit code when no daemon answers at the socket (`EX_UNAVAILABLE`).
pub const UNAVAILABLE: u8 = 69;

/// The exit code when the daemon answered `busy`: every connection slot was taken, try again
/// (`EX_TEMPFAIL`).
pub const BUSY: u8 = 75;

/// Where the CLI finds the daemon: `--socket <path>`, else the socket of `--config <path>`,
/// else the platform's default (`mayfly_composition::operator::socket_for`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Target {
    /// `--socket <path>`.
    pub socket: Option<PathBuf>,
    /// `--config <path>`.
    pub config: Option<PathBuf>,
}

/// What `mayfly` was asked to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MayflyCommand {
    /// `mayfly vm-shim <args>`: run as a VM's shim (ADR 0037), the arguments after `vm-shim`.
    VmShim(Vec<OsString>),
    /// `mayfly image verify <manifest>`: check an image manifest (plan §11, OPS-02).
    ImageVerify(PathBuf),
    /// `mayfly config validate <path>`: check a configuration offline (plan §11).
    ConfigValidate(PathBuf),
    /// A request to the daemon over its socket (RT-03): `status`, `pool ...`, `host ...`,
    /// `vm list`, `events`.
    Operator {
        /// Where the daemon is.
        target: Target,
        /// The request.
        request: Request,
        /// `--json`: print the answer as JSON.
        json: bool,
    },
    /// `mayfly doctor [--json]`.
    Doctor {
        /// Where the daemon is.
        target: Target,
        /// `--json`.
        json: bool,
    },
    /// A command line that does not parse: the usage message, for exit [`USAGE`].
    Usage(String),
    /// No command, `--version` or `version`: print the version.
    Version,
}

/// The usage of `mayfly image`.
const IMAGE_USAGE: &str = "usage: mayfly image verify <manifest>";

/// The usage of `mayfly`.
pub const MAYFLY_USAGE: &str = "usage: mayfly [--socket <path>] [--config <path>] <command>
  status [--json]
  pool list [--json] | pool pause|resume|drain <pool> | pool reset-breaker <pool> [--digest <sha256:...>]
  host drain [--deadline <duration>] | host resume
  vm list [--json]
  events [--follow] [--since <duration>] [--json]
  doctor [--json]
  config validate <path>
  image verify <manifest>
durations: a whole number and s, m, h or d (30m, 6h)";

/// The arguments after the command, with `--json` and the global options taken out.
struct Rest {
    words: Vec<String>,
    json: bool,
    target: Target,
}

fn rest(args: Vec<OsString>) -> Result<Rest, String> {
    let mut out = Rest {
        words: Vec::new(),
        json: false,
        target: Target::default(),
    };
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        let text = arg
            .to_str()
            .ok_or_else(|| "arguments must be UTF-8".to_owned())?
            .to_owned();
        match text.as_str() {
            "--json" => out.json = true,
            "--socket" => {
                out.target.socket =
                    Some(PathBuf::from(args.next().ok_or("--socket needs a path")?));
            }
            "--config" => {
                out.target.config =
                    Some(PathBuf::from(args.next().ok_or("--config needs a path")?));
            }
            _ => out.words.push(text),
        }
    }
    Ok(out)
}

fn seconds(text: &str) -> Result<u64, String> {
    mayfly_composition::config::parse_duration(text)
        .map(|d| d.as_secs())
        .ok_or_else(|| format!("{text:?} is not a duration (30m, 6h, 1d)"))
}

/// The request `words` name, with the flags they carry.
fn request(words: &[String]) -> Result<Request, String> {
    let words: Vec<&str> = words.iter().map(String::as_str).collect();
    let owned = |s: &str| s.to_owned();
    Ok(match words.as_slice() {
        ["status"] => Request::Status,
        ["pool", "list"] => Request::PoolList,
        ["pool", "pause", pool] => Request::PoolPause { pool: owned(pool) },
        ["pool", "resume", pool] => Request::PoolResume { pool: owned(pool) },
        ["pool", "drain", pool] => Request::PoolDrain { pool: owned(pool) },
        ["pool", "reset-breaker", pool] => Request::PoolResetBreaker {
            pool: owned(pool),
            digest: None,
        },
        ["pool", "reset-breaker", pool, "--digest", digest] => Request::PoolResetBreaker {
            pool: owned(pool),
            digest: Some(owned(digest)),
        },
        ["host", "drain"] => Request::HostDrain {
            deadline_secs: None,
        },
        ["host", "drain", "--deadline", deadline] => Request::HostDrain {
            deadline_secs: Some(seconds(deadline)?),
        },
        ["host", "resume"] => Request::HostResume,
        ["vm", "list"] => Request::VmList,
        ["events", flags @ ..] => {
            let (mut follow, mut since_secs) = (false, None);
            let mut flags = flags.iter();
            while let Some(flag) = flags.next() {
                match *flag {
                    "--follow" => follow = true,
                    "--since" => {
                        since_secs =
                            Some(seconds(flags.next().ok_or("--since needs a duration")?)?);
                    }
                    other => return Err(format!("unexpected argument {other:?}")),
                }
            }
            if !follow && since_secs.is_none() {
                return Err("events needs --follow, --since <duration> or both".to_owned());
            }
            Request::Events { follow, since_secs }
        }
        _ => return Err(MAYFLY_USAGE.to_owned()),
    })
}

impl MayflyCommand {
    /// Reads the arguments after the program name. Pinned by
    /// `tests::should_read_the_manifest_path_when_the_command_is_image_verify` and
    /// `tests::should_read_each_operator_request_when_its_words_are_given`.
    #[must_use]
    pub fn parse(args: Vec<OsString>) -> Self {
        let first = args.first().and_then(|a| a.to_str()).map(str::to_owned);
        match first.as_deref() {
            None | Some("--version" | "version") => return Self::Version,
            Some("vm-shim") => return Self::VmShim(args.into_iter().skip(1).collect()),
            Some("image") => {
                return match args.get(1..) {
                    Some([verb, path]) if verb == "verify" => {
                        Self::ImageVerify(PathBuf::from(path))
                    }
                    _ => Self::Usage(IMAGE_USAGE.to_owned()),
                };
            }
            Some(_) => {}
        }
        let Rest {
            words,
            json,
            target,
        } = match rest(args) {
            Ok(rest) => rest,
            Err(why) => return Self::Usage(why),
        };
        match words
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .as_slice()
        {
            ["config", "validate", path] => return Self::ConfigValidate(PathBuf::from(path)),
            ["doctor"] => return Self::Doctor { target, json },
            _ => {}
        }
        match request(&words) {
            Ok(request) => Self::Operator {
                target,
                request,
                json,
            },
            Err(why) => Self::Usage(why),
        }
    }
}

/// `mayfly config validate <path>`: the configuration read and checked offline, as `mayflyd`
/// reads it before its own requirements (`[images]`, the manifests): `Ok` with the line to
/// print, or the exit code (78) and the report.
///
/// # Errors
/// The exit code and the report, every problem named with its field and its fix.
pub fn config_validate(path: &std::path::Path) -> Result<String, (u8, String)> {
    mayfly_composition::config::Config::load(path)
        .map(|_| format!("configuration valid: {}", path.display()))
        .map_err(|error| (error.exit_code(), error.to_string()))
}

/// The socket `target` names, or the exit code and why not.
fn socket(target: &Target, env: &PlatformEnv) -> Result<PathBuf, (u8, String)> {
    if let Some(socket) = &target.socket {
        return Ok(socket.clone());
    }
    operator::socket_for(target.config.as_deref(), env).map_err(|e| match e {
        operator::LocateError::Config(error) => (error.exit_code(), error.to_string()),
        operator::LocateError::NoPlace => (USAGE, e.to_string()),
    })
}

/// Runs a request against the daemon, printing to `out` and `err`; returns the exit code: 0
/// served, [`REFUSED`] refused (the daemon's code and message on `err`, and with `--json` its
/// failure document on `out`), [`BUSY`] when the daemon had no free connection,
/// [`UNAVAILABLE`] when nothing answers, 78 when `--config` does not load. Pinned by
/// `tests/operator.rs`.
pub fn operate(
    target: &Target,
    request: &Request,
    json: bool,
    env: &PlatformEnv,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> u8 {
    let socket = match socket(target, env) {
        Ok(socket) => socket,
        Err((code, why)) => {
            let _ = writeln!(err, "mayfly: {why}");
            return code;
        }
    };
    if let Request::Events { .. } = request {
        // the first line of a follow is protocol, not an event: render prints nothing for it
        let mut code = 0;
        let streamed = client::stream(&socket, request, |answer| {
            match (answer.ok, answer.result) {
                (true, Some(result)) => render::event(&result, json, out),
                (_, _) => {
                    code = refused(answer.error.as_ref(), json, out, err);
                    false
                }
            }
        });
        return match streamed {
            Ok(()) => code,
            Err(e) => unreachable_code(&e, err),
        };
    }
    match client::request(&socket, request) {
        Ok(answer) if answer.ok => {
            render::served(request, answer.result.as_ref(), json, out);
            0
        }
        Ok(answer) => refused(answer.error.as_ref(), json, out, err),
        Err(e) => unreachable_code(&e, err),
    }
}

fn refused(failure: Option<&Failure>, json: bool, out: &mut dyn Write, err: &mut dyn Write) -> u8 {
    let Some(failure) = failure else {
        let _ = writeln!(err, "mayfly: refused");
        return REFUSED;
    };
    if json {
        let _ = writeln!(out, "{}", operator::wire::pretty(failure));
    }
    let _ = writeln!(err, "mayfly: {}: {}", failure.code, failure.message);
    if failure.code == "busy" {
        BUSY
    } else {
        REFUSED
    }
}

fn unreachable_code(error: &ClientError, err: &mut dyn Write) -> u8 {
    let _ = writeln!(err, "mayfly: {error}");
    match error {
        ClientError::Unreachable { .. } => UNAVAILABLE,
        ClientError::Io(_) | ClientError::Protocol(_) | ClientError::NotDaemon { .. } => REFUSED,
    }
}

/// `mayfly doctor [--json]`: every check printed; exit [`REFUSED`] when one failed. An invalid
/// `--config` is reported as the `config` check's failure, and the daemon is then looked for
/// at the platform's default socket (or `--socket`), so every other check still runs.
pub fn doctor(
    target: &Target,
    json: bool,
    env: &PlatformEnv,
    path: Option<&std::ffi::OsStr>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> u8 {
    let (socket, from) = match &target.socket {
        Some(socket) => (Ok(socket.clone()), SocketFrom::Named),
        None => (
            operator::socket_for(target.config.as_deref(), env)
                .or_else(|_| operator::socket_for(None, env)),
            SocketFrom::Derived,
        ),
    };
    let socket = match socket {
        Ok(socket) => socket,
        Err(why) => {
            let _ = writeln!(err, "mayfly: {why}");
            return USAGE;
        }
    };
    let report = operator::doctor::run(target.config.as_deref(), &socket, from, path);
    if json {
        let _ = writeln!(out, "{}", operator::wire::pretty(&report));
    } else {
        for check in &report.checks {
            let verdict = match check.verdict {
                Verdict::Ok => "ok  ",
                Verdict::Warn => "warn",
                Verdict::Fail => "FAIL",
            };
            let _ = writeln!(out, "{verdict} {}: {}", check.name, check.detail);
        }
    }
    if report.failed() { REFUSED } else { 0 }
}

/// `mayfly image verify <manifest>`: the manifest read and checked alone, offline (no registry,
/// no configuration): `Ok` with the line to print, or the exit code (78, an invalid
/// configuration's) and the report. Its checks against a deployment (the repository, the
/// pool's guest, the runner pins) are `mayflyd`'s at start (ADR 0091). Pinned by
/// `tests/image_verify.rs`.
///
/// # Errors
/// The exit code and the report, every problem named with its field and its fix.
pub fn image_verify(path: &std::path::Path) -> Result<String, (u8, String)> {
    mayfly_composition::manifest::Manifest::load(path)
        .map(|manifest| format!("image manifest valid: {}", manifest.summary()))
        .map_err(|error| (error.exit_code(), error.to_string()))
}

#[cfg(test)]
mod tests {
    use mayfly_composition::operator::wire::Request;

    use super::{
        DaemonArgs, IMAGE_USAGE, MAYFLY_USAGE, MayflyCommand, Role, Target, find_on_path,
        version_line,
    };

    #[test]
    fn should_exit_75_and_print_the_failure_when_the_daemon_is_busy() {
        // given
        let busy = mayfly_composition::operator::wire::Failure {
            code: "busy".to_owned(),
            message: "try again".to_owned(),
        };
        let other = mayfly_composition::operator::wire::Failure {
            code: "already_paused".to_owned(),
            message: "refused".to_owned(),
        };
        let (mut out, mut err) = (Vec::new(), Vec::new());

        // when
        let codes = (
            super::refused(Some(&busy), true, &mut out, &mut err),
            super::refused(Some(&other), false, &mut Vec::new(), &mut Vec::new()),
        );

        // then
        assert_eq!(codes, (super::BUSY, super::REFUSED));
        assert!(String::from_utf8_lossy(&out).contains(r#""code": "busy""#));
        assert_eq!(String::from_utf8_lossy(&err), "mayfly: busy: try again\n");
    }

    fn words(text: &str) -> Vec<std::ffi::OsString> {
        text.split(' ').map(Into::into).collect()
    }

    fn operator(text: &str) -> Option<(Target, Request, bool)> {
        match MayflyCommand::parse(words(text)) {
            MayflyCommand::Operator {
                target,
                request,
                json,
            } => Some((target, request, json)),
            _ => None,
        }
    }

    #[test]
    fn should_read_each_operator_request_when_its_words_are_given() {
        // given
        let socket = Target {
            socket: Some("/run/m.sock".into()),
            config: None,
        };

        // when
        let parsed = [
            operator("status --json"),
            operator("--socket /run/m.sock pool pause mac"),
            operator("pool reset-breaker mac --digest sha256:ab --config /etc/m.toml"),
            operator("host drain --deadline 30m"),
            operator("events --since 1h --follow"),
        ];

        // then
        assert_eq!(parsed[0], Some((Target::default(), Request::Status, true)));
        assert_eq!(
            parsed[1],
            Some((
                socket,
                Request::PoolPause {
                    pool: "mac".to_owned()
                },
                false
            ))
        );
        assert_eq!(
            parsed[2],
            Some((
                Target {
                    socket: None,
                    config: Some("/etc/m.toml".into())
                },
                Request::PoolResetBreaker {
                    pool: "mac".to_owned(),
                    digest: Some("sha256:ab".to_owned())
                },
                false
            ))
        );
        assert_eq!(
            parsed[3].as_ref().map(|p| &p.1),
            Some(&Request::HostDrain {
                deadline_secs: Some(1800)
            })
        );
        assert_eq!(
            parsed[4].as_ref().map(|p| &p.1),
            Some(&Request::Events {
                follow: true,
                since_secs: Some(3600)
            })
        );
        assert_eq!(
            MayflyCommand::parse(words("config validate /etc/m.toml")),
            MayflyCommand::ConfigValidate("/etc/m.toml".into())
        );
        assert_eq!(
            MayflyCommand::parse(words("doctor --json")),
            MayflyCommand::Doctor {
                target: Target::default(),
                json: true
            }
        );
        assert_eq!(
            MayflyCommand::parse(words("pool frobnicate mac")),
            MayflyCommand::Usage(MAYFLY_USAGE.to_owned())
        );
        assert!(matches!(
            MayflyCommand::parse(words("events")),
            MayflyCommand::Usage(_)
        ));
        assert!(matches!(
            MayflyCommand::parse(words("host drain --deadline soon")),
            MayflyCommand::Usage(_)
        ));
    }

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
    fn should_read_the_manifest_path_when_the_command_is_image_verify() {
        // given
        let verify = vec![
            "image".into(),
            "verify".into(),
            "/etc/mayfly/mac.toml".into(),
        ];

        // when
        let command = MayflyCommand::parse(verify);

        // then
        assert_eq!(
            command,
            MayflyCommand::ImageVerify("/etc/mayfly/mac.toml".into())
        );
        let usage = MayflyCommand::Usage(IMAGE_USAGE.to_owned());
        assert_eq!(MayflyCommand::parse(vec!["image".into()]), usage);
        assert_eq!(
            MayflyCommand::parse(vec!["image".into(), "pull".into(), "x".into()]),
            usage
        );
        assert_eq!(
            MayflyCommand::parse(vec![
                "image".into(),
                "verify".into(),
                "a".into(),
                "b".into()
            ]),
            usage
        );
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
