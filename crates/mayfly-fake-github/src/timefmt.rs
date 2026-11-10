//! RFC 3339 instants in UTC, as GitHub writes `expires_at` (`2026-11-10T12:00:00Z`).

/// Formats Unix-epoch seconds as `YYYY-MM-DDTHH:MM:SSZ`.
pub fn rfc3339(epoch_secs: u64) -> String {
    let days = epoch_secs / 86_400;
    let rest = epoch_secs % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rest / 3600,
        (rest % 3600) / 60,
        rest % 60
    )
}

/// The proleptic Gregorian date `days` after 1970-01-01 (Howard Hinnant's algorithm).
fn civil_from_days(days: u64) -> (u64, u64, u64) {
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + u64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_write_utc_text_when_formatting_known_instants() {
        // given
        let instants = [
            (0, "1970-01-01T00:00:00Z"),
            (951_782_400, "2000-02-29T00:00:00Z"),
            (1_794_312_000, "2026-11-10T12:00:00Z"),
        ];

        // when
        let written: Vec<String> = instants.iter().map(|(s, _)| rfc3339(*s)).collect();

        // then
        let expected: Vec<&str> = instants.iter().map(|(_, t)| *t).collect();
        assert_eq!(written, expected);
    }
}
