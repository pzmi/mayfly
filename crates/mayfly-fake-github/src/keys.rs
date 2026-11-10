//! Synthetic App keys, generated at run time (`.gitleaks.toml`: no private key is committed).

use rsa::pkcs1::{EncodeRsaPrivateKey, EncodeRsaPublicKey, LineEnding};
use rsa::{Generate, PrivateKey};

/// A freshly generated RSA key pair in the PEM shapes GitHub hands out: the private key as
/// PKCS#1 (`BEGIN RSA PRIVATE KEY`, what an App's downloaded key file holds) and the public
/// key as PKCS#1, which [`crate::FakeConfig::app_public_key_pem`] takes.
pub struct AppKeyPair {
    /// The private key, PKCS#1 PEM. Synthetic and test-only.
    pub private_pem: String,
    /// The public key, PKCS#1 PEM.
    pub public_pem: String,
}

impl std::fmt::Debug for AppKeyPair {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AppKeyPair(<synthetic>)")
    }
}

/// Generates a 3072-bit key pair from the operating system's generator (about a second in a
/// debug build: generate once per test binary).
///
/// # Panics
/// When the generator or the encoder fails, which a test cannot recover from.
#[must_use]
pub fn generate_app_key() -> AppKeyPair {
    let key = PrivateKey::generate();
    let private_pem = key
        .to_pkcs1_pem(LineEnding::LF)
        .expect("encode the synthetic private key")
        .to_string();
    let public_pem = key
        .to_public_key()
        .to_pkcs1_pem(LineEnding::LF)
        .expect("encode the synthetic public key");
    AppKeyPair {
        private_pem,
        public_pem,
    }
}
