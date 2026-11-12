//! Who may own, and write, what on the way to a secret file or a secret command (ADR 0033,
//! ADR 0058), as pure rules over a [`Stat`], and the walk that applies them from `/`.
//!
//! - **Ancestors** (every directory above the file's or command's directory): owned by root or
//!   the daemon's user; not writable by group or others unless sticky (`/tmp`). A symlink on
//!   the way is followed only when root or the daemon's user owns it (`/var` and `/tmp` are
//!   root's symlinks on macOS), its target holds no `.` or `..`, and at most [`MAX_LINKS`] are
//!   followed; its target is then walked component by component by the same rules (from `/`
//!   when absolute), so every directory it leads through is checked as an ancestor, not only
//!   the last.
//! - **A secret file's directory**: the daemon's, nothing for group or others (`0700`).
//! - **A secret file**: a regular file, the daemon's, nothing for group or others and no special
//!   bit (`0600`), at most the port's limit.
//! - **A command's directory and the command**: root's or the daemon's, writable by neither
//!   group nor others; the command a regular file.
//!
//! The walk opens each directory relative to the one before with `O_DIRECTORY | O_NOFOLLOW` and
//! checks it on the opened descriptor, so a directory swapped after its check is never entered.
//! The rules are unit-tested branch by branch below; the walk by the contract's FS and EX cases.

use std::collections::VecDeque;
use std::ffi::{OsStr, OsString};
use std::fs::{File, Metadata};
use std::io::ErrorKind;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path};

use mayfly_control::secrets::{MAX_SECRET_BYTES, Refusal};
use rustix::fs::{AtFlags, FileType, Mode, OFlags, open, openat, readlinkat, statat};
use rustix::io::Errno;

/// What a node is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A regular file.
    File,
    /// A directory.
    Directory,
    /// A symbolic link.
    Symlink,
    /// A FIFO, socket or device.
    Other,
}

/// The facts the rules read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stat {
    /// Owner.
    pub uid: u32,
    /// Mode bits, type bits included.
    pub mode: u32,
    /// What it is.
    pub kind: Kind,
    /// Size in bytes.
    pub len: u64,
}

impl Stat {
    /// The facts of `metadata`.
    pub fn of(metadata: &Metadata) -> Self {
        let file_type = metadata.file_type();
        let kind = if file_type.is_file() {
            Kind::File
        } else if file_type.is_dir() {
            Kind::Directory
        } else if file_type.is_symlink() {
            Kind::Symlink
        } else {
            Kind::Other
        };
        Self {
            uid: metadata.uid(),
            mode: metadata.mode(),
            kind,
            len: metadata.len(),
        }
    }

    const fn permissions(&self) -> u32 {
        self.mode & 0o7777
    }
}

/// The most symlinks followed on the way to a secret's directory; one more is refused as a loop.
pub const MAX_LINKS: usize = 40;

const ROOT: u32 = 0;
const GROUP_OTHER: u32 = 0o077;
const GROUP_OTHER_WRITE: u32 = 0o022;
const SPECIAL: u32 = 0o7000;
const STICKY: u32 = 0o1000;

const fn trusted(owner: u32, uid: u32) -> bool {
    uid == ROOT || uid == owner
}

/// Who may own the directories above the last one (and the symlinks on the way to it): root,
/// the daemon's user and, only where a caller says so, one more user.
///
/// The daemon's providers trust exactly root and the daemon's user (`for_daemon`, where
/// `also` is the daemon's uid). The one more user exists for the contract's FS-03: a provider
/// that expects files of another uid than the tests' must still accept the tempdir the tests'
/// user owns above the secret, or the case would be decided by an ancestor and so by who runs
/// the tests (root trusts everything, any other user nothing). Rejected: starting the walk at
/// a configured root below `/`, which would leave the top of the path unchecked in any provider
/// built that way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ancestors {
    /// The daemon's user.
    pub owner: u32,
    /// One more user whose ancestors are trusted; `owner` when there is none.
    pub also: u32,
}

impl Ancestors {
    /// Root and `owner` only.
    pub const fn of(owner: u32) -> Self {
        Self { owner, also: owner }
    }

    const fn trust(self, uid: u32) -> bool {
        trusted(self.owner, uid) || uid == self.also
    }
}

/// A directory above the last one, or a symlink on the way.
pub const fn ancestor_refusal(ancestors: Ancestors, stat: &Stat) -> Option<Refusal> {
    let owner = ancestors.owner;
    if !ancestors.trust(stat.uid) {
        return Some(Refusal::AncestorWrongOwner {
            expected: owner,
            found: stat.uid,
        });
    }
    if matches!(stat.kind, Kind::Directory)
        && stat.mode & GROUP_OTHER_WRITE != 0
        && stat.mode & STICKY == 0
    {
        return Some(Refusal::AncestorTooOpen {
            mode: stat.permissions(),
        });
    }
    None
}

