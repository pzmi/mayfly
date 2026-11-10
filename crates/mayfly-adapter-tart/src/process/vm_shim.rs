//! The body of `mayfly vm-shim` (ADR 0037): owns one VM's `tart run` process.
//!
//! ```text
//! mayfly vm-shim --tart <path> --vm <name> --dir <shim dir> --run-id <id> --log-limit <bytes>
//! ```
//!
//! In order: leave the daemon's session (`setsid`), so neither a signal to the daemon's group
//! nor the daemon's death reaches the VM; take the directory's lock (waiting briefly, never
//! stealing it) and hold it for life; drop an earlier run's `exit.json`; spawn `tart run <vm>
//! --no-graphics` as the only child, with a cleared environment plus the allowlist it was given
//! (ADR 0058), a null stdin and stdio to `run.stdout`/`run.stderr`, files never a pipe, so a
//! dead reader can never kill the VM by SIGPIPE; record `shim.json`; wait for the child,
//! truncating a stdio file that passes the limit; write `exit.json` atomically; exit, which
//! releases the lock. The shim never signals its child: the VM is stopped only by `tart stop`.
//!
//! Exit codes: 0 once `exit.json` is written (whatever the run's own status), 64 for a usage
//! error, 71 when the session cannot be left, 75 when another shim holds the lock, 74 for an
//! I/O failure before `tart run` is spawned. Pinned by `tests/tart/shim.rs` (the files, the
//! exit record, the stdio files and survival of a killed daemon) and the tests below.

use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use mayfly_domain::OwnedVmName;
use rustix::fs::{FlockOperation, flock};

use crate::config::{NO_AUTO_PRUNE, TART_HOME, allowed};
use crate::shim::{
    EXIT, ExitRecord, LOCK, RECORD, RUN_STDERR, RUN_STDOUT, RunRecord, SPAWN_FAILED, VERSION,
    WAIT_FAILED, now_ms, write_atomically,
};

/// Usage error.
pub const EXIT_USAGE: i32 = 64;
/// I/O failure before `tart run` was spawned.
pub const EXIT_IO: i32 = 74;
/// The session could not be left.
pub const EXIT_SESSION: i32 = 71;
/// Another shim holds the lock.
pub const EXIT_LOCKED: i32 = 75;

/// How often the shim looks at its child and its stdio files.
const TICK: Duration = Duration::from_millis(100);

/// How long the shim waits for a lock another process holds (a probe holds it for an instant).
const LOCK_WAIT: Duration = Duration::from_secs(2);

/// The shim's arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShimArgs {
    /// The `tart` executable.
    pub tart: PathBuf,
    /// The VM: only a managed name, so the shim can never be pointed at a base or a foreign VM.
    pub vm: OwnedVmName,
    /// The shim directory.
    pub dir: PathBuf,
    /// This start's run id.
    pub run_id: String,
    /// The most bytes each stdio file may hold.
    pub log_limit: u64,
}

impl ShimArgs {
    /// The argv the adapter passes after the shim program's own arguments.
    #[must_use]
    pub fn to_args(&self) -> Vec<OsString> {
        vec![
            "--tart".into(),
            self.tart.clone().into_os_string(),
            "--vm".into(),
            self.vm.to_string().into(),
            "--dir".into(),
            self.dir.clone().into_os_string(),
            "--run-id".into(),
            self.run_id.clone().into(),
            "--log-limit".into(),
            self.log_limit.to_string().into(),
        ]
    }

