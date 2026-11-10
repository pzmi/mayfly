//! The App JWT (spec §1 step 1) and the admin token's expiry (spec §1 step 4).
//!
//! RS256 is signed with `aws-lc-rs` (ADR 0064), the same provider the TLS stack uses, so
//! signing adds no crypto library. A JWT is three base64url parts; building one by hand is a few
//! lines and keeps a general JWT library (and the RSA crates it pulls) out of the production
//! graph.

use std::fmt;

use aws_lc_rs::rand::SystemRandom;
use aws_lc_rs::signature::{RSA_PKCS1_SHA256, RsaKeyPair};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use mayfly_domain::{Millis, Secret};
use rustls_pki_types::PrivateKeyDer;
use rustls_pki_types::pem::PemObject;
use serde::Deserialize;
use zeroize::Zeroize;

/// How far `iat` is set back for clock skew between this host and GitHub (spec §1 step 1).
pub const JWT_BACKDATE_SECS: u64 = 60;

/// `exp - iat`: GitHub refuses an App JWT that expires more than ten minutes ahead [docs]; with
/// `iat` set back a minute, `exp` lands nine minutes from now, a minute inside that limit.
pub const JWT_LIFETIME_SECS: u64 = 600;

/// The App's private key, parsed. Its `Debug` prints nothing of it.
pub struct AppKey(RsaKeyPair);

impl fmt::Debug for AppKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AppKey(<redacted>)")
    }
}

/// Why a key or a token could not be used. Carries no byte of either.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JwtError {
    /// The key is not a PEM private key (PKCS#1 `RSA PRIVATE KEY` or PKCS#8 `PRIVATE KEY`).
    KeyNotPem,
    /// The key parsed but is not an RSA key the provider accepts (2048 to 8192 bits).
    KeyRejected,
    /// Signing failed.
    Sign,
    /// The admin token is not a JWT with a JSON claims part.
    TokenShape,
    /// The admin token has no numeric `exp` claim (refused, spec §1 step 4).
    TokenWithoutExpiry,
}

impl fmt::Display for JwtError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::KeyNotPem => "the App key is not a PEM RSA private key",
            Self::KeyRejected => "the App key is not an RSA key of 2048 to 8192 bits",
            Self::Sign => "the App JWT could not be signed",
            Self::TokenShape => "the admin token is not a JWT",
            Self::TokenWithoutExpiry => "the admin token carries no exp claim",
        })
    }
}

impl std::error::Error for JwtError {}

impl AppKey {
    /// Parses a PEM key as GitHub hands it out (PKCS#1), or PKCS#8.
    ///
    /// # Errors
    /// [`JwtError::KeyNotPem`] or [`JwtError::KeyRejected`].
    ///
    /// The decoded DER buffer is zeroized before this returns. The PEM text stays in the
    /// caller's `Secret` (freed, not wiped); the provider's parsed key pair lives in
    /// `aws-lc`'s memory, which frees it with `OPENSSL_free` (a cleansing free) when dropped.
    pub fn from_pem(pem: &Secret<String>) -> Result<Self, JwtError> {
        let mut der = PrivateKeyDer::from_pem_slice(pem.expose().as_bytes())
            .map_err(|_| JwtError::KeyNotPem)?;
        let pair = match &der {
            PrivateKeyDer::Pkcs1(key) => RsaKeyPair::from_der(key.secret_pkcs1_der()).ok(),
            PrivateKeyDer::Pkcs8(key) => RsaKeyPair::from_pkcs8(key.secret_pkcs8_der()).ok(),
            _ => None,
        };
        der.zeroize();
        pair.map(Self).ok_or(JwtError::KeyRejected)
    }
}

/// A fresh App JWT for `issuer` at `now` (spec §1 step 1): `iat = now - 60 s`,
/// `exp = iat + 600 s`, RS256. Pinned by `tests/client/auth.rs::should_mint_an_admin_connection_when_the_chain_succeeds`,
/// where the fake verifies the signature and the claims with its own RSA code.
///
/// # Errors
/// [`JwtError::Sign`].
pub fn app_jwt(key: &AppKey, issuer: &str, now: Millis) -> Result<Secret<String>, JwtError> {
    let iat = (now.get() / 1000).saturating_sub(JWT_BACKDATE_SECS);
    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256","typ":"JWT"}"#);
    let claims = serde_json::json!({
        "iat": iat,
        "exp": iat + JWT_LIFETIME_SECS,
        "iss": issuer,
    });
    let claims = URL_SAFE_NO_PAD.encode(claims.to_string().as_bytes());
    let signed = format!("{header}.{claims}");
    let mut signature = vec![0; key.0.public_modulus_len()];
    // The provider takes a generator for its signing API only; PKCS#1 v1.5 signatures are
    // deterministic, so nothing observable depends on it and the randomness port is not involved.
    key.0
        .sign(
            &RSA_PKCS1_SHA256,
            &SystemRandom::new(),
            signed.as_bytes(),
            &mut signature,
        )
        .map_err(|_| JwtError::Sign)?;
    Ok(Secret::new(format!(
        "{signed}.{}",
        URL_SAFE_NO_PAD.encode(signature)
    )))
}

