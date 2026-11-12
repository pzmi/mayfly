//! The external epoch floor (ADR 0026, plan §3.1): the one durable fact the data plane writes
//! about sessions, kept in mayfly's own `TART_HOME` beside the VMs it fences, so a journal
//! restored from an older backup cannot mint an epoch a VM's last session already saw (I17).
//!
//! **Fail closed.** A floor file that exists but does not read is refused ([`FloorUnreadable`]):
//! a daemon that guessed 0 would accept a welcome below the epoch it last acted under. A missing
//! file is "no session yet" only on a host with nothing of mayfly's on it; [`FloorFile::stored`]
//! tells the two apart, and composition refuses to start on a missing floor beside mayfly's VMs
//! or bases (ADR 0026: a lost floor plus a restored journal is the case the floor exists for). The file is written whole through
//! [`crate::shim::write_atomically`] (temporary file, fsync, rename, directory fsync), so a
//! reader sees the old floor or the new one, never a torn one.
//!
//! **Never lowered.** [`FloorFile::raise`] to an epoch below the stored one is refused: the
//! session is not entered (the domain only raises to a welcome above the floor it read at start,
//! so this guards against a second writer, never a normal path). Pinned by the tests below.

use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};

use mayfly_data::{FloorFailed, FloorStore};
use mayfly_domain::Epoch;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::process::blocking::blocking;
use crate::shim::write_atomically;

/// The file's name under `TART_HOME`.
pub const FLOOR_FILE_NAME: &str = "mayfly-epoch-floor.json";

/// The file's format version.
const VERSION: u32 = 1;

/// The most bytes a floor file may have; more is unreadable.
const LIMIT: u64 = 4096;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Stored {
    v: u32,
    epoch: u64,
}

/// The floor file exists but could not be read as one: the data plane must not start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FloorUnreadable {
    /// The file.
    pub path: PathBuf,
}

impl std::fmt::Display for FloorUnreadable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the epoch floor {} exists but does not read as one; restore it or remove it only \
             after checking no VM of a later session exists",
            self.path.display()
        )
    }
}

impl std::error::Error for FloorUnreadable {}

/// The epoch floor kept at `<TART_HOME>/mayfly-epoch-floor.json`.
#[derive(Debug)]
pub struct FloorFile {
    path: PathBuf,
    write: Mutex<()>,
}

fn stored_at(path: &Path) -> Result<Option<Epoch>, FloorUnreadable> {
    let unreadable = || FloorUnreadable {
        path: path.to_path_buf(),
    };
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(unreadable()),
    };
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > LIMIT {
        return Err(unreadable());
    }
    match serde_json::from_slice::<Stored>(&bytes) {
        Ok(stored) if stored.v == VERSION => Ok(Some(Epoch::new(stored.epoch))),
        Ok(_) | Err(_) => Err(unreadable()),
    }
}

fn read_at(path: &Path) -> Result<Epoch, FloorUnreadable> {
    stored_at(path).map(|stored| stored.unwrap_or(Epoch::new(0)))
}

fn raise_at(path: &Path, epoch: Epoch) -> io::Result<()> {
    let stored = read_at(path).map_err(|_| io::Error::from(ErrorKind::InvalidData))?;
    if stored > epoch {
        return Err(io::Error::from(ErrorKind::InvalidInput));
    }
    if stored == epoch {
        return Ok(());
    }
    write_atomically(
        path,
        &Stored {
            v: VERSION,
            epoch: epoch.get(),
        },
    )
}

impl FloorFile {
    /// The floor of the Tart home `tart_home`.
    #[must_use]
    pub fn in_home(tart_home: &Path) -> Self {
        Self {
            path: tart_home.join(FLOOR_FILE_NAME),
            write: Mutex::new(()),
        }
    }

    /// The file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The stored floor, `None` when the file is missing. Pinned by
    /// `tests::should_read_no_floor_when_the_file_is_missing`.
    ///
    /// # Errors
    /// [`FloorUnreadable`] when the file exists but is not a floor.
    pub fn stored(&self) -> Result<Option<Epoch>, FloorUnreadable> {
        stored_at(&self.path)
    }

    /// The stored floor; `Epoch(0)` when no session was ever entered here. Pinned by
    /// `tests::should_refuse_the_floor_when_its_file_is_corrupt`.
    ///
    /// # Errors
    /// [`FloorUnreadable`] when the file exists but is not a floor.
    pub fn read(&self) -> Result<Epoch, FloorUnreadable> {
        read_at(&self.path)
    }
}

impl FloorStore for FloorFile {
    async fn raise(&self, epoch: Epoch) -> Result<(), FloorFailed> {
        let path = self.path.clone();
        let _one_writer = self.write.lock().await;
        blocking(move || raise_at(&path, epoch))
            .await
            .map_err(|_| FloorFailed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap_or_else(|e| panic!("{e}"))
    }

    #[test]
    fn should_read_no_floor_when_the_file_is_missing() {
        // given
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let floor = FloorFile::in_home(dir.path());

        // when
        let read = floor.read();

        // then
        assert_eq!(read, Ok(Epoch::new(0)));
        assert_eq!(floor.stored(), Ok(None));
    }

    #[test]
    fn should_keep_the_raised_floor_when_the_file_is_read_again() {
        // given
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let floor = FloorFile::in_home(dir.path());

        // when
        let raised = runtime().block_on(floor.raise(Epoch::new(7)));

        // then
        assert_eq!(raised, Ok(()));
        assert_eq!(FloorFile::in_home(dir.path()).read(), Ok(Epoch::new(7)));
    }

    #[test]
    fn should_refuse_to_lower_the_floor_when_a_lower_epoch_is_raised() {
        // given
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let floor = FloorFile::in_home(dir.path());
        let rt = runtime();
        assert_eq!(rt.block_on(floor.raise(Epoch::new(7))), Ok(()));

        // when
        let lowered = rt.block_on(floor.raise(Epoch::new(3)));
        let again = rt.block_on(floor.raise(Epoch::new(7)));

        // then
        assert_eq!(lowered, Err(FloorFailed));
        assert_eq!(again, Ok(()));
        assert_eq!(floor.read(), Ok(Epoch::new(7)));
    }

    #[test]
    fn should_refuse_the_floor_when_its_file_is_corrupt() {
        // given
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let floor = FloorFile::in_home(dir.path());
        std::fs::write(floor.path(), b"{\"v\":1,\"epo").unwrap_or_else(|e| panic!("{e}"));

        // when
        let read = floor.read();
        let raised = runtime().block_on(floor.raise(Epoch::new(2)));

        // then
        assert_eq!(
            read,
            Err(FloorUnreadable {
                path: floor.path().to_path_buf()
            })
        );
        assert_eq!(raised, Err(FloorFailed));
    }
}
