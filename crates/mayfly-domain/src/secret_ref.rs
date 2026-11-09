//! A reference to a secret, never the secret (ADR 0033, ADR 0014).
//!
//! Config holds references, chosen per secret; a `SecretProvider` resolves one to a value in
//! memory. This type is the parsed reference only: it lives in the kernel so the config
//! reader, the provider port and its adapters share one parser. Pinned by
//! `tests/secret_ref.rs`.
//!
//! Refusals never carry the text they refused: an operator who pastes a key where a reference
//! belongs must not see it echoed into a terminal, a log or a bug report. Pinned by
//! `should_not_echo_the_input_when_a_reference_is_refused`.

use std::fmt;
use std::path::{Component, Path, PathBuf};
use std::str::FromStr;

/// Where a secret comes from (ADR 0033).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecretRef {
    /// `keychain:<service>/<account>`: a macOS Keychain item.
    Keychain {
        /// The Keychain service name.
        service: String,
        /// The Keychain account name.
        account: String,
    },
    /// `file:<absolute path>`: an owner-only file in an owner-only directory.
    File {
        /// The file's absolute path.
        path: PathBuf,
    },
    /// `env:<NAME>`: a variable of the daemon's environment, read once at start.
    Env {
        /// The variable name.
        name: String,
    },
    /// `exec:<absolute path>`: the stdout of an operator command; argv only, no arguments here.
    Exec {
        /// The executable's absolute path.
        path: PathBuf,
    },
}

/// Why a string is not a secret reference. Carries none of the refused text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretRefError {
    /// Not `keychain:`, `file:`, `env:` or `exec:`: a bare value, or a scheme that is not known.
    UnknownScheme,
    /// `keychain:` without exactly `<service>/<account>`, both non-empty and printable.
    KeychainShape,
    /// `file:` without an absolute path free of NUL and `..`.
    FilePath,
    /// `env:` without a variable name matching `[A-Za-z_][A-Za-z0-9_]{0,254}`.
    EnvName,
    /// `exec:` without an absolute, whitespace-free path free of NUL and `..`.
    ExecPath,
}

impl fmt::Display for SecretRefError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::UnknownScheme => {
                "not a secret reference: use keychain:<service>/<account>, file:<absolute path>, \
                 env:<NAME> or exec:<absolute path>; the config never holds the secret itself"
            }
            Self::KeychainShape => {
                "a keychain: reference is keychain:<service>/<account>, both non-empty"
            }
            Self::FilePath => "a file: reference needs an absolute path with no `..` component",
            Self::EnvName => "an env: reference names a variable matching [A-Za-z_][A-Za-z0-9_]*",
            Self::ExecPath => {
                "an exec: reference is the absolute path of an executable, with no arguments \
                 and no `..` component; wrap arguments in a script"
            }
        })
    }
}

impl std::error::Error for SecretRefError {}

impl SecretRef {
    /// The scheme text, `keychain`, `file`, `env` or `exec`.
    #[must_use]
    pub const fn scheme(&self) -> &'static str {
        match self {
            Self::Keychain { .. } => "keychain",
            Self::File { .. } => "file",
            Self::Env { .. } => "env",
            Self::Exec { .. } => "exec",
        }
    }
}

fn clean_path(text: &str) -> Option<PathBuf> {
    let path = Path::new(text);
    let plain = path.is_absolute()
        && !text.contains('\0')
        && path.components().all(|part| part != Component::ParentDir);
    plain.then(|| path.to_path_buf())
}

fn keychain_part(text: &str) -> bool {
    !text.is_empty() && text.len() <= 255 && !text.chars().any(char::is_control)
}

impl FromStr for SecretRef {
    type Err = SecretRefError;

    fn from_str(text: &str) -> Result<Self, SecretRefError> {
        let Some((scheme, rest)) = text.split_once(':') else {
            return Err(SecretRefError::UnknownScheme);
        };
        match scheme {
            "keychain" => match rest.split_once('/') {
                Some((service, account))
                    if keychain_part(service)
                        && keychain_part(account)
                        && !account.contains('/') =>
                {
                    Ok(Self::Keychain {
                        service: service.to_owned(),
                        account: account.to_owned(),
                    })
                }
                _ => Err(SecretRefError::KeychainShape),
            },
            "file" => clean_path(rest)
                .map(|path| Self::File { path })
                .ok_or(SecretRefError::FilePath),
            "env" => {
                let mut chars = rest.chars();
                let head = chars
                    .next()
                    .is_some_and(|c| c.is_ascii_alphabetic() || c == '_');
                if head && rest.len() <= 255 && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
                {
                    Ok(Self::Env {
                        name: rest.to_owned(),
                    })
                } else {
                    Err(SecretRefError::EnvName)
                }
            }
            "exec" => clean_path(rest)
                .filter(|_| !rest.chars().any(char::is_whitespace))
                .map(|path| Self::Exec { path })
                .ok_or(SecretRefError::ExecPath),
            _ => Err(SecretRefError::UnknownScheme),
        }
    }
}

impl fmt::Display for SecretRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Keychain { service, account } => write!(f, "keychain:{service}/{account}"),
            Self::File { path } => write!(f, "file:{}", path.display()),
            Self::Env { name } => write!(f, "env:{name}"),
            Self::Exec { path } => write!(f, "exec:{}", path.display()),
        }
    }
}