    /// Parses what [`ShimArgs::to_args`] wrote; every flag once, nothing else.
    ///
    /// # Errors
    /// The first problem, as a message.
    pub fn parse(args: &[OsString]) -> Result<Self, String> {
        let mut tart = None;
        let mut vm = None;
        let mut dir = None;
        let mut run_id = None;
        let mut log_limit = None;
        let mut iter = args.iter();
        while let Some(flag) = iter.next() {
            let value = iter
                .next()
                .ok_or_else(|| format!("{} needs a value", flag.to_string_lossy()))?;
            let slot = match flag.to_str() {
                Some("--tart") => &mut tart,
                Some("--vm") => &mut vm,
                Some("--dir") => &mut dir,
                Some("--run-id") => &mut run_id,
                Some("--log-limit") => &mut log_limit,
                _ => return Err(format!("unknown argument {}", flag.to_string_lossy())),
            };
            if slot.replace(value.clone()).is_some() {
                return Err(format!("{} given twice", flag.to_string_lossy()));
            }
        }
        let text = |value: Option<OsString>, name: &str| {
            value
                .and_then(|v| v.into_string().ok())
                .filter(|v| !v.is_empty())
                .ok_or_else(|| format!("--{name} is required"))
        };
        let tart = PathBuf::from(tart.ok_or("--tart is required")?);
        let dir = PathBuf::from(dir.ok_or("--dir is required")?);
        if !tart.is_absolute() || !dir.is_absolute() {
            return Err("--tart and --dir must be absolute".to_owned());
        }
        let vm = OwnedVmName::parse(&text(vm, "vm")?)
            .map_err(|_| "--vm is not a managed VM name".to_owned())?;
        Ok(Self {
            tart,
            vm,
            dir,
            run_id: text(run_id, "run-id")?,
            log_limit: text(log_limit, "log-limit")?
                .parse()
                .map_err(|_| "--log-limit is not a number".to_owned())?,
        })
    }
}

/// Runs the shim; returns its exit code. Diagnostics go to `stderr` (the shim's `shim.log`).
pub fn main(args: &[OsString], stderr: &mut dyn Write) -> i32 {
    let args = match ShimArgs::parse(args) {
        Ok(args) => args,
        Err(why) => {
            let _shown = writeln!(stderr, "mayfly vm-shim: {why}");
            return EXIT_USAGE;
        }
    };
    if let Err(err) = rustix::process::setsid() {
        let _shown = writeln!(stderr, "mayfly vm-shim: cannot leave the session: {err}");
        return EXIT_SESSION;
    }
    let lock = match take_lock(&args.dir.join(LOCK)) {
        Ok(Some(lock)) => lock,
        Ok(None) => {
            let _shown = writeln!(stderr, "mayfly vm-shim: another shim holds the lock");
            return EXIT_LOCKED;
        }
        Err(err) => {
            let _shown = writeln!(stderr, "mayfly vm-shim: cannot open the lock: {err}");
            return EXIT_IO;
        }
    };
    let code = supervise(&args, stderr);
    drop(lock);
    code
}

/// Opens and locks `path`, waiting up to [`LOCK_WAIT`]; `None` when another process keeps it.
fn take_lock(path: &Path) -> std::io::Result<Option<File>> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    let started = std::time::Instant::now();
    loop {
        match flock(&file, FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => return Ok(Some(file)),
            Err(_) if started.elapsed() < LOCK_WAIT => std::thread::sleep(TICK / 10),
            Err(_) => return Ok(None),
        }
    }
}

/// Opens a stdio file for `tart run`, empty, appending.
fn stdio_file(path: &Path) -> std::io::Result<File> {
    let file = OpenOptions::new().append(true).create(true).open(path)?;
    file.set_len(0)?;
    Ok(file)
}

/// The environment `tart run` gets: the shim's own, which the adapter already cleared, filtered
/// once more to the allowlist.
fn child_environment() -> Vec<(OsString, OsString)> {
    std::env::vars_os()
        .filter(|(name, _)| {
            name == OsStr::new(TART_HOME) || name == OsStr::new(NO_AUTO_PRUNE) || allowed(name)
        })
        .collect()
}

