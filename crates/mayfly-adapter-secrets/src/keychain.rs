//! `keychain:<service>/<account>`: a macOS Keychain item, read with `/usr/bin/security
//! find-generic-password -s <service> -a <account> -w` through the shell-free runner (ADR 0058).
//!
//! The provider is portable code over a program path, so its tests run anywhere over a fake
//! `security` (`tests/contract.rs`); only [`KeychainProvider::system`] is macOS-only, and the
//! real Keychain runs in the hardware lane (D14). The child gets the same few variables as an
//! `exec:` command (it finds the user's keychains through them) and no stdin: nothing can
//! answer a prompt, so a locked keychain is refused, and anything that waits is cut by the
//! deadline.
//!
//! `security -w` ends the password with a line break, which is dropped; then the port's one
//! rule applies as to every source, so an item whose value itself ends in a line break resolves
//! without it, as a file would (ADR 0058; pinned by the contract's SP-06 over the fake). It prints a password holding non-printable bytes in hex, which then resolves
//! to that hex text: Keychain secrets are text (ADR 0058).

use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

use mayfly_control::secrets::{
    MAX_SECRET_BYTES, Refusal, SecretError, SecretProvider, secret_from_bytes,
};
use mayfly_domain::{Secret, SecretRef};

use crate::command::run;
use crate::exec::ChildEnvironment;

/// `security`'s exit status for `errSecItemNotFound` (-25300, low byte).
const ITEM_NOT_FOUND: i32 = 44;
/// `errSecInteractionNotAllowed` (-25308) and `errSecAuthFailed` (-25293), low bytes.
const LOCKED: [i32; 2] = [36, 51];

/// Reads Keychain items through the `security` tool.
#[derive(Debug, Clone)]
pub struct KeychainProvider {
    program: PathBuf,
    environment: ChildEnvironment,
    deadline: Duration,
}

impl KeychainProvider {
    /// The system's `/usr/bin/security`, given exactly `environment`.
    #[cfg(target_os = "macos")]
    #[must_use]
    pub fn system(environment: Vec<(OsString, OsString)>, deadline: Duration) -> Self {
        // Composition reaches this through `UriSecretProvider::new`, which hands it the `exec:`
        // environment with every `env:` name already withheld.
        Self::with_program(PathBuf::from("/usr/bin/security"), environment, deadline)
    }

    /// A provider over another `security`-compatible program (a fake, in tests).
    #[must_use]
    pub const fn with_program(
        program: PathBuf,
        environment: Vec<(OsString, OsString)>,
        deadline: Duration,
    ) -> Self {
        Self {
            program,
            environment: ChildEnvironment(environment),
            deadline,
        }
    }
}

impl SecretProvider for KeychainProvider {
    async fn resolve(&self, reference: &SecretRef) -> Result<Secret<String>, SecretError> {
        let SecretRef::Keychain { service, account } = reference else {
            return Err(SecretError::new(reference, Refusal::Unsupported));
        };
        let args = [
            "find-generic-password",
            "-s",
            service.as_str(),
            "-a",
            account.as_str(),
            "-w",
        ];
        // One byte more than the port's limit: the tool's own line break.
        let finished = run(
            reference,
            &self.program,
            &args,
            &self.environment.0,
            self.deadline,
            MAX_SECRET_BYTES + 1,
        )
        .await?;
        match finished.status.code() {
            Some(0) => {
                // `security -w` ends the password with a line break of its own.
                let mut stdout = finished.stdout;
                if stdout.last() == Some(&b'\n') {
                    stdout.pop();
                }
                secret_from_bytes(reference, stdout)
            }
            Some(ITEM_NOT_FOUND) => Err(SecretError::new(reference, Refusal::NotFound)),
            Some(code) if LOCKED.contains(&code) => {
                Err(SecretError::new(reference, Refusal::Locked))
            }
            code => Err(SecretError::new(reference, Refusal::Exited { code })),
        }
    }
}
