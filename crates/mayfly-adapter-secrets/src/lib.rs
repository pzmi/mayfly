//! `mayfly-adapter-secrets` (control plane): the `SecretProvider` backends, one per reference
//! scheme (ADR 0033), and [`UriSecretProvider`], which picks one by the reference's scheme.
//!
//! - [`FileProvider`] (`file:`): an owner-only file in an owner-only directory, opened without
//!   following a symlink and checked on the opened descriptor, so nothing is swapped between
//!   the check and the read.
//! - [`EnvProvider`] (`env:`): the daemon's variables, captured once at start.
//! - [`ExecProvider`] (`exec:`): an operator command's stdout, through the shell-free runner.
//! - [`KeychainProvider`] (`keychain:`): `/usr/bin/security` through the same runner (ADR
//!   0058); the system provider exists on macOS only, and the real Keychain runs in the
//!   hardware lane (D14).
//!
//! Every one passes the `SecretProvider` contract (`tests/contract.rs`, ver §5). No value is
//! cached, logged, written or handed to a child (ADR 0014): a provider reads its source on each
//! call. Only its `command` module holds process authority and spawns blocking work (ADR 0058).

mod command;
mod env;
mod exec;
mod file;
mod keychain;
mod path;
mod uri;

pub use env::EnvProvider;
pub use exec::{DEFAULT_DEADLINE, ExecProvider, PASSED_VARIABLES};
pub use file::FileProvider;
pub use keychain::KeychainProvider;
pub use uri::UriSecretProvider;
