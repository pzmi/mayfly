//! The `SecretProvider` port (ADR 0033): a reference URI in, a [`Secret`] in memory out.
//!
//! Config holds references only; a provider is asked again each time a credential is refreshed,
//! so a rotated value is picked up without a restart and nothing is cached by mayfly (ADR 0014).
//! The adapters (`file:`, `env:`, `exec:`, `keychain:`) live in `mayfly-adapter-secrets`; the
//! in-memory one in `mayfly-testkit`; every one passes the `SecretProvider` contract suite
//! (`mayfly_testkit::contracts::secrets`, ver §5).
//!
//! A refusal ([`SecretError`]) names the reference and why, never the value or any byte read
//! from the source: an operator pastes errors into bug reports. Pinned by the contract's
//! `SP-07` and `tests/secrets.rs::should_not_echo_the_value_when_a_source_is_refused`.

use std::fmt;
use std::future::Future;
use std::io::ErrorKind;

use mayfly_domain::{Secret, SecretRef};

/// The largest secret any provider returns, in bytes before the trailing line break is
/// dropped: a GitHub App key is about 1.7 KiB. A larger source is refused, never truncated.
pub const MAX_SECRET_BYTES: usize = 64 * 1024;

/// Resolves secret references (ADR 0033).
///
/// `resolve` is cancel-safe: it records nothing, and dropping it mid-way leaves no state
/// behind (an `exec:` command in flight is killed with its process group).
pub trait SecretProvider: Send + Sync {
    /// The value `reference` points to now, read afresh on every call.
    ///
    /// # Errors
    /// [`SecretError`] when the source is absent, unsafe (wrong owner or mode, a symlink, a
    /// relative path), fails (non-zero exit, deadline), or holds nothing usable.
    fn resolve(
        &self,
        reference: &SecretRef,
    ) -> impl Future<Output = Result<Secret<String>, SecretError>> + Send;
}

/// A reference that did not resolve. Carries no byte of the source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretError {
    /// The reference asked for (config text, not a secret).
    pub reference: SecretRef,
    /// Why it was refused.
    pub refusal: Refusal,
}

impl SecretError {
    /// A refusal of `reference`.
    #[must_use]
    pub fn new(reference: &SecretRef, refusal: Refusal) -> Self {
        Self {
            reference: reference.clone(),
            refusal,
        }
    }
}

