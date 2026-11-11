//! The launcher's answers as typed, bounded values (plan §8.3, I30): what the guest claims is
//! parsed into a [`GuestClaims`] or a [`RunnerStatus`], or refused. The runner already bounds
//! the whole answer (the transport's claims limit); here each field is bounded too: a nonce of
//! exactly 32 lowercase hex digits, an operating system of a known name, a MAC of at most
//! [`MAC_LIMIT`] characters of `[0-9a-f:]`, an exit code that fits `i32`, and `v` 1. Unknown
//! fields are ignored, so a newer launcher may add some. Pinned by the tests below.

use std::str::FromStr;

use mayfly_data::{GuestOs, Nonce, RunnerStatus};
use mayfly_domain::Millis;
use serde::Deserialize;

/// The longest MAC a claim may name: six octets, colon-separated.
pub const MAC_LIMIT: usize = 17;

/// What a guest claimed in answer to a challenge. Never a proof of ownership (I30): the
/// transport only compares it with what it sent and what Tart lists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuestClaims {
    /// The nonce the guest read back.
    pub nonce: Nonce,
    /// Its operating system.
    pub os: GuestOs,
    /// Its uptime by its own clock.
    pub uptime: Millis,
    /// The MAC of its network interface, lower case.
    pub mac: String,
}

/// An answer that does not have the launcher contract's shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Malformed;

#[derive(Deserialize)]
struct ClaimsWire {
    v: u32,
    nonce: String,
    os: String,
    uptime_ms: u64,
    mac: String,
}

#[derive(Deserialize)]
struct StatusWire {
    v: u32,
    state: String,
    code: Option<i64>,
}

fn one_line(bytes: &[u8]) -> Result<&str, Malformed> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| Malformed)?
        .trim_end();
    if text.contains('\n') {
        return Err(Malformed);
    }
    Ok(text)
}

/// Parses a `verify` answer.
///
/// # Errors
/// [`Malformed`] for anything but one JSON line of the contract's shape and bounds.
pub fn parse_claims(bytes: &[u8]) -> Result<GuestClaims, Malformed> {
    let wire: ClaimsWire = serde_json::from_str(one_line(bytes)?).map_err(|_| Malformed)?;
    if wire.v != 1 {
        return Err(Malformed);
    }
    let os = match wire.os.as_str() {
        "linux" => GuestOs::Linux,
        "macos" => GuestOs::Macos,
        _ => return Err(Malformed),
    };
    let mac = wire.mac.to_ascii_lowercase();
    if mac.is_empty()
        || mac.len() > MAC_LIMIT
        || !mac.bytes().all(|b| b.is_ascii_hexdigit() || b == b':')
    {
        return Err(Malformed);
    }
    Ok(GuestClaims {
        nonce: Nonce::from_str(&wire.nonce).map_err(|_| Malformed)?,
        os,
        uptime: Millis::new(wire.uptime_ms),
        mac,
    })
}

/// Parses a `status` answer: the launcher's status file.
///
/// # Errors
/// [`Malformed`] for anything but one JSON line of the contract's shape and bounds.
pub fn parse_status(bytes: &[u8]) -> Result<RunnerStatus, Malformed> {
    let wire: StatusWire = serde_json::from_str(one_line(bytes)?).map_err(|_| Malformed)?;
    if wire.v != 1 {
        return Err(Malformed);
    }
    match (wire.state.as_str(), wire.code) {
        ("none", None) => Ok(RunnerStatus::NotLaunched),
        ("running", None) => Ok(RunnerStatus::Running),
        ("exited", Some(code)) => Ok(RunnerStatus::Exited {
            code: i32::try_from(code).map_err(|_| Malformed)?,
        }),
        _ => Err(Malformed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONCE: &str = "0123456789abcdef0011223344556677";

    #[test]
    fn should_parse_claims_when_they_have_the_contract_shape() {
        // given
        let line = format!(
            "{{\"v\":1,\"nonce\":\"{NONCE}\",\"os\":\"macos\",\"uptime_ms\":42,\"mac\":\"7E:0B:14:52:9A:C1\",\"extra\":true}}\n"
        );

        // when
        let parsed = parse_claims(line.as_bytes());

        // then
        assert_eq!(
            parsed,
            Ok(GuestClaims {
                nonce: Nonce::from_str(NONCE).expect("a nonce"),
                os: GuestOs::Macos,
                uptime: Millis::new(42),
                mac: "7e:0b:14:52:9a:c1".to_owned(),
            })
        );
    }

    #[test]
    fn should_refuse_claims_when_a_field_is_out_of_shape_or_bounds() {
        // given
        let good = |field: &str, value: &str| {
            let mut v = serde_json::json!({
                "v": 1, "nonce": NONCE, "os": "linux", "uptime_ms": 1, "mac": "0a:00:00:00:00:01"
            });
            v[field] = serde_json::from_str(value).expect("json");
            v.to_string()
        };
        let bad = [
            good("v", "2"),
            good("nonce", "\"0123\""),
            good("nonce", "\"0123456789ABCDEF0011223344556677\""),
            good("os", "\"windows\""),
            good("mac", "\"0a:00:00:00:00:01:02:03\""),
            good("mac", "\"0a;00\""),
            good("mac", "\"\""),
            good("uptime_ms", "-1"),
            "not json".to_owned(),
            format!("{}\n{}", good("v", "1"), good("v", "1")),
        ];

        // when
        let parsed: Vec<_> = bad.iter().map(|b| parse_claims(b.as_bytes())).collect();

        // then
        assert!(parsed.iter().all(|p| *p == Err(Malformed)), "{parsed:?}");
    }

    #[test]
    fn should_read_each_status_when_the_status_file_has_the_contract_shape() {
        // given
        let lines = [
            "{\"v\":1,\"state\":\"none\"}",
            "{\"v\":1,\"state\":\"running\"}",
            "{\"v\":1,\"state\":\"exited\",\"code\":3}\n",
            "{\"v\":1,\"state\":\"exited\"}",
            "{\"v\":1,\"state\":\"running\",\"code\":0}",
            "{\"v\":1,\"state\":\"exited\",\"code\":4294967296}",
            "{\"v\":2,\"state\":\"none\"}",
        ];

        // when
        let parsed = lines.map(|l| parse_status(l.as_bytes()));

        // then
        assert_eq!(
            parsed,
            [
                Ok(RunnerStatus::NotLaunched),
                Ok(RunnerStatus::Running),
                Ok(RunnerStatus::Exited { code: 3 }),
                Err(Malformed),
                Err(Malformed),
                Err(Malformed),
                Err(Malformed),
            ]
        );
    }
}
