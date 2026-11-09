//! The state directory and its single-instance lock (plan §13, ADR 0031, F19).
//!
//! One daemon owns a state dir at a time: the journal's epoch and the single-owner rule
//! (event-store §6) both rest on it. The lock is `flock(2)` on `<state_dir>/mayflyd.lock`:
//! the kernel drops it when the holder dies, however it dies, so a crash never leaves a stale
//! lock and nothing needs a pid check or a cleanup. Rejected alternatives: a pid file (stale
//! after a crash, racy to take over) and `fcntl` record locks (dropped when any descriptor of
//! the file closes in the process).
//!
//! The directory must be owned by the service user and `0700`; the lock file `0600`. A wider
//! mode is refused with the command that fixes it, never repaired silently: the operator
//! chooses (philosophy §4). A missing directory is created `0700`.
//!
//! The lock file is never deleted: removing a locked file lets a second instance lock a new
//! one. The descriptor is close-on-exec, so no child process (Tart, a secret command) inherits
//! the lock.

use std::fmt;
use std::fs::{DirBuilder, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use rustix::fs::{FlockOperation, OFlags, flock};
use rustix::io::Errno;

use crate::exit_code;

/// The lock file's name under the state dir. Never renamed (compatibility rule): an older
/// and a newer build must contend on the same file.
pub const LOCK_FILE_NAME: &str = "mayflyd.lock";

/// The mode of the state dir.
const DIR_MODE: u32 = 0o700;
/// The mode of the lock file.
const FILE_MODE: u32 = 0o600;

/// The lock on a state dir, held until dropped (or the process ends).
#[derive(Debug)]
pub struct StateDirLock {
    state_dir: PathBuf,
    _file: File,
}

/// Why the state dir was refused.
#[derive(Debug)]
pub enum StateDirError {
    /// Another instance holds the lock.
    Locked {
        /// The lock file contended on.
        lock_file: PathBuf,
        /// The holder's pid as it wrote it; advisory, `None` when unreadable.
        holder: Option<u32>,
    },
    /// The state dir path exists and is not a directory.
    NotADirectory {
        /// The path.
        path: PathBuf,
    },
    /// The path is owned by another user.
    NotOwned {
        /// The path.
        path: PathBuf,
        /// Its owner's uid.
        owner: u32,
        /// The daemon's effective uid.
        daemon: u32,
    },
    /// The path's mode is wider than allowed.
    BadMode {
        /// The path.
        path: PathBuf,
        /// Its mode bits (permissions only).
        mode: u32,
        /// The mode it must have: `0700` for the dir, `0600` for the lock file.
        required: u32,
    },
    /// The lock file exists and is not a regular file.
    LockFileNotRegular {
        /// The path.
        path: PathBuf,
    },
    /// An operating-system call failed.
    Io {
        /// What was being done.
        action: &'static str,
        /// The path.
        path: PathBuf,
        /// The operating system's reason.
        source: std::io::Error,
    },
}

impl StateDirError {
    /// The exit code the daemon ends with: [`exit_code::LOCK_REFUSED`] when another instance
    /// holds the lock, [`exit_code::STATE_DIR_UNUSABLE`] for every other refusal (ADR 0031).
    /// Pinned by `tests/state_dir.rs::should_use_distinct_exit_codes_when_each_refusal_is_mapped`.
    #[must_use]
    pub const fn exit_code(&self) -> u8 {
        match self {
            Self::Locked { .. } => exit_code::LOCK_REFUSED,
            _ => exit_code::STATE_DIR_UNUSABLE,
        }
    }
}

impl fmt::Display for StateDirError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Locked { lock_file, holder } => {
                write!(f, "another mayflyd holds {}", lock_file.display())?;
                if let Some(pid) = holder {
                    write!(f, " (pid {pid})")?;
                }
                f.write_str(": stop it first; two daemons must never share one state dir")
            }
            Self::NotADirectory { path } => write!(
                f,
                "state dir {} exists and is not a directory: point `state_dir` at a directory",
                path.display()
            ),
            Self::NotOwned {
                path,
                owner,
                daemon,
            } => write!(
                f,
                "{} is owned by uid {owner}, not by the daemon's uid {daemon}: \
                 run `chown` to the service user, or run the daemon as its owner",
                path.display()
            ),
            Self::BadMode {
                path,
                mode,
                required,
            } => write!(
                f,
                "{} has mode {mode:04o}; it must be {required:04o} (no group or other access): \
                 run `chmod {required:o} {}`",
                path.display(),
                path.display()
            ),
            Self::LockFileNotRegular { path } => write!(
                f,
                "{} is not a regular file: remove it and start again",
                path.display()
            ),
            Self::Io {
                action,
                path,
                source,
            } => {
                write!(f, "cannot {action} {}: {source}", path.display())
            }
        }
    }
}

impl std::error::Error for StateDirError {}

/// What is wrong with a path's owner and mode, judged from its numbers alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fault {
    NotOwned { owner: u32 },
    BadMode { mode: u32 },
}

/// Judges an owner and a mode against the daemon's uid and the mode the path must have: the
/// owner must be the daemon, no permission bit beyond `required` may be set, and the owner's
/// bits of `required` must all be set (a directory without owner access cannot be used).
/// Pinned by `tests/state_dir.rs::should_refuse_a_path_when_its_owner_or_mode_is_wrong`.
fn judge(owner: u32, mode: u32, daemon: u32, required: u32) -> Option<Fault> {
    if owner != daemon {
        return Some(Fault::NotOwned { owner });
    }
    let bits = mode & 0o777;
    let beyond = bits & !required != 0;
    let owner_access = required & 0o700;
    let lacks_owner_access = bits & owner_access != owner_access;
    (beyond || lacks_owner_access).then_some(Fault::BadMode { mode: bits })
}