/// Why a reference did not resolve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// This provider, or this operating system, does not serve the reference's scheme
    /// (`keychain:` off macOS).
    Unsupported,
    /// A `file:` or `exec:` path that is not absolute.
    RelativePath,
    /// The file, variable, command or Keychain item does not exist.
    NotFound,
    /// The file, or its directory, is a symbolic link.
    Symlink,
    /// The path names something other than a regular file (a directory, a FIFO, a device).
    NotRegularFile,
    /// The file is owned by another user than the daemon's (an `exec:` command: than root or the
    /// daemon's).
    WrongOwner {
        /// The daemon's user id.
        expected: u32,
        /// The file's owner.
        found: u32,
    },
    /// The file's mode grants anything to group or others, or sets a special bit (an `exec:`
    /// command: lets group or others write it).
    TooOpen {
        /// The file's permission bits.
        mode: u32,
    },
    /// The file's directory is owned by another user than the daemon's.
    DirectoryWrongOwner {
        /// The daemon's user id.
        expected: u32,
        /// The directory's owner.
        found: u32,
    },
    /// The file's directory grants anything to group or others (an `exec:` command's: lets
    /// group or others write it).
    DirectoryTooOpen {
        /// The directory's permission bits.
        mode: u32,
    },
    /// A directory above the file's (or command's) directory, or a symlink on the way, is owned
    /// by another user than root or the daemon's.
    AncestorWrongOwner {
        /// The daemon's user id.
        expected: u32,
        /// Its owner.
        found: u32,
    },
    /// A directory above the file's (or command's) directory is writable by group or others
    /// without the sticky bit.
    AncestorTooOpen {
        /// Its permission bits.
        mode: u32,
    },
    /// The source holds more than [`MAX_SECRET_BYTES`].
    TooLarge {
        /// The limit it exceeded.
        limit: usize,
    },
    /// The source holds nothing (after one trailing line break is dropped).
    Empty,
    /// The source is not UTF-8 text.
    NotText,
    /// An `exec:` command (or the Keychain tool) exited unsuccessfully; its output is
    /// discarded, so nothing it printed is echoed.
    Exited {
        /// The exit code, or `None` when a signal ended it.
        code: Option<i32>,
    },
    /// The command did not finish within its deadline; its process group was killed.
    TimedOut {
        /// The deadline, in milliseconds.
        after_ms: u64,
    },
    /// The Keychain is locked, or refused this process access to the item.
    Locked,
    /// The operating system refused an operation (open, read, spawn) for another reason.
    Io {
        /// The kind of the error, never its message.
        kind: ErrorKind,
    },
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported => f.write_str("this scheme is not supported here"),
            Self::RelativePath => f.write_str("the path is not absolute"),
            Self::NotFound => f.write_str("not found"),
            Self::Symlink => f.write_str("the file or its directory is a symbolic link"),
            Self::NotRegularFile => f.write_str("not a regular file"),
            Self::WrongOwner { expected, found } => {
                write!(
                    f,
                    "the file is owned by uid {found}, not the daemon's uid {expected}"
                )
            }
            Self::TooOpen { mode } => {
                write!(
                    f,
                    "the file's mode is {mode:04o}; it must be 0600 or narrower"
                )
            }
            Self::DirectoryWrongOwner { expected, found } => write!(
                f,
                "the directory is owned by uid {found}, not the daemon's uid {expected}"
            ),
            Self::DirectoryTooOpen { mode } => {
                write!(
                    f,
                    "the directory's mode is {mode:04o}; it must be 0700 or narrower"
                )
            }
            Self::AncestorWrongOwner { expected, found } => write!(
                f,
                "a directory on the path is owned by uid {found}, not root or the daemon's uid {expected}"
            ),
            Self::AncestorTooOpen { mode } => write!(
                f,
                "a directory on the path has mode {mode:04o}; group or others may write it"
            ),
            Self::TooLarge { limit } => write!(f, "larger than {limit} bytes"),
            Self::Empty => f.write_str("empty"),
            Self::NotText => f.write_str("not UTF-8 text"),
            Self::Exited { code: Some(code) } => write!(f, "the command exited with {code}"),
            Self::Exited { code: None } => f.write_str("the command was killed by a signal"),
            Self::TimedOut { after_ms } => {
                write!(f, "the command did not finish within {after_ms} ms")
            }
            Self::Locked => f.write_str("the keychain is locked or refused access"),
            Self::Io { kind } => write!(f, "the system refused it ({kind})"),
        }
    }
}

impl fmt::Display for SecretError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "secret {} refused: {}", self.reference, self.refusal)
    }
}

impl std::error::Error for SecretError {}

/// The secret a source's bytes hold, by the one rule every provider follows: refused above
/// [`MAX_SECRET_BYTES`], one trailing `\n` or `\r\n` dropped (a file written by `echo`, a
/// command that ends its line), refused when nothing remains or it is not UTF-8.
///
/// # Errors
/// `TooLarge`, `Empty` or `NotText`, naming `reference` only.
pub fn secret_from_bytes(
    reference: &SecretRef,
    mut bytes: Vec<u8>,
) -> Result<Secret<String>, SecretError> {
    if bytes.len() > MAX_SECRET_BYTES {
        return Err(SecretError::new(
            reference,
            Refusal::TooLarge {
                limit: MAX_SECRET_BYTES,
            },
        ));
    }
    if bytes.last() == Some(&b'\n') {
        bytes.pop();
        if bytes.last() == Some(&b'\r') {
            bytes.pop();
        }
    }
    if bytes.is_empty() {
        return Err(SecretError::new(reference, Refusal::Empty));
    }
    String::from_utf8(bytes)
        .map(Secret::new)
        .map_err(|_| SecretError::new(reference, Refusal::NotText))
}

/// What `doctor` reports for one configured reference: whether it resolves, never its value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReferenceCheck {
    /// The config key the reference sits under (`github.app_key`).
    pub name: String,
    /// The reference.
    pub reference: SecretRef,
    /// `Ok` when it resolved; the value was dropped at once.
    pub outcome: Result<(), SecretError>,
}

/// Resolves each reference once and reports which resolve, without keeping or printing any
/// value (ADR 0033: "`doctor` resolves each reference without printing it"). Pinned by
/// `tests/secrets.rs::should_report_each_reference_without_its_value_when_doctor_checks_them`.
pub async fn check_references<P: SecretProvider>(
    provider: &P,
    references: &[(String, SecretRef)],
) -> Vec<ReferenceCheck> {
    let mut out = Vec::with_capacity(references.len());
    for (name, reference) in references {
        let outcome = provider.resolve(reference).await.map(drop);
        out.push(ReferenceCheck {
            name: name.clone(),
            reference: reference.clone(),
            outcome,
        });
    }
    out
}
