//! The state-dir lock through the public API (plan §13, ADR 0031, F19): one instance wins,
//! the rest are refused with a distinct code, and an unsafe directory or lock file is refused
//! with the command that fixes it.

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::Path;
use std::sync::Barrier;

use mayfly_composition::exit_code;
use mayfly_composition::state_dir::{LOCK_FILE_NAME, StateDirError, StateDirLock};

fn set_mode(path: &Path, mode: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

fn mode_of(path: &Path) -> u32 {
    fs::metadata(path).unwrap().mode() & 0o777
}

fn private_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    set_mode(dir.path(), 0o700);
    dir
}

#[test]
fn should_create_the_state_dir_and_lock_file_private_when_the_dir_is_missing() {
    // given
    let root = private_dir();
    let state = root.path().join("a").join("state");
    // when
    let lock = StateDirLock::acquire(&state).unwrap();
    // then
    assert_eq!(mode_of(&state), 0o700);
    assert_eq!(mode_of(&state.join(LOCK_FILE_NAME)), 0o600);
    assert_eq!(lock.state_dir(), state);
}

#[test]
fn should_write_the_holder_pid_when_the_lock_is_taken() {
    // given
    let root = private_dir();
    // when
    let _lock = StateDirLock::acquire(root.path()).unwrap();
    // then
    let text = fs::read_to_string(root.path().join(LOCK_FILE_NAME)).unwrap();
    assert_eq!(text, format!("{}\n", std::process::id()));
}

#[test]
fn should_refuse_the_second_instance_when_the_first_holds_the_lock() {
    // given
    let root = private_dir();
    let first = StateDirLock::acquire(root.path()).unwrap();
    // when
    let second = StateDirLock::acquire(root.path()).unwrap_err();
    // then
    assert!(matches!(
        second,
        StateDirError::Locked { holder: Some(pid), .. } if pid == std::process::id()
    ));
    let shown = second.to_string();
    assert!(shown.contains("another mayflyd holds"), "{shown}");
    assert!(shown.contains(&std::process::id().to_string()), "{shown}");
    drop(first);
}

#[test]
fn should_let_exactly_one_instance_win_when_many_race_for_the_lock() {
    // given
    let root = private_dir();
    let racers = 8;
    let start = Barrier::new(racers);
    // when
    let results: Vec<_> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..racers)
            .map(|_| {
                scope.spawn(|| {
                    start.wait();
                    StateDirLock::acquire(root.path())
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    // then
    let won = results.iter().filter(|r| r.is_ok()).count();
    let refused = results
        .iter()
        .filter(|r| matches!(r, Err(StateDirError::Locked { .. })))
        .count();
    assert_eq!((won, refused), (1, racers - 1));
}

#[test]
fn should_release_the_lock_when_the_holder_is_dropped() {
    // given
    let root = private_dir();
    drop(StateDirLock::acquire(root.path()).unwrap());
    // when
    let again = StateDirLock::acquire(root.path());
    // then
    assert!(again.is_ok());
}

#[test]
fn should_refuse_the_dir_when_its_mode_is_wider_than_0700() {
    // given
    let root = private_dir();
    set_mode(root.path(), 0o755);
    // when
    let error = StateDirLock::acquire(root.path()).unwrap_err();
    // then
    assert!(matches!(
        error,
        StateDirError::BadMode {
            mode: 0o755,
            required: 0o700,
            ..
        }
    ));
    let shown = error.to_string();
    assert!(
        shown.contains("0755") && shown.contains("chmod 700"),
        "{shown}"
    );
    assert!(!root.path().join(LOCK_FILE_NAME).exists());
}

#[test]
fn should_refuse_the_lock_file_when_its_mode_is_wider_than_0600() {
    // given
    let root = private_dir();
    let lock_file = root.path().join(LOCK_FILE_NAME);
    fs::write(&lock_file, "").unwrap();
    set_mode(&lock_file, 0o644);
    // when
    let error = StateDirLock::acquire(root.path()).unwrap_err();
    // then
    assert!(matches!(
        error,
        StateDirError::BadMode {
            mode: 0o644,
            required: 0o600,
            ..
        }
    ));
}

#[test]
fn should_refuse_the_path_when_it_is_a_file() {
    // given
    let root = private_dir();
    let file = root.path().join("not-a-dir");
    fs::write(&file, "").unwrap();
    // when
    let error = StateDirLock::acquire(&file).unwrap_err();
    // then
    assert!(matches!(error, StateDirError::NotADirectory { .. }));
}

#[test]
fn should_refuse_the_lock_file_when_it_is_a_symlink() {
    // given
    let root = private_dir();
    let target = root.path().join("elsewhere");
    fs::write(&target, "").unwrap();
    symlink(&target, root.path().join(LOCK_FILE_NAME)).unwrap();
    // when
    let error = StateDirLock::acquire(root.path()).unwrap_err();
    // then
    assert!(matches!(error, StateDirError::Io { action: "open", .. }));
    assert!(error.to_string().starts_with("cannot open"), "{error}");
}

// `mknodat` is not offered on macOS targets; the branch it reaches is platform-neutral.
#[cfg(target_os = "linux")]
#[test]
fn should_refuse_the_lock_file_when_it_is_not_a_regular_file() {
    // given
    let root = private_dir();
    rustix::fs::mknodat(
        rustix::fs::CWD,
        root.path().join(LOCK_FILE_NAME),
        rustix::fs::FileType::Fifo,
        rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
        0,
    )
    .unwrap();
    // when
    let error = StateDirLock::acquire(root.path()).unwrap_err();
    // then
    assert!(matches!(error, StateDirError::LockFileNotRegular { .. }));
}

#[test]
fn should_use_distinct_exit_codes_when_each_refusal_is_mapped() {
    // given
    let root = private_dir();
    let held = StateDirLock::acquire(root.path()).unwrap();
    let locked = StateDirLock::acquire(root.path()).unwrap_err();
    let other = private_dir();
    set_mode(other.path(), 0o755);
    let unusable = StateDirLock::acquire(other.path()).unwrap_err();
    // when
    let codes = [locked.exit_code(), unusable.exit_code()];
    // then
    assert_eq!(
        codes,
        [exit_code::LOCK_REFUSED, exit_code::STATE_DIR_UNUSABLE]
    );
    let all = [
        exit_code::CONFIG_INVALID,
        exit_code::LOCK_REFUSED,
        exit_code::STATE_DIR_UNUSABLE,
    ];
    assert!(
        all.iter()
            .all(|code| *code != 0 && *code != 1 && *code != 2)
    );
    assert!(all[0] != all[1] && all[1] != all[2] && all[0] != all[2]);
    drop(held);
}