fn io(action: &'static str, path: &Path) -> impl FnOnce(std::io::Error) -> StateDirError {
    let path = path.to_path_buf();
    move |source| StateDirError::Io {
        action,
        path,
        source,
    }
}

fn refuse(path: &Path, fault: Fault, daemon: u32, required: u32) -> StateDirError {
    match fault {
        Fault::NotOwned { owner } => StateDirError::NotOwned {
            path: path.to_path_buf(),
            owner,
            daemon,
        },
        Fault::BadMode { mode } => StateDirError::BadMode {
            path: path.to_path_buf(),
            mode,
            required,
        },
    }
}

impl StateDirLock {
    /// Prepares `state_dir` and takes its lock without waiting.
    ///
    /// A missing directory is created `0700`; an existing one must be a directory owned by
    /// the daemon's user with mode `0700`; the lock file is created `0600` or, when it
    /// exists, must be a regular file of the daemon's user with no group or other access.
    /// The lock file is opened without following a symlink.
    ///
    /// # Errors
    ///
    /// [`StateDirError::Locked`] when another instance holds the lock (also when two
    /// acquisitions race: exactly one wins, pinned by
    /// `tests/state_dir.rs::should_refuse_the_second_instance_when_the_first_holds_the_lock`),
    /// another variant when the directory or file is unsafe or an operating-system call fails.
    pub fn acquire(state_dir: &Path) -> Result<Self, StateDirError> {
        let daemon = rustix::process::geteuid().as_raw();
        prepare_dir(state_dir, daemon)?;

        let lock_path = state_dir.join(LOCK_FILE_NAME);
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(FILE_MODE)
            .custom_flags(OFlags::NOFOLLOW.bits().cast_signed())
            .open(&lock_path)
            .map_err(io("open", &lock_path))?;
        let meta = file.metadata().map_err(io("inspect", &lock_path))?;
        if !meta.is_file() {
            return Err(StateDirError::LockFileNotRegular { path: lock_path });
        }
        if let Some(fault) = judge(meta.uid(), meta.mode(), daemon, FILE_MODE) {
            return Err(refuse(&lock_path, fault, daemon, FILE_MODE));
        }

        match flock(&file, FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => {}
            Err(Errno::WOULDBLOCK) => {
                return Err(StateDirError::Locked {
                    holder: read_holder(&lock_path),
                    lock_file: lock_path,
                });
            }
            Err(errno) => return Err(io("lock", &lock_path)(errno.into())),
        }

        file.set_len(0).map_err(io("write", &lock_path))?;
        writeln!(file, "{}", std::process::id()).map_err(io("write", &lock_path))?;
        Ok(Self {
            state_dir: state_dir.to_path_buf(),
            _file: file,
        })
    }

    /// The state dir this lock guards.
    #[must_use]
    pub fn state_dir(&self) -> &Path {
        &self.state_dir
    }
}

fn prepare_dir(state_dir: &Path, daemon: u32) -> Result<(), StateDirError> {
    let meta = match std::fs::metadata(state_dir) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            DirBuilder::new()
                .recursive(true)
                .mode(DIR_MODE)
                .create(state_dir)
                .map_err(io("create", state_dir))?;
            std::fs::metadata(state_dir).map_err(io("inspect", state_dir))?
        }
        Err(error) => return Err(io("inspect", state_dir)(error)),
    };
    if !meta.is_dir() {
        return Err(StateDirError::NotADirectory {
            path: state_dir.to_path_buf(),
        });
    }
    match judge(meta.uid(), meta.mode(), daemon, DIR_MODE) {
        Some(fault) => Err(refuse(state_dir, fault, daemon, DIR_MODE)),
        None => Ok(()),
    }
}

/// The holder's pid as it wrote it, when readable. Advisory: the lock, not this file's text,
/// is the truth, and a reader never acts on the number.
fn read_holder(lock_path: &Path) -> Option<u32> {
    let mut text = String::new();
    File::open(lock_path)
        .ok()?
        .take(32)
        .read_to_string(&mut text)
        .ok()?;
    text.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::{Fault, judge};

    const DAEMON: u32 = 501;

    #[test]
    fn should_accept_a_path_when_owner_and_mode_are_exact() {
        // given
        let (dir, file) = (0o040_700, 0o100_600);
        // when
        let verdicts = [
            judge(DAEMON, dir, DAEMON, 0o700),
            judge(DAEMON, file, DAEMON, 0o600),
        ];
        // then
        assert_eq!(verdicts, [None, None]);
    }

    #[test]
    fn should_refuse_a_path_when_its_owner_or_mode_is_wrong() {
        // given
        let cases = [
            (
                DAEMON + 1,
                0o700,
                0o700,
                Some(Fault::NotOwned { owner: DAEMON + 1 }),
            ),
            (DAEMON, 0o755, 0o700, Some(Fault::BadMode { mode: 0o755 })),
            (DAEMON, 0o770, 0o700, Some(Fault::BadMode { mode: 0o770 })),
            (DAEMON, 0o500, 0o700, Some(Fault::BadMode { mode: 0o500 })),
            (DAEMON, 0o644, 0o600, Some(Fault::BadMode { mode: 0o644 })),
            (DAEMON, 0o400, 0o600, Some(Fault::BadMode { mode: 0o400 })),
            (DAEMON, 0o700, 0o600, Some(Fault::BadMode { mode: 0o700 })),
        ];
        for (owner, mode, required, expected) in cases {
            // when
            let verdict = judge(owner, mode, DAEMON, required);
            // then
            assert_eq!(verdict, expected, "mode {mode:o} against {required:o}");
        }
    }
}
