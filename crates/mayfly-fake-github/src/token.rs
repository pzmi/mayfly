//! JSON Web Tokens as the fake sees them: the App JWT it verifies and the admin token it issues.

use aws_lc_rs::signature::{RSA_PKCS1_2048_8192_SHA256, UnparsedPublicKey};
use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use serde::Deserialize;

/// The claims of an App JWT the fake accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JwtClaims {
    /// `iat`, Unix-epoch seconds.
    pub issued_at: u64,
    /// `exp`, Unix-epoch seconds.
    pub expires_at: u64,
}

/// Why an App JWT was refused; the fake answers 401 for each.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JwtRefusal {
    Shape,
    Algorithm,
    Signature,
    Issuer,
    IssuedInFuture,
    Expired,
    TooLong,
}

#[derive(Deserialize)]
struct Header {
    alg: String,
}

#[derive(Deserialize)]
struct Claims {
    iat: u64,
    exp: u64,
    iss: String,
}

/// GitHub refuses an App JWT whose expiry is more than ten minutes away [docs].
const MAX_LIFETIME_SECS: u64 = 600;

/// Verifies `token` as an RS256 App JWT signed by `public_pem`'s key for `issuer`, at `now`
/// (Unix-epoch seconds).
pub fn verify_app_jwt(
    token: &str,
    public_pem: &str,
    issuer: &str,
    now: u64,
) -> Result<JwtClaims, JwtRefusal> {
    let mut parts = token.split('.');
    let (Some(header), Some(claims), Some(signature), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(JwtRefusal::Shape);
    };
    let header: Header = decode_json(header)?;
    if header.alg != "RS256" {
        return Err(JwtRefusal::Algorithm);
    }
    let signature = URL_SAFE_NO_PAD
        .decode(signature)
        .map_err(|_| JwtRefusal::Shape)?;
    let key = public_der(public_pem).ok_or(JwtRefusal::Signature)?;
    let signed = &token[..token.rfind('.').unwrap_or(0)];
    UnparsedPublicKey::new(&RSA_PKCS1_2048_8192_SHA256, key)
        .verify(signed.as_bytes(), &signature)
        .map_err(|_| JwtRefusal::Signature)?;
    let claims: Claims = decode_json(claims)?;
    if claims.iss != issuer {
        return Err(JwtRefusal::Issuer);
    }
    if claims.iat > now {
        return Err(JwtRefusal::IssuedInFuture);
    }
    if claims.exp <= now {
        return Err(JwtRefusal::Expired);
    }
    if claims.exp - claims.iat > MAX_LIFETIME_SECS || claims.exp - now > MAX_LIFETIME_SECS {
        return Err(JwtRefusal::TooLong);
    }
    Ok(JwtClaims {
        issued_at: claims.iat,
        expires_at: claims.exp,
    })
}

/// The DER inside a PKCS#1 `RSA PUBLIC KEY` PEM block, read with the fake's own few lines.
fn public_der(pem: &str) -> Option<Vec<u8>> {
    let body: String = pem
        .lines()
        .skip_while(|l| l.trim() != "-----BEGIN RSA PUBLIC KEY-----")
        .skip(1)
        .take_while(|l| l.trim() != "-----END RSA PUBLIC KEY-----")
        .map(str::trim)
        .collect();
    STANDARD.decode(body).ok().filter(|der| !der.is_empty())
}

fn decode_json<T: for<'de> Deserialize<'de>>(part: &str) -> Result<T, JwtRefusal> {
    let bytes = URL_SAFE_NO_PAD
        .decode(part)
        .map_err(|_| JwtRefusal::Shape)?;
    serde_json::from_slice(&bytes).map_err(|_| JwtRefusal::Shape)
}

/// An admin token in the service's shape, a JWT the client reads `exp` from without verifying
/// it (spec §1 step 4); `exp` is left out when `expires_at` is `None`. Its signature is filler:
/// nothing verifies it.
pub fn admin_token(serial: u64, expires_at: Option<u64>) -> String {
    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256","typ":"JWT"}"#);
    let claims = match expires_at {
        Some(exp) => format!(r#"{{"sub":"fake-admin-{serial}","exp":{exp}}}"#),
        None => format!(r#"{{"sub":"fake-admin-{serial}"}}"#),
    };
    let claims = URL_SAFE_NO_PAD.encode(claims.as_bytes());
    let signature = URL_SAFE_NO_PAD.encode(format!("fake-signature-{serial}").as_bytes());
    format!("{header}.{claims}.{signature}")
}

#[cfg(test)]
mod tests {
    use rsa::pkcs1::EncodeRsaPublicKey;
    use rsa::sha2::Sha256;
    use rsa::{Generate, PrivateKey};

    use super::*;

    fn sign(key: &PrivateKey, header: &str, claims: &str) -> String {
        let signed = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(header.as_bytes()),
            URL_SAFE_NO_PAD.encode(claims.as_bytes())
        );
        let signature = key
            .pkcs1v15_sign::<Sha256>(signed.as_bytes())
            .expect("sign");
        format!("{signed}.{}", URL_SAFE_NO_PAD.encode(signature))
    }

    #[test]
    fn should_refuse_each_flaw_when_verifying_an_app_jwt() {
        // given
        let key = PrivateKey::generate();
        let public = key
            .to_public_key()
            .to_pkcs1_pem(rsa::pkcs1::LineEnding::LF)
            .expect("pem");
        let rs = r#"{"alg":"RS256"}"#;
        let now = 1_000_000;
        let claims =
            |iat: u64, exp: u64, iss: &str| format!(r#"{{"iat":{iat},"exp":{exp},"iss":"{iss}"}}"#);
        let good = sign(&key, rs, &claims(now - 60, now + 540, "app"));
        let mut parts: Vec<&str> = good.split('.').collect();
        let forged =
            URL_SAFE_NO_PAD.encode(claims(now - 60, now + 540, "app").replace("app", "apq"));
        parts[1] = &forged;
        let tampered = parts.join(".");
        let cases = [
            (
                good.clone(),
                Ok(JwtClaims {
                    issued_at: now - 60,
                    expires_at: now + 540,
                }),
            ),
            ("a.b".to_owned(), Err(JwtRefusal::Shape)),
            ("a.b.c.d".to_owned(), Err(JwtRefusal::Shape)),
            (
                sign(&key, r#"{"alg":"HS256"}"#, &claims(now, now + 60, "app")),
                Err(JwtRefusal::Algorithm),
            ),
            (tampered, Err(JwtRefusal::Signature)),
            (
                sign(&key, rs, &claims(now, now + 60, "other")),
                Err(JwtRefusal::Issuer),
            ),
            (
                sign(&key, rs, &claims(now + 5, now + 60, "app")),
                Err(JwtRefusal::IssuedInFuture),
            ),
            (
                sign(&key, rs, &claims(now - 600, now, "app")),
                Err(JwtRefusal::Expired),
            ),
            (
                sign(&key, rs, &claims(now - 60, now + 541, "app")),
                Err(JwtRefusal::TooLong),
            ),
            (sign(&key, rs, "{}"), Err(JwtRefusal::Shape)),
        ];

        // when
        let got: Vec<_> = cases
            .iter()
            .map(|(token, _)| verify_app_jwt(token, &public, "app", now))
            .collect();

        // then
        let expected: Vec<_> = cases.iter().map(|(_, e)| *e).collect();
        assert_eq!(got, expected);
        assert_eq!(
            verify_app_jwt(&good, "not a key", "app", now),
            Err(JwtRefusal::Signature)
        );
    }
}
