//! Wall-clock helpers: milliseconds since the Unix epoch and Tart's ISO 8601 `Accessed` format.

use std::time::{SystemTime, UNIX_EPOCH};

/// Milliseconds since the Unix epoch.
pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// `YYYY-MM-DDThh:mm:ssZ`, what Foundation's `ISO8601DateFormatter` prints for `list --format json`.
pub(crate) fn iso8601(ms: u64) -> String {
    let secs = ms / 1000;
    let days = i64::try_from(secs / 86_400).unwrap_or(0);
    let rem = secs % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// Days since 1970-01-01 to a proleptic Gregorian date (Howard Hinnant's `civil_from_days`).
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::iso8601;

    #[test]
    fn should_format_the_epoch_when_given_zero() {
        // given
        let ms = 0;

        // when
        let text = iso8601(ms);

        // then
        assert_eq!(text, "1970-01-01T00:00:00Z");
    }

    #[test]
    fn should_format_a_leap_day_when_given_one() {
        // given
        let ms = 1_709_210_096_000; // 2024-02-29T12:34:56Z

        // when
        let text = iso8601(ms);

        // then
        assert_eq!(text, "2024-02-29T12:34:56Z");
    }

    #[test]
    fn should_format_a_date_before_march_when_the_year_rolls_back() {
        // given
        let ms = 951_782_400_000; // 2000-02-29T00:00:00Z

        // when
        let text = iso8601(ms);

        // then
        assert_eq!(text, "2000-02-29T00:00:00Z");
    }
}