#[derive(Deserialize)]
struct Expiry {
    exp: Option<u64>,
}

/// The `exp` claim of `token`, read without verifying the signature (spec §1 step 4: the client
/// cannot verify it and only needs to know when to refresh), as a Unix-epoch instant.
///
/// # Errors
/// [`JwtError::TokenShape`], or [`JwtError::TokenWithoutExpiry`] for a token without `exp`.
pub fn token_expiry(token: &Secret<String>) -> Result<Millis, JwtError> {
    let claims = token
        .expose()
        .split('.')
        .nth(1)
        .ok_or(JwtError::TokenShape)?;
    let claims = URL_SAFE_NO_PAD
        .decode(claims.trim_end_matches('='))
        .map_err(|_| JwtError::TokenShape)?;
    let expiry: Expiry = serde_json::from_slice(&claims).map_err(|_| JwtError::TokenShape)?;
    let exp = expiry.exp.ok_or(JwtError::TokenWithoutExpiry)?;
    Ok(Millis::new(exp.saturating_mul(1000)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token(claims: &str) -> Secret<String> {
        Secret::new(format!(
            "e30.{}.c2ln",
            URL_SAFE_NO_PAD.encode(claims.as_bytes())
        ))
    }

    #[test]
    fn should_read_exp_when_the_token_carries_one() {
        // given
        let cases = [
            (
                token(r#"{"exp":1794312000}"#),
                Ok(Millis::new(1_794_312_000_000)),
            ),
            (token(r#"{"sub":"x"}"#), Err(JwtError::TokenWithoutExpiry)),
            (token(r#"{"exp":"soon"}"#), Err(JwtError::TokenShape)),
            (Secret::new("opaque".to_owned()), Err(JwtError::TokenShape)),
            (Secret::new("a.!!!.c".to_owned()), Err(JwtError::TokenShape)),
        ];

        // when
        let got: Vec<_> = cases.iter().map(|(t, _)| token_expiry(t)).collect();

        // then
        let expected: Vec<_> = cases.iter().map(|(_, e)| *e).collect();
        assert_eq!(got, expected);
    }

    #[test]
    fn should_sign_three_parts_when_the_key_is_pkcs8() {
        // given: a PKCS#8 key generated here (GitHub's own keys are PKCS#1, covered by
        // tests/client/auth.rs)
        use aws_lc_rs::encoding::AsDer;
        use aws_lc_rs::rsa::{KeyPair, KeySize};
        use base64::engine::general_purpose::STANDARD;
        let der = KeyPair::generate(KeySize::Rsa2048)
            .expect("generate")
            .as_der()
            .expect("der");
        let body = STANDARD.encode(der.as_ref());
        let label = "PRIVATE KEY";
        let pem = Secret::new(format!(
            "-----BEGIN {label}-----\n{body}\n-----END {label}-----\n"
        ));

        // when
        let parsed = AppKey::from_pem(&pem).expect("pkcs8 key");
        let jwt = app_jwt(&parsed, "app", Millis::new(1_794_312_000_000)).expect("jwt");

        // then
        let parts: Vec<&str> = jwt.expose().split('.').collect();
        assert_eq!(parts.len(), 3);
        assert_eq!(
            URL_SAFE_NO_PAD.decode(parts[2]).expect("signature").len(),
            256
        );
        assert_eq!(
            token_expiry(&jwt),
            Ok(Millis::new(
                (1_794_312_000 - JWT_BACKDATE_SECS + JWT_LIFETIME_SECS) * 1000
            ))
        );
    }

    #[test]
    fn should_refuse_when_the_key_is_not_an_rsa_pem() {
        // given
        // Armour built at run time: the secret scan matches a literal private-key block.
        let armoured = |kind: &str| {
            Secret::new(format!(
                "-----BEGIN {kind} PRIVATE KEY-----\nAAAA\n-----END {kind} PRIVATE KEY-----\n"
            ))
        };
        let keys = [
            Secret::new("not a key".to_owned()),
            armoured("RSA"),
            armoured("EC"),
        ];

        // when
        let got: Vec<_> = keys.iter().map(|k| AppKey::from_pem(k).err()).collect();

        // then
        assert_eq!(
            got,
            [
                Some(JwtError::KeyNotPem),
                Some(JwtError::KeyRejected),
                Some(JwtError::KeyRejected)
            ]
        );
        assert!(
            [JwtError::KeyNotPem, JwtError::Sign, JwtError::TokenShape]
                .iter()
                .all(|e| !e.to_string().is_empty())
        );
    }
}
