//! The two instant formats GitHub sends: RFC 3339 (`expires_at`) and the HTTP date of a
//! `Retry-After` header (RFC 9110 §5.6.7, IMF-fixdate).

use mayfly_domain::Millis;

/// `2026-11-10T12:00:00Z` (or with `+hh:mm`/`-hh:mm`, and fractional seconds ignored) as a
/// Unix-epoch instant; `None` for anything else.
#[must_use]
pub fn parse_rfc3339(text: &str) -> Option<Millis> {
    let bytes = text.as_bytes();
    if bytes.len() < 20 || bytes[4] != b'-' || bytes[7] != b'-' || !matches!(bytes[10], b'T' | b't')
    {
        return None;
    }
    let year = number(text, 0, 4)?;
    let month = number(text, 5, 7)?;
    let day = number(text, 8, 10)?;
    let (hour, minute, second) = clock(text, 11)?;
    let mut rest = &text[19..];
    if let Some(fraction) = rest.strip_prefix('.') {
        let digits = fraction.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 {
            return None;
        }
        rest = &fraction[digits..];
    }
    let offset: i64 = match rest {
        "Z" | "z" => 0,
        _ if rest.len() == 6 && rest.as_bytes()[3] == b':' => {
            let sign = match rest.as_bytes()[0] {
                b'+' => 1,
                b'-' => -1,
                _ => return None,
            };
            let hours = i64::try_from(number(rest, 1, 3)?).ok()?;
            let minutes = i64::try_from(number(rest, 4, 6)?).ok()?;
            sign * (hours * 3600 + minutes * 60)
        }
        _ => return None,
    };
    let local = epoch_secs(year, month, day, hour, minute, second)?;
    let utc = i64::try_from(local).ok()?.checked_sub(offset)?;
    Some(Millis::new(u64::try_from(utc).ok()?.checked_mul(1000)?))
}

/// `Sun, 06 Nov 1994 08:49:37 GMT` as a Unix-epoch instant; `None` for anything else (the
/// obsolete RFC 850 and asctime forms are not sent by GitHub and are not read).
#[must_use]
pub fn parse_http_date(text: &str) -> Option<Millis> {
    let text = text.trim();
    let (_, rest) = text.split_once(", ")?;
    let mut parts = rest.split(' ');
    let (Some(day), Some(month), Some(year), Some(time), Some("GMT"), None) = (
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
    ) else {
        return None;
    };
    let month = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ]
    .iter()
    .position(|m| *m == month)?;
    if day.len() != 2 || year.len() != 4 || time.len() != 8 {
        return None;
    }
    let (hour, minute, second) = clock(time, 0)?;
    let secs = epoch_secs(
        number(year, 0, 4)?,
        u64::try_from(month).ok()? + 1,
        number(day, 0, 2)?,
        hour,
        minute,
        second,
    )?;
    Some(Millis::new(secs.checked_mul(1000)?))
}

fn number(text: &str, from: usize, to: usize) -> Option<u64> {
    let digits = text.get(from..to)?;
    if !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

fn clock(text: &str, at: usize) -> Option<(u64, u64, u64)> {
    let bytes = text.as_bytes();
    if bytes.get(at + 2) != Some(&b':') || bytes.get(at + 5) != Some(&b':') {
        return None;
    }
    Some((
        number(text, at, at + 2)?,
        number(text, at + 3, at + 5)?,
        number(text, at + 6, at + 8)?,
    ))
}

/// Seconds since the epoch of a proleptic Gregorian UTC date and time (Howard Hinnant's
/// `days_from_civil`); `None` for a field out of range or a date before 1970.
fn epoch_secs(year: u64, month: u64, day: u64, hour: u64, minute: u64, second: u64) -> Option<u64> {
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    let y = if month <= 2 {
        year.checked_sub(1)?
    } else {
        year
    };
    let era = y / 400;
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = (era * 146_097 + doe).checked_sub(719_468)?;
    Some(days * 86_400 + hour * 3600 + minute * 60 + second)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_read_instants_when_github_formats_them() {
        // given
        let cases = [
            ("2026-11-10T12:00:00Z", Some(1_794_312_000_000)),
            ("2026-11-10T12:00:00.123Z", Some(1_794_312_000_000)),
            ("2026-11-10T13:30:00+01:30", Some(1_794_312_000_000)),
            ("2026-11-10T10:00:00-02:00", Some(1_794_312_000_000)),
            ("2000-02-29T00:00:00Z", Some(951_782_400_000)),
            ("1970-01-01T00:00:00Z", Some(0)),
            ("2026-13-10T12:00:00Z", None),
            ("2026-11-10 12:00:00Z", None),
            ("2026-11-10T12:00:00", None),
            ("2026-11-10T12:00:00.Z", None),
            ("2026-11-10T12:00:00*01:00", None),
            ("1969-12-31T23:59:59Z", None),
            ("soon", None),
        ];

        // when
        let got: Vec<_> = cases
            .iter()
            .map(|(t, _)| parse_rfc3339(t).map(Millis::get))
            .collect();

        // then
        let expected: Vec<_> = cases.iter().map(|(_, e)| *e).collect();
        assert_eq!(got, expected);
    }

    #[test]
    fn should_read_an_imf_fixdate_when_retry_after_is_a_date() {
        // given
        let cases = [
            ("Tue, 10 Nov 2026 12:00:00 GMT", Some(1_794_312_000_000)),
            ("Sun, 06 Nov 1994 08:49:37 GMT", Some(784_111_777_000)),
            ("Sunday, 06-Nov-94 08:49:37 GMT", None),
            ("Sun Nov  6 08:49:37 1994", None),
            ("Sun, 06 Nov 1994 08:49:37 UTC", None),
            ("Sun, 06 Foo 1994 08:49:37 GMT", None),
            ("Sun, 6 Nov 1994 08:49:37 GMT", None),
            ("Sun, 06 Nov 1994 08-49-37 GMT", None),
        ];

        // when
        let got: Vec<_> = cases
            .iter()
            .map(|(t, _)| parse_http_date(t).map(Millis::get))
            .collect();

        // then
        let expected: Vec<_> = cases.iter().map(|(_, e)| *e).collect();
        assert_eq!(got, expected);
    }
}
