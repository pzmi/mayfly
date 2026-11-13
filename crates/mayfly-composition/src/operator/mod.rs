//! The operator interface (plan §11, RT-03): `mayfly` talks to `mayflyd` over a `0600` Unix
//! socket in a directory private to the daemon's user.
//!
//! - [`wire`]: the NDJSON request and answer documents, API 1, a compatibility surface.
//! - [`server`]: the daemon's end: the socket, the peer-uid check, the only place an operator
//!   actor is built (`ARCH-operator-actor`), the commands and queries it serves.
//! - [`status`]: `status`, `pool list` and `vm list` from the control plane's views.
//! - [`client`]: the CLI's blocking end.
//! - [`doctor`]: `mayfly doctor`, basic.
//!
//! The socket's place is [`crate::paths::socket`]; [`socket_for`] is the CLI's way to it.

pub mod client;
pub mod doctor;
pub mod server;
pub mod status;
pub mod wire;

use std::path::{Path, PathBuf};

use crate::config::{Config, LoadError};
use crate::paths::{self, PlatformEnv};

/// Why the CLI could not tell where the socket is.
#[derive(Debug)]
pub enum LocateError {
    /// The configuration given does not load.
    Config(LoadError),
    /// Neither the configuration nor the platform names a state directory (no usable `HOME`).
    NoPlace,
}

impl std::fmt::Display for LocateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Config(e) => e.fmt(f),
            Self::NoPlace => f.write_str(
                "no default place for the socket: HOME is not set; pass --socket <path> or --config <path>",
            ),
        }
    }
}

impl std::error::Error for LocateError {}

/// Where `mayflyd` listens, as the CLI finds it: the socket of the configuration at `config`
/// when one is given (its `state_dir`, else the platform's places), else the platform's
/// default ([`paths::socket`], the rule `mayflyd` binds by). Pinned by
/// `crates/mayfly-cli/tests/operator.rs::should_answer_status_when_the_cli_is_given_the_daemons_config`.
///
/// # Errors
/// [`LocateError`].
pub fn socket_for(config: Option<&Path>, env: &PlatformEnv) -> Result<PathBuf, LocateError> {
    let state_dir = match config {
        Some(path) => Config::load(path).map_err(LocateError::Config)?.state_dir,
        None => None,
    };
    let places = paths::resolve(state_dir.as_deref(), None, paths::defaults(env))
        .ok_or(LocateError::NoPlace)?;
    Ok(paths::socket(state_dir.as_deref(), &places, env))
}
