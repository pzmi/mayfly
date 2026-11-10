//! A VM's shim directory and what it tells a restarted data plane (ADR 0037): the re-adoption
//! inputs.
//!
//! ```text
//! <shims>/<vm>/            0700, created by the adapter before it spawns the shim
//!   shim.lock              flock(2)ed by the shim for its whole life
//!   shim.json              written atomically once `tart run` is spawned: run id, pids, start
//!   exit.json              written atomically when `tart run` exited: code or signal, times
//!   run.stdout, run.stderr `tart run`'s stdio, files never a pipe, bounded by the shim
//!   shim.log               the shim's own stderr (a refusal before `tart run` is spawned)
//! ```
//!
//! [`read`] turns the directory into the supervisor's [`Shim`] fact: the lock held is `Alive`
//! (the shim lives, and with it its `tart run` child), an `exit.json` is `Exited`, neither is
//! `Lost`, and no directory is `NeverStarted`. A lock is probed with a fresh open file
//! description, so the probe never mistakes the shim's own lock for its own. Pinned by the tests
//! below and `tests/tart/shim.rs::should_read_alive_then_exited_when_the_shim_runs_and_tart_stop_ends_the_run`.

use std::fs::{self, File, OpenOptions};
use std::io::{self, ErrorKind, Write};
use std::path::{Path, PathBuf};

use mayfly_data::Shim;
use rustix::fs::{FlockOperation, flock};
use serde::{Deserialize, Serialize};

/// The lock file.
pub const LOCK: &str = "shim.lock";
/// The record written once `tart run` is spawned.
pub const RECORD: &str = "shim.json";
/// The record written when `tart run` exited.
pub const EXIT: &str = "exit.json";
/// `tart run`'s stdout.
pub const RUN_STDOUT: &str = "run.stdout";
/// `tart run`'s stderr.
pub const RUN_STDERR: &str = "run.stderr";
/// The shim's own stderr.
pub const SHIM_LOG: &str = "shim.log";

/// The records' schema version.
pub const VERSION: u32 = 1;

/// `shim.json`: a run under way.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunRecord {
    /// Schema version.
    pub v: u32,
    /// The run id the adapter gave this start, so a record of an earlier run is never read as
    /// this one's.
    pub run_id: String,
    /// The VM.
    pub vm: String,
    /// The shim's pid.
    pub shim_pid: u32,
    /// `tart run`'s pid: the process that holds the VM's run lock (plan §8.3).
    pub child_pid: u32,
    /// When `tart run` was spawned, milliseconds since the Unix epoch.
    pub started_at_ms: u64,
}

/// `exit.json`: how a run ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExitRecord {
    /// Schema version.
    pub v: u32,
    /// The run it ends.
    pub run_id: String,
    /// The VM.
    pub vm: String,
    /// `tart run`'s exit code; `None` when a signal ended it or it never ran.
    pub code: Option<i32>,
    /// The signal that ended it.
    pub signal: Option<i32>,
    /// Why it never ran (`tart run` could not be spawned), if so.
    pub error: Option<String>,
    /// When it was spawned (or the spawn failed), milliseconds since the Unix epoch.
    pub started_at_ms: u64,
    /// When it ended.
    pub ended_at_ms: u64,
    /// How often the shim truncated `run.stdout` and `run.stderr` to bound them.
    pub truncations: u64,
}

/// What a shim directory says now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShimReading {
    /// The supervisor's fact.
    pub shim: Shim,
    /// The run under way or last started, if recorded.
    pub run: Option<RunRecord>,
    /// How the last run ended, if it did.
    pub exit: Option<ExitRecord>,
}

/// The directory of `vm`'s shim under `shims`.
#[must_use]
pub fn dir(shims: &Path, vm: &str) -> PathBuf {
    shims.join(vm)
}

