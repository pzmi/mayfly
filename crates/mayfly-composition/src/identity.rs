//! The host's identity: the [`HostId`] both planes name this host by, kept in the state
//! directory so it outlives every run (ADR 0091).
//!
//! The control plane's streams (`host-<id>`, `hostrun-<id>`), every VM name and every
//! ownership proof carry it, so it must never change under a journal: it is generated once, on
//! the first start, from the operating system's generator, and read back on every later start.
//! A file that exists but does not hold a valid id is refused, never replaced: a new id would
//! orphan every VM and lease the journal records (fail closed). Rejected alternative: the
//! host name, which an operator may change and which may not fit `[a-z0-9]{1,16}`.
//!
//! The same id is the image store's `deployment_id` (ADR 0087): one deployment per state
//! directory, which is one host.

use std::fs::{File, OpenOptions};
use std::io::{self, ErrorKind, Read as _, Write as _};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};

use mayfly_domain::HostId;

/// The file under the state directory.
pub const HOST_ID_FILE: &str = "host-id";

/// Why the host's identity could not be had.
#[derive(Debug)]
pub enum IdentityError {
    /// The file exists and does not hold a host id: it is never replaced.
    Invalid(PathBuf),
    /// The file is missing while a journal exists: a new id would orphan what the journal
    /// records, so none is generated.
    MissingWithJournal(PathBuf),
    /// The file could not be read or written, or the generator could not be read.
    Io(PathBuf, io::Error),
}

impl std::fmt::Display for IdentityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(path) => write!(
                f,
                "{} does not hold a host id ([a-z0-9]{{1,16}}); it is never replaced, since the \
                 journal names this host by it: restore it",
                path.display()
            ),
            Self::MissingWithJournal(path) => write!(
                f,
                "{} is missing but a journal exists beside it; a new host id would orphan every \
                 VM and lease the journal records: restore the file (the host id is in the \
                 journal's `host-<id>` streams)",
                path.display()
            ),
            Self::Io(path, error) => write!(f, "cannot use {}: {error}", path.display()),
        }
    }
}

impl std::error::Error for IdentityError {}

/// `n` bytes from the operating system's generator.
///
/// # Errors
/// The generator could not be read.
pub fn random_bytes<const N: usize>() -> io::Result<[u8; N]> {
    let mut bytes = [0_u8; N];
    File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes)
}

fn generate() -> io::Result<HostId> {
    let bytes: [u8; 8] = random_bytes()?;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    let id = format!("h{}", &hex[..15]);
    HostId::new(&id).map_err(|_| io::Error::from(ErrorKind::InvalidData))
}

/// The host id kept in `state_dir`, generated and written `0600` (synced, then the directory
/// synced) on the first call, and only while `journal` does not exist yet: with a journal, a
/// missing file is refused (fail closed). The caller holds the state-dir lock, so no other
/// daemon writes it. Pinned by `tests::should_keep_the_same_id_when_read_again`,
/// `tests::should_refuse_the_id_when_the_file_is_not_one` and
/// `tests::should_refuse_a_new_id_when_a_journal_exists` and
/// `tests::should_refuse_a_new_id_when_the_file_is_a_dangling_link`.
///
/// # Errors
/// [`IdentityError`].
pub fn host_id(state_dir: &Path, journal: &Path) -> Result<HostId, IdentityError> {
    let path = state_dir.join(HOST_ID_FILE);
    let io = |e| IdentityError::Io(path.clone(), e);
    // Existence is read without following a link, so a dangling `host-id` link is an entry
    // that does not read (refused), never a missing file (regenerated).
    match std::fs::symlink_metadata(&path) {
        Ok(_) => {
            let text = std::fs::read_to_string(&path).map_err(io)?;
            return HostId::new(text.trim_end_matches('\n'))
                .map_err(|_| IdentityError::Invalid(path.clone()));
        }
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => return Err(io(error)),
    }
    match std::fs::symlink_metadata(journal) {
        Ok(_) => return Err(IdentityError::MissingWithJournal(path)),
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => return Err(io(error)),
    }
    let id = generate().map_err(io)?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .map_err(io)?;
    file.write_all(format!("{id}\n").as_bytes()).map_err(io)?;
    file.sync_all().map_err(io)?;
    File::open(state_dir)
        .and_then(|dir| dir.sync_all())
        .map_err(io)?;
    Ok(id)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;

    #[test]
    fn should_keep_the_same_id_when_read_again() {
        // given
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));

        // when
        let first =
            host_id(dir.path(), &dir.path().join("journal.db")).unwrap_or_else(|e| panic!("{e}"));
        let again =
            host_id(dir.path(), &dir.path().join("journal.db")).unwrap_or_else(|e| panic!("{e}"));

        // then
        assert_eq!(first, again);
        let mode = std::fs::metadata(dir.path().join(HOST_ID_FILE))
            .unwrap_or_else(|e| panic!("{e}"))
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn should_refuse_the_id_when_the_file_is_not_one() {
        // given
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        std::fs::write(dir.path().join(HOST_ID_FILE), "Not A Host\n")
            .unwrap_or_else(|e| panic!("{e}"));

        // when
        let read = host_id(dir.path(), &dir.path().join("journal.db"));

        // then
        assert!(matches!(read, Err(IdentityError::Invalid(_))), "{read:?}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join(HOST_ID_FILE)).ok(),
            Some("Not A Host\n".to_owned())
        );
    }

    #[test]
    fn should_refuse_a_new_id_when_a_journal_exists() {
        // given
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        std::fs::write(dir.path().join("journal.db"), b"").unwrap_or_else(|e| panic!("{e}"));

        // when
        let read = host_id(dir.path(), &dir.path().join("journal.db"));

        // then
        assert!(
            matches!(read, Err(IdentityError::MissingWithJournal(_))),
            "{read:?}"
        );
        assert!(!dir.path().join(HOST_ID_FILE).exists());
    }

    #[test]
    fn should_refuse_a_new_id_when_the_file_is_a_dangling_link() {
        // given
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        std::os::unix::fs::symlink(dir.path().join("gone"), dir.path().join(HOST_ID_FILE))
            .unwrap_or_else(|e| panic!("{e}"));

        // when
        let read = host_id(dir.path(), &dir.path().join("journal.db"));

        // then
        assert!(matches!(read, Err(IdentityError::Io(..))), "{read:?}");
        assert!(!dir.path().join("gone").exists());
    }
}