/// A secret file's directory.
pub const fn secret_directory_refusal(owner: u32, stat: &Stat) -> Option<Refusal> {
    if stat.uid != owner {
        return Some(Refusal::DirectoryWrongOwner {
            expected: owner,
            found: stat.uid,
        });
    }
    if stat.mode & GROUP_OTHER != 0 {
        return Some(Refusal::DirectoryTooOpen {
            mode: stat.permissions(),
        });
    }
    None
}

/// A secret file.
pub const fn secret_file_refusal(owner: u32, stat: &Stat) -> Option<Refusal> {
    if !matches!(stat.kind, Kind::File) {
        return Some(Refusal::NotRegularFile);
    }
    if stat.uid != owner {
        return Some(Refusal::WrongOwner {
            expected: owner,
            found: stat.uid,
        });
    }
    if stat.mode & (GROUP_OTHER | SPECIAL) != 0 {
        return Some(Refusal::TooOpen {
            mode: stat.permissions(),
        });
    }
    if stat.len > MAX_SECRET_BYTES as u64 {
        return Some(Refusal::TooLarge {
            limit: MAX_SECRET_BYTES,
        });
    }
    None
}

/// A secret command's directory.
pub const fn command_directory_refusal(owner: u32, stat: &Stat) -> Option<Refusal> {
    if !trusted(owner, stat.uid) {
        return Some(Refusal::DirectoryWrongOwner {
            expected: owner,
            found: stat.uid,
        });
    }
    if stat.mode & GROUP_OTHER_WRITE != 0 {
        return Some(Refusal::DirectoryTooOpen {
            mode: stat.permissions(),
        });
    }
    None
}

/// A secret command.
pub const fn command_refusal(owner: u32, stat: &Stat) -> Option<Refusal> {
    match stat.kind {
        Kind::Symlink => return Some(Refusal::Symlink),
        Kind::File => {}
        Kind::Directory | Kind::Other => return Some(Refusal::NotRegularFile),
    }
    if !trusted(owner, stat.uid) {
        return Some(Refusal::WrongOwner {
            expected: owner,
            found: stat.uid,
        });
    }
    if stat.mode & GROUP_OTHER_WRITE != 0 {
        return Some(Refusal::TooOpen {
            mode: stat.permissions(),
        });
    }
    None
}

/// A refused open: a symlink where `O_NOFOLLOW` met one, absent, or the system's kind.
pub fn errno_refusal(errno: Errno) -> Refusal {
    match errno {
        Errno::LOOP | Errno::MLINK => Refusal::Symlink,
        Errno::NOENT => Refusal::NotFound,
        other => io_refusal(&other.into()),
    }
}

/// An I/O failure by kind, never by message.
pub fn io_refusal(err: &std::io::Error) -> Refusal {
    Refusal::Io { kind: err.kind() }
}

fn stat_of(file: &File) -> Result<Stat, Refusal> {
    file.metadata()
        .map(|m| Stat::of(&m))
        .map_err(|e| io_refusal(&e))
}