fn supervise(args: &ShimArgs, stderr: &mut dyn Write) -> i32 {
    match fs::remove_file(args.dir.join(EXIT)) {
        Ok(()) => {}
        Err(err) if err.kind() == ErrorKind::NotFound => {}
        Err(err) => {
            let _shown = writeln!(stderr, "mayfly vm-shim: cannot clear exit.json: {err}");
            return EXIT_IO;
        }
    }
    let stdout_path = args.dir.join(RUN_STDOUT);
    let stderr_path = args.dir.join(RUN_STDERR);
    let files = stdio_file(&stdout_path).and_then(|o| Ok((o, stdio_file(&stderr_path)?)));
    let (out, err) = match files {
        Ok(files) => files,
        Err(err) => {
            let _shown = writeln!(stderr, "mayfly vm-shim: cannot open the run's stdio: {err}");
            return EXIT_IO;
        }
    };
    let started_at_ms = now_ms();
    let mut exit = ExitRecord {
        v: VERSION,
        run_id: args.run_id.clone(),
        vm: args.vm.to_string(),
        code: None,
        signal: None,
        error: None,
        started_at_ms,
        ended_at_ms: started_at_ms,
        truncations: 0,
    };
    let vm = args.vm.to_string();
    let spawned = Command::new(&args.tart)
        .env_clear()
        .envs(child_environment())
        .args(["run", vm.as_str(), "--no-graphics"])
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(out)
        .stderr(err)
        .spawn();
    match spawned {
        Ok(mut child) => {
            let record = RunRecord {
                v: VERSION,
                run_id: args.run_id.clone(),
                vm: args.vm.to_string(),
                shim_pid: std::process::id(),
                child_pid: child.id(),
                started_at_ms,
            };
            if let Err(err) = write_atomically(&args.dir.join(RECORD), &record) {
                let _shown = writeln!(stderr, "mayfly vm-shim: cannot write shim.json: {err}");
            }
            let (ended, truncations) = wait(&mut child, &[&stdout_path, &stderr_path], args);
            match ended {
                Some((code, signal)) => (exit.code, exit.signal) = (code, signal),
                None => exit.error = Some(WAIT_FAILED.to_owned()),
            }
            exit.truncations = truncations;
        }
        Err(err) => exit.error = Some(format!("{SPAWN_FAILED}:{:?}", err.kind())),
    }
    exit.ended_at_ms = now_ms();
    if let Err(err) = write_atomically(&args.dir.join(EXIT), &exit) {
        let _shown = writeln!(stderr, "mayfly vm-shim: cannot write exit.json: {err}");
        return EXIT_IO;
    }
    0
}

/// The exit code and signal of a child that ended.
type Ended = (Option<i32>, Option<i32>);

/// Waits for `child`, truncating each of `files` that passes the limit; returns how it ended
/// (`None` when it cannot be waited for, so its end is unknown) and the number of truncations.
/// Never signals the child.
fn wait(child: &mut Child, files: &[&Path], args: &ShimArgs) -> (Option<Ended>, u64) {
    let mut truncations = 0;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return (Some((status.code(), status.signal())), truncations),
            Ok(None) => {}
            Err(_) => return (None, truncations),
        }
        for path in files {
            let over = fs::metadata(path).is_ok_and(|m| m.len() > args.log_limit);
            if over
                && OpenOptions::new()
                    .write(true)
                    .open(path)
                    .and_then(|f| f.set_len(0))
                    .is_ok()
            {
                truncations += 1;
            }
        }
        std::thread::sleep(TICK);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args() -> ShimArgs {
        ShimArgs {
            tart: "/opt/tart".into(),
            vm: OwnedVmName::parse("mf-pool0-host0-s1-g1-00000001").expect("a managed name"),
            dir: "/var/shims/mf-a".into(),
            run_id: "7-1".into(),
            log_limit: 1024,
        }
    }

    #[test]
    fn should_round_trip_when_the_adapter_writes_the_arguments() {
        // given
        let written = args().to_args();

        // when
        let parsed = ShimArgs::parse(&written);

        // then
        assert_eq!(parsed, Ok(args()));
    }

    #[test]
    fn should_refuse_when_the_arguments_are_wrong() {
        // given
        let mut relative = args();
        relative.tart = "tart".into();
        let mut base_vm = args().to_args();
        base_vm[3] = "mfb-base".into();
        let mut twice = args().to_args();
        twice.extend(["--vm".into(), "mf-b".into()]);
        let cases: Vec<Vec<OsString>> = vec![
            Vec::new(),
            vec!["--vm".into()],
            vec!["--bogus".into(), "x".into()],
            relative.to_args(),
            base_vm,
            twice,
        ];

        for case in cases {
            // when
            let parsed = ShimArgs::parse(&case);

            // then
            assert!(parsed.is_err(), "{case:?}");
        }
    }

    #[test]
    fn should_exit_with_the_usage_code_when_the_arguments_do_not_parse() {
        // given
        let mut stderr = Vec::new();

        // when
        let code = main(&["--bogus".into(), "x".into()], &mut stderr);

        // then
        assert_eq!(code, EXIT_USAGE);
        assert!(String::from_utf8_lossy(&stderr).contains("unknown argument"));
    }
}
