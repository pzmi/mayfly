//! [`UriSecretProvider`]: the provider composition hands the control plane, which picks the
//! backend by the reference's scheme (ADR 0033).

use std::time::Duration;

use mayfly_control::secrets::{SecretError, SecretProvider};
use mayfly_domain::{Secret, SecretRef};

use crate::{EnvProvider, ExecProvider, FileProvider};

/// Every scheme's backend; `keychain:` on macOS only.
#[derive(Debug)]
pub struct UriSecretProvider {
    file: FileProvider,
    env: EnvProvider,
    exec: ExecProvider,
    #[cfg(target_os = "macos")]
    keychain: crate::KeychainProvider,
}

impl UriSecretProvider {
    /// A provider over the given backends. Whatever `exec` was built with, its children (and,
    /// on macOS, the system Keychain tool, which gets the same environment and deadline) are
    /// never given a variable `env` captured: an `env:` secret reaches no child (ADR 0033,
    /// ADR 0058; pinned by `tests/uri.rs::should_withhold_env_secrets_when_exec_runs_a_command`).
    #[must_use]
    pub fn new(file: FileProvider, env: EnvProvider, exec: ExecProvider) -> Self {
        let captured: Vec<&str> = env.names().collect();
        let exec = exec.withholding(&captured);
        Self {
            #[cfg(target_os = "macos")]
            keychain: crate::KeychainProvider::system(exec.environment.0.clone(), exec.deadline),
            file,
            env,
            exec,
        }
    }

    /// The daemon's provider: files owned by its user; the variables `references` name,
    /// captured now; commands of root or its user that get its basic variables less those
    /// names, with `deadline`.
    #[must_use]
    pub fn from_process(references: &[SecretRef], deadline: Duration) -> Self {
        let names: Vec<&str> = references
            .iter()
            .filter_map(|reference| match reference {
                SecretRef::Env { name } => Some(name.as_str()),
                _ => None,
            })
            .collect();
        Self::new(
            FileProvider::for_daemon_user(),
            EnvProvider::capture(names.iter().copied()),
            ExecProvider::for_daemon(&names, deadline),
        )
    }

    #[cfg(target_os = "macos")]
    async fn keychain(&self, reference: &SecretRef) -> Result<Secret<String>, SecretError> {
        self.keychain.resolve(reference).await
    }

    #[cfg(not(target_os = "macos"))]
    async fn keychain(&self, reference: &SecretRef) -> Result<Secret<String>, SecretError> {
        Err(SecretError::new(
            reference,
            mayfly_control::secrets::Refusal::Unsupported,
        ))
    }
}

impl SecretProvider for UriSecretProvider {
    async fn resolve(&self, reference: &SecretRef) -> Result<Secret<String>, SecretError> {
        match reference {
            SecretRef::File { .. } => self.file.resolve(reference).await,
            SecretRef::Env { .. } => self.env.resolve(reference).await,
            SecretRef::Exec { .. } => self.exec.resolve(reference).await,
            SecretRef::Keychain { .. } => self.keychain(reference).await,
        }
    }
}