/// Opens the directory `path` names its last component in, walking from `/`: every ancestor
/// passes [`ancestor_refusal`] for `ancestors`, a symlink on the way is followed only as the
/// module says (its target's components walked like the path's own), the directory itself is
/// no symlink and passes `directory_rule`. Returns that directory and the last component.
/// Pinned by the contract's FS cases and `tests/contract.rs::
/// should_check_every_directory_a_trusted_link_leads_through_when_walking_to_a_secret`.
///
/// # Errors
/// The first refusal on the way; `RelativePath` for a path that is not absolute or holds `..`.
pub fn open_directory_of(
    path: &Path,
    ancestors: Ancestors,
    directory_rule: fn(u32, &Stat) -> Option<Refusal>,
) -> Result<(File, &OsStr), Refusal> {
    if !path.is_absolute() {
        return Err(Refusal::RelativePath);
    }
    let (Some(name), Some(parent)) = (path.file_name(), path.parent()) else {
        return Err(Refusal::NotRegularFile);
    };
    let mut pending: VecDeque<OsString> = VecDeque::new();
    for component in parent.components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(part) => pending.push_back(part.to_os_string()),
            Component::ParentDir | Component::Prefix(_) => return Err(Refusal::RelativePath),
        }
    }
    let owner = ancestors.owner;
    let check = |is_last: bool, stat: &Stat| {
        if is_last {
            directory_rule(owner, stat)
        } else {
            ancestor_refusal(ancestors, stat)
        }
    };
    let open_root = |is_last: bool| -> Result<File, Refusal> {
        let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
        let root = File::from(open("/", flags, Mode::empty()).map_err(errno_refusal)?);
        match check(is_last, &stat_of(&root)?) {
            Some(refusal) => Err(refusal),
            None => Ok(root),
        }
    };
    let mut directory = open_root(pending.is_empty())?;
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
    let mut links = 0;
    while let Some(part) = pending.pop_front() {
        let is_last = pending.is_empty();
        let next = match openat(&directory, &part, flags | OFlags::NOFOLLOW, Mode::empty()) {
            Ok(fd) => File::from(fd),
            Err(Errno::LOOP | Errno::NOTDIR | Errno::MLINK) => {
                let link =
                    statat(&directory, &part, AtFlags::SYMLINK_NOFOLLOW).map_err(errno_refusal)?;
                if FileType::from_raw_mode(link.st_mode) != FileType::Symlink {
                    return Err(Refusal::Io {
                        kind: ErrorKind::NotADirectory,
                    });
                }
                links += 1;
                if is_last || links > MAX_LINKS {
                    return Err(Refusal::Symlink);
                }
                // `directory`, the link's own, passed the ancestor rule when it was opened
                if !ancestors.trust(link.st_uid) {
                    return Err(Refusal::AncestorWrongOwner {
                        expected: owner,
                        found: link.st_uid,
                    });
                }
                let target = readlinkat(&directory, &part, Vec::new()).map_err(errno_refusal)?;
                let target = Path::new(OsStr::from_bytes(target.as_bytes()));
                for component in target.components().rev() {
                    match component {
                        Component::Normal(step) => pending.push_front(step.to_os_string()),
                        Component::RootDir => {}
                        Component::CurDir | Component::ParentDir | Component::Prefix(_) => {
                            return Err(Refusal::Symlink);
                        }
                    }
                }
                if target.is_absolute() {
                    directory = open_root(false)?;
                }
                continue;
            }
            Err(errno) => return Err(errno_refusal(errno)),
        };
        let stat = stat_of(&next)?;
        if let Some(refusal) = check(is_last, &stat) {
            return Err(refusal);
        }
        directory = next;
    }
    Ok((directory, name))
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAEMON: u32 = 501;
    const OTHER: u32 = 502;

    fn stat(uid: u32, mode: u32, kind: Kind) -> Stat {
        Stat {
            uid,
            mode,
            kind,
            len: 10,
        }
    }

    #[test]
    fn should_accept_ancestors_of_root_or_the_daemon_when_no_one_else_may_write() {
        // given
        let cases = [
            stat(ROOT, 0o755, Kind::Directory),
            stat(DAEMON, 0o700, Kind::Directory),
            stat(ROOT, 0o1777, Kind::Directory),
            stat(ROOT, 0o777, Kind::Symlink),
        ];

        // when
        let got: Vec<Option<Refusal>> = cases
            .iter()
            .map(|s| ancestor_refusal(Ancestors::of(DAEMON), s))
            .collect();

        // then
        assert_eq!(got, [None, None, None, None]);
    }

    #[test]
    fn should_refuse_an_ancestor_when_another_user_owns_it_or_others_may_write_it() {
        // given
        let cases = [
            stat(OTHER, 0o755, Kind::Directory),
            stat(OTHER, 0o777, Kind::Symlink),
            stat(ROOT, 0o775, Kind::Directory),
            stat(DAEMON, 0o757, Kind::Directory),
        ];

        // when
        let got: Vec<Option<Refusal>> = cases
            .iter()
            .map(|s| ancestor_refusal(Ancestors::of(DAEMON), s))
            .collect();

        // then
        assert_eq!(
            got,
            [
                Some(Refusal::AncestorWrongOwner {
                    expected: DAEMON,
                    found: OTHER
                }),
                Some(Refusal::AncestorWrongOwner {
                    expected: DAEMON,
                    found: OTHER
                }),
                Some(Refusal::AncestorTooOpen { mode: 0o775 }),
                Some(Refusal::AncestorTooOpen { mode: 0o757 }),
            ]
        );
    }

    #[test]
    fn should_trust_one_more_ancestor_owner_when_named_and_never_otherwise() {
        // given
        const ANOTHER: u32 = 503;
        let theirs = stat(OTHER, 0o755, Kind::Directory);
        let open = stat(OTHER, 0o775, Kind::Directory);
        let elsewhere = stat(ANOTHER, 0o755, Kind::Directory);
        let naming = Ancestors {
            owner: DAEMON,
            also: OTHER,
        };

        // when
        let got = [
            ancestor_refusal(naming, &theirs),
            ancestor_refusal(naming, &open),
            ancestor_refusal(naming, &elsewhere),
            ancestor_refusal(Ancestors::of(DAEMON), &theirs),
        ];

        // then
        assert_eq!(
            got,
            [
                None,
                Some(Refusal::AncestorTooOpen { mode: 0o775 }),
                Some(Refusal::AncestorWrongOwner {
                    expected: DAEMON,
                    found: ANOTHER
                }),
                Some(Refusal::AncestorWrongOwner {
                    expected: DAEMON,
                    found: OTHER
                }),
            ]
        );
    }

    #[test]
    fn should_refuse_a_secret_directory_when_anyone_but_the_daemon_may_use_it() {
        // given
        let cases = [
            stat(DAEMON, 0o700, Kind::Directory),
            stat(DAEMON, 0o500, Kind::Directory),
            stat(ROOT, 0o700, Kind::Directory),
            stat(DAEMON, 0o710, Kind::Directory),
            stat(DAEMON, 0o701, Kind::Directory),
        ];

        // when
        let got: Vec<Option<Refusal>> = cases
            .iter()
            .map(|s| secret_directory_refusal(DAEMON, s))
            .collect();

        // then
        assert_eq!(
            got,
            [
                None,
                None,
                Some(Refusal::DirectoryWrongOwner {
                    expected: DAEMON,
                    found: ROOT
                }),
                Some(Refusal::DirectoryTooOpen { mode: 0o710 }),
                Some(Refusal::DirectoryTooOpen { mode: 0o701 }),
            ]
        );
    }

    #[test]
    fn should_refuse_a_secret_file_when_its_kind_owner_mode_or_size_is_wrong() {
        // given
        let large = Stat {
            len: MAX_SECRET_BYTES as u64 + 1,
            ..stat(DAEMON, 0o600, Kind::File)
        };
        let cases = [
            stat(DAEMON, 0o600, Kind::File),
            stat(DAEMON, 0o400, Kind::File),
            stat(DAEMON, 0o600, Kind::Other),
            stat(OTHER, 0o644, Kind::File),
            stat(DAEMON, 0o640, Kind::File),
            stat(DAEMON, 0o4600, Kind::File),
            large,
        ];

        // when
        let got: Vec<Option<Refusal>> = cases
            .iter()
            .map(|s| secret_file_refusal(DAEMON, s))
            .collect();

        // then
        assert_eq!(
            got,
            [
                None,
                None,
                Some(Refusal::NotRegularFile),
                Some(Refusal::WrongOwner {
                    expected: DAEMON,
                    found: OTHER
                }),
                Some(Refusal::TooOpen { mode: 0o640 }),
                Some(Refusal::TooOpen { mode: 0o4600 }),
                Some(Refusal::TooLarge {
                    limit: MAX_SECRET_BYTES
                }),
            ]
        );
    }

    #[test]
    fn should_refuse_a_command_or_its_directory_when_another_user_may_change_it() {
        // given
        let directories = [
            stat(ROOT, 0o755, Kind::Directory),
            stat(OTHER, 0o755, Kind::Directory),
            stat(DAEMON, 0o775, Kind::Directory),
        ];
        let commands = [
            stat(ROOT, 0o755, Kind::File),
            stat(DAEMON, 0o700, Kind::File),
            stat(DAEMON, 0o777, Kind::Symlink),
            stat(DAEMON, 0o755, Kind::Directory),
            stat(OTHER, 0o755, Kind::File),
            stat(DAEMON, 0o722, Kind::File),
        ];

        // when
        let got_directories: Vec<Option<Refusal>> = directories
            .iter()
            .map(|s| command_directory_refusal(DAEMON, s))
            .collect();
        let got_commands: Vec<Option<Refusal>> = commands
            .iter()
            .map(|s| command_refusal(DAEMON, s))
            .collect();

        // then
        assert_eq!(
            got_directories,
            [
                None,
                Some(Refusal::DirectoryWrongOwner {
                    expected: DAEMON,
                    found: OTHER
                }),
                Some(Refusal::DirectoryTooOpen { mode: 0o775 }),
            ]
        );
        assert_eq!(
            got_commands,
            [
                None,
                None,
                Some(Refusal::Symlink),
                Some(Refusal::NotRegularFile),
                Some(Refusal::WrongOwner {
                    expected: DAEMON,
                    found: OTHER
                }),
                Some(Refusal::TooOpen { mode: 0o722 }),
            ]
        );
    }
}
