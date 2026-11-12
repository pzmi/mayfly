//! Running `tart`, `security`, `cargo` and the other children of a run: argv only (never a
//! shell), an explicit environment (the caller's is cleared), each child in its own process group
//! so Ctrl-C reaches `hw01` and not the child, a deadline after which the whole group is killed,
//! and output kept to a bound while the rest is drained and dropped.

use std::ffi::OsString;
use std::io::{Read, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use rustix::process::{Pid, Signal, kill_process_group};

/// How often a running child is looked at.
const TICK: Duration = Duration::from_millis(20);

/// One child to run to completion.
#[derive(Debug, Clone)]
pub struct Call {
    /// The program.
    pub program: PathBuf,
    /// Its arguments.
    pub args: Vec<OsString>,
    /// Its whole environment.
    pub env: Vec<(OsString, OsString)>,
    /// Bytes written to its stdin, then closed; `None` gives it no stdin.
    pub stdin: Option<Vec<u8>>,
    /// When it is killed.
    pub deadline: Duration,
    /// How many bytes of stdout and of stderr are kept.
    pub limit: usize,
    /// Run in the terminal's process group with the terminal's stdin (`sudo -v` asks there).
    pub foreground: bool,
    /// Send stdout and stderr to these files instead of pipes (read back up to `limit`).
    pub to_files: Option<(PathBuf, PathBuf)>,
    /// The working directory (`cargo` runs in the checkout).
    pub cwd: Option<PathBuf>,
}

impl Call {
    /// `program args…` with `env`, no stdin, a 64 KiB bound.
    pub fn new(
        program: &Path,
        args: &[&str],
        env: &[(OsString, OsString)],
        deadline: Duration,
    ) -> Self {
        Self {
            program: program.to_path_buf(),
            args: args.iter().map(OsString::from).collect(),
            env: env.to_vec(),
            stdin: None,
            deadline,
            limit: 64 << 10,
            foreground: false,
            to_files: None,
            cwd: None,
        }
    }

    /// The same call with `stdin` written to the child.
    #[must_use]
    pub fn input(mut self, stdin: &[u8]) -> Self {
        self.stdin = Some(stdin.to_vec());
        self
    }
}

/// How a call ended.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Finished {
    /// The exit code; `None` when a signal ended it.
    pub code: Option<i32>,
    /// The first `limit` bytes of stdout.
    pub stdout: Vec<u8>,
    /// The first `limit` bytes of stderr.
    pub stderr: Vec<u8>,
    /// Killed at its deadline.
    pub timed_out: bool,
    /// Killed because the run was interrupted.
    pub interrupted: bool,
}

impl Finished {
    /// Exit 0, neither killed nor interrupted.
    pub fn ok(&self) -> bool {
        self.code == Some(0)
    }

    /// stdout as text, trimmed.
    pub fn out(&self) -> String {
        String::from_utf8_lossy(&self.stdout).trim().to_owned()
    }

    /// stderr as text, trimmed.
    pub fn err(&self) -> String {
        String::from_utf8_lossy(&self.stderr).trim().to_owned()
    }

    /// Exit code (or how it ended) and stderr, bounded, for evidence.
    pub fn summary(&self) -> String {
        let how = match (self.code, self.timed_out, self.interrupted) {
            (_, true, _) => "killed at its deadline".to_owned(),
            (_, _, true) => "killed: interrupted".to_owned(),
            (Some(code), _, _) => format!("exit {code}"),
            (None, _, _) => "ended by a signal".to_owned(),
        };
        let err = self.err();
        let err: String = err.chars().take(400).collect();
        if err.is_empty() {
            how
        } else {
            format!("{how}: {err}")
        }
    }
}

fn drain(mut from: impl Read + Send + 'static, limit: usize) -> thread::JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut kept = Vec::new();
        let mut buf = [0u8; 8192];
        loop {
            match from.read(&mut buf) {
                Ok(0) | Err(_) => return kept,
                Ok(n) => {
                    let room = limit.saturating_sub(kept.len());
                    kept.extend_from_slice(&buf[..n.min(room)]);
                }
            }
        }
    })
}

/// Kills `child`'s whole process group (it leads one) and reaps it.
pub fn kill_group(child: &mut Child) {
    let _gone = kill_process_group(Pid::from_child(child), Signal::KILL);
    let _killed = child.kill();
    let _reaped = child.wait();
}

