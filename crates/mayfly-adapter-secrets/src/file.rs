//! `file:<absolute path>`: a file owned by the daemon's user, mode `0600` or narrower, in a
//! directory owned by that user, mode `0700` or narrower, below ancestors only root or the
//! daemon's user can change (ADR 0033, ADR 0058; the rules in `path.rs`).
//!
//! The directory is reached by the `O_NOFOLLOW` walk from `/` and the file opened relative to
//! it with `openat(O_NOFOLLOW | O_NONBLOCK)`; owner, mode and type are checked on the opened
//! descriptor (`fstat`) and the same descriptor is read, so no rename or symlink swapped in
//! between a check and the read is followed. `O_NONBLOCK` keeps a FIFO planted at the path from
//! blocking the open; it is then refused as not a regular file. All of it runs on Tokio's
//! blocking pool. Pinned by the contract's FS cases and `path.rs`'s unit tests.

use std::fs::File;
use std::io::Read;
use std::path::Path;

use mayfly_control::secrets::{
    MAX_SECRET_BYTES, Refusal, SecretError, SecretProvider, secret_from_bytes,
};
use mayfly_domain::{Secret, SecretRef};
use rustix::fs::{Mode, OFlags, openat};

use crate::command::{blocking, daemon_uid};
use crate::path::{
    Ancestors, Stat, errno_refusal, io_refusal, open_directory_of, secret_directory_refusal,
    secret_file_refusal,
};

/// Reads owner-only files.
#[derive(Debug, Clone, Copy)]
pub struct FileProvider {
    ancestors: Ancestors,
}

impl FileProvider {
    /// Files must belong to the user the daemon runs as.
    #[must_use]
    pub fn for_daemon_user() -> Self {
        Self::owned_by(daemon_uid())
    }

    /// Files must belong to `uid`.
    #[must_use]
    pub const fn owned_by(uid: u32) -> Self {
        Self {
            ancestors: Ancestors::of(uid),
        }
    }

    /// The same provider, also accepting directories above the secret's directory (never the
    /// directory or the file) that `uid` owns. The daemon never builds one: its provider
    /// ([`Self::for_daemon_user`]) trusts root and its own user only. It exists so that the
    /// contract's FS-03 can expect files of another user and still be decided by the secret's
    /// own owner, not by who owns the tempdir above it, as root and as any other user (pinned
    /// by `tests/contract.rs::should_pass_file_contract_when_provider_is_file_provider`).
    #[must_use]
    pub const fn trusting_ancestors_of(self, uid: u32) -> Self {
        Self {
            ancestors: Ancestors {
                owner: self.ancestors.owner,
                also: uid,
            },
        }
    }
}

/// Opens, checks and reads the file `path` names.
fn read_checked(path: &Path, ancestors: Ancestors) -> Result<Vec<u8>, Refusal> {
    let owner = ancestors.owner;
    let (directory, name) = open_directory_of(path, ancestors, secret_directory_refusal)?;
    let flags =
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::NOCTTY | OFlags::CLOEXEC;
    let file = File::from(openat(&directory, name, flags, Mode::empty()).map_err(errno_refusal)?);
    let stat = file.metadata().map_err(|e| io_refusal(&e))?;
    if let Some(refusal) = secret_file_refusal(owner, &Stat::of(&stat)) {
        return Err(refusal);
    }
    let mut bytes = Vec::new();
    file.take(MAX_SECRET_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| io_refusal(&e))?;
    Ok(bytes)
}

impl SecretProvider for FileProvider {
    async fn resolve(&self, reference: &SecretRef) -> Result<Secret<String>, SecretError> {
        let SecretRef::File { path } = reference else {
            return Err(SecretError::new(reference, Refusal::Unsupported));
        };
        let (path, ancestors) = (path.clone(), self.ancestors);
        let bytes = blocking(reference, move || read_checked(&path, ancestors)).await?;
        secret_from_bytes(reference, bytes)
    }
}