/// Reads `dir` as the supervisor sees it.
#[must_use]
pub fn read(dir: &Path) -> ShimReading {
    let run = read_json::<RunRecord>(&dir.join(RECORD));
    let exit = read_json::<ExitRecord>(&dir.join(EXIT));
    let shim = match lock_held(&dir.join(LOCK)) {
        Some(true) => Shim::Alive,
        Some(false) | None if exit.is_some() => Shim::Exited,
        Some(false) => Shim::Lost,
        None => Shim::NeverStarted,
    };
    ShimReading { shim, run, exit }
}

/// Whether a live process holds the lock at `path`; `None` when there is no lock file.
fn lock_held(path: &Path) -> Option<bool> {
    let file = File::open(path).ok()?;
    match flock(&file, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => {
            let _released = flock(&file, FlockOperation::Unlock);
            Some(false)
        }
        Err(_) => Some(true),
    }
}

/// A record, or `None` when it is absent or does not parse (a torn file cannot exist: every
/// record is written by [`write_atomically`]).
#[must_use]
pub fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Option<T> {
    let bytes = fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Writes `value` to `path` atomically: a temporary file in the same directory, fsync, rename,
/// then fsync of the directory, so a reader sees the old file or the whole new one.
///
/// # Errors
/// Any I/O failure; the temporary file is removed.
pub fn write_atomically<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| io::Error::new(ErrorKind::InvalidInput, "no parent directory"))?;
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(ErrorKind::InvalidInput, "no file name"))?;
    let mut tmp_name = name.to_os_string();
    tmp_name.push(format!(".tmp-{}", std::process::id()));
    let tmp = dir.join(tmp_name);
    let text = serde_json::to_vec(value).map_err(|e| io::Error::new(ErrorKind::InvalidData, e))?;
    let written = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp)?;
        file.write_all(&text)?;
        file.sync_all()?;
        fs::rename(&tmp, path)?;
        File::open(dir)?.sync_all()
    })();
    if written.is_err() {
        let _gone = fs::remove_file(&tmp);
    }
    written
}

/// Milliseconds since the Unix epoch, for the records' display-only times.
#[must_use]
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exit_record() -> ExitRecord {
        ExitRecord {
            v: VERSION,
            run_id: "r1".into(),
            vm: "vm".into(),
            code: Some(0),
            signal: None,
            error: None,
            started_at_ms: 1,
            ended_at_ms: 2,
            truncations: 0,
        }
    }

    #[test]
    fn should_read_never_started_when_the_directory_is_absent() {
        // given
        let root = tempfile::tempdir().expect("tempdir");

        // when
        let reading = read(&root.path().join("absent"));

        // then
        assert_eq!(reading.shim, Shim::NeverStarted);
    }

    #[test]
    fn should_read_lost_when_the_lock_is_free_and_no_exit_was_written() {
        // given
        let root = tempfile::tempdir().expect("tempdir");
        File::create(root.path().join(LOCK)).expect("lock file");

        // when
        let reading = read(root.path());

        // then
        assert_eq!(reading.shim, Shim::Lost);
    }

    #[test]
    fn should_read_alive_when_another_description_holds_the_lock() {
        // given
        let root = tempfile::tempdir().expect("tempdir");
        let held = File::create(root.path().join(LOCK)).expect("lock file");
        flock(&held, FlockOperation::NonBlockingLockExclusive).expect("lock");
        write_atomically(&root.path().join(EXIT), &exit_record()).expect("exit");

        // when
        let reading = read(root.path());

        // then
        assert_eq!(reading.shim, Shim::Alive);
    }

    #[test]
    fn should_read_exited_with_its_record_when_exit_json_was_written() {
        // given
        let root = tempfile::tempdir().expect("tempdir");
        File::create(root.path().join(LOCK)).expect("lock file");
        write_atomically(&root.path().join(EXIT), &exit_record()).expect("exit");

        // when
        let reading = read(root.path());

        // then
        assert_eq!(reading.shim, Shim::Exited);
        assert_eq!(reading.exit, Some(exit_record()));
        let leftovers: Vec<_> = fs::read_dir(root.path())
            .expect("dir")
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp-"))
            .collect();
        assert!(leftovers.is_empty());
    }
}