/// Runs `call` to its end, its deadline or the run's interruption, whichever comes first.
///
/// # Errors
/// The program could not be started.
pub fn run(call: &Call, interrupt: &AtomicBool) -> std::io::Result<Finished> {
    let mut command = Command::new(&call.program);
    command
        .args(&call.args)
        .env_clear()
        .envs(call.env.iter().map(|(k, v)| (k, v)));
    if let Some(cwd) = &call.cwd {
        command.current_dir(cwd);
    }
    if let Some((out, err)) = &call.to_files {
        command
            .stdout(std::fs::File::create(out)?)
            .stderr(std::fs::File::create(err)?);
    } else {
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
    }
    if call.foreground {
        command.stdin(Stdio::inherit());
    } else {
        command
            .stdin(if call.stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .process_group(0);
    }
    let mut child = command.spawn()?;
    let writer = match (child.stdin.take(), call.stdin.clone()) {
        (Some(mut pipe), Some(bytes)) => Some(thread::spawn(move || {
            let _written = pipe.write_all(&bytes);
        })),
        _ => None,
    };
    let stdout = child.stdout.take().map(|s| drain(s, call.limit));
    let stderr = child.stderr.take().map(|s| drain(s, call.limit));
    let started = Instant::now();
    let mut finished = Finished::default();
    loop {
        if let Some(status) = child.try_wait()? {
            finished.code = status.code();
            break;
        }
        if interrupt.load(Ordering::SeqCst) && !call.foreground {
            finished.interrupted = true;
        } else if started.elapsed() >= call.deadline {
            finished.timed_out = true;
        }
        if finished.interrupted || finished.timed_out {
            kill_group(&mut child);
            break;
        }
        thread::sleep(TICK);
    }
    if let Some(writer) = writer {
        let _joined = writer.join();
    }
    finished.stdout = stdout.and_then(|h| h.join().ok()).unwrap_or_default();
    finished.stderr = stderr.and_then(|h| h.join().ok()).unwrap_or_default();
    if let Some((out, err)) = &call.to_files {
        let read = |path: &Path| {
            let mut kept = std::fs::read(path).unwrap_or_default();
            kept.truncate(call.limit);
            kept
        };
        finished.stdout = read(out);
        finished.stderr = read(err);
    }
    Ok(finished)
}

/// Starts `program args…` in the background in its own process group, with no stdin and its
/// output appended to `log`.
///
/// # Errors
/// The log could not be opened or the program could not be started.
pub fn spawn(
    program: &Path,
    args: &[&str],
    env: &[(OsString, OsString)],
    log: &Path,
    cwd: &Path,
) -> std::io::Result<Child> {
    let out = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)?;
    let err = out.try_clone()?;
    Command::new(program)
        .args(args)
        .env_clear()
        .envs(env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::null())
        .stdout(out)
        .stderr(err)
        .current_dir(cwd)
        .process_group(0)
        .spawn()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_kill_the_group_when_a_child_passes_its_deadline() {
        // given
        let call =
            Call::new(Path::new("/bin/cat"), &[], &[], Duration::from_millis(200)).input(b"");
        let mut hung = call.clone();
        hung.stdin = None;
        hung.program = PathBuf::from("/bin/sleep");
        hung.args = vec!["30".into()];
        let quiet = AtomicBool::new(false);

        // when
        let done = run(&hung, &quiet).expect("started");
        let echoed = run(&call.input(b"abc"), &quiet).expect("started");

        // then
        assert!(done.timed_out && !done.ok(), "{done:?}");
        assert_eq!(
            (echoed.code, echoed.stdout.as_slice()),
            (Some(0), &b"abc"[..])
        );
    }

    #[test]
    fn should_keep_only_the_bound_when_a_child_prints_more() {
        // given
        let mut call = Call::new(Path::new("/bin/cat"), &[], &[], Duration::from_secs(10))
            .input(&vec![b'x'; 100_000]);
        call.limit = 10;

        // when
        let done = run(&call, &AtomicBool::new(false)).expect("started");

        // then
        assert_eq!(done.stdout.len(), 10);
        assert!(done.ok());
    }

    #[test]
    fn should_stop_a_child_when_the_run_is_interrupted() {
        // given
        let call = Call::new(
            Path::new("/bin/sleep"),
            &["30"],
            &[],
            Duration::from_secs(60),
        );
        let interrupted = AtomicBool::new(true);

        // when
        let done = run(&call, &interrupted).expect("started");

        // then
        assert!(done.interrupted && !done.ok(), "{done:?}");
    }
}
