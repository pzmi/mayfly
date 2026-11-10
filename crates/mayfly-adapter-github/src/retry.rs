//! The retry policy of spec §8 as plain values and free functions: which answers are retried,
//! which are rate limits, and how long to wait. Nothing here sleeps or reads a clock; the
//! client applies the verdicts (`client.rs`).

use mayfly_control::store::Rng;
use mayfly_domain::Millis;

use crate::http::{HttpResponse, TransportError};
use crate::metrics::StatusClass;
use crate::timefmt::parse_http_date;

/// How a request is retried.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Retries after the first attempt.
    pub max_retries: u32,
    /// The first backoff; each later one doubles it.
    pub first_backoff: Millis,
    /// The largest backoff before jitter.
    pub max_backoff: Millis,
    /// Up to this many percent of the backoff are added, drawn from the randomness port.
    pub jitter_percent: u64,
    /// The least wait after a secondary rate limit that names no `Retry-After` (GitHub advises at
    /// least a minute [docs]).
    pub secondary_rate_limit_wait: Millis,
}

/// Spec §8: at most 4 retries, from 1 s doubling to a 30 s cap, with up to 50 % jitter.
pub const DEFAULT_RETRY_POLICY: RetryPolicy = RetryPolicy {
    max_retries: 4,
    first_backoff: Millis::new(1_000),
    max_backoff: Millis::new(30_000),
    jitter_percent: 50,
    secondary_rate_limit_wait: Millis::new(60_000),
};

/// Which step a request is: the runner-registration exchange retries more (spec §1 step 4, §8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Any request but the exchange.
    Ordinary,
    /// The runner-registration exchange: also retries 401 and 403 (a freshly minted
    /// registration token is not always usable at once), rate-limited 403 included.
    AdminExchange,
}

/// Which limit an answer reports (spec §8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateLimitKind {
    /// `X-RateLimit-Remaining: 0` without the secondary text.
    Primary,
    /// The body mentions a secondary rate limit.
    Secondary,
    /// A 429, or a 403 with `Retry-After`, that says neither.
    Unspecified,
}

/// A rate-limited answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateLimit {
    /// Which limit.
    pub kind: RateLimitKind,
    /// `X-RateLimit-Reset`, as an instant.
    pub reset_at: Option<Millis>,
}

/// What an attempt's answer means for the request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Verdict {
    /// The metric label.
    pub status_class: StatusClass,
    /// Whether another attempt may succeed.
    pub retryable: bool,
    /// The wait the answer asked for (`Retry-After`), honoured uncapped when retrying.
    pub retry_after: Option<Millis>,
    /// Set when the answer is a rate limit.
    pub rate_limit: Option<RateLimit>,
}

/// Classifies an HTTP answer (spec §8). `now` turns an HTTP-date `Retry-After` into a wait.
/// Pinned by `retry::tests::should_classify_each_answer_when_the_table_is_applied`.
#[must_use]
pub fn classify_response(response: &HttpResponse, phase: Phase, now: Millis) -> Verdict {
    let status = response.status;
    let retry_after = response
        .header("retry-after")
        .and_then(|value| parse_retry_after(value, now));
    let rate_limit = rate_limit(response, retry_after.is_some());
    let exchange = phase == Phase::AdminExchange;
    let (status_class, retryable) = match status {
        200..=299 => (StatusClass::Success, false),
        _ if rate_limit.is_some() => (StatusClass::RateLimited, status == 429 || exchange),
        401 | 403 => (StatusClass::ClientError, exchange),
        400..=499 => (StatusClass::ClientError, false),
        501 => (StatusClass::ServerError, false),
        500..=599 => (StatusClass::ServerError, true),
        _ => (StatusClass::Unexpected, false),
    };
    let honours_retry_after = rate_limit.is_some() || status == 503;
    Verdict {
        status_class,
        retryable,
        retry_after: retry_after.filter(|_| honours_retry_after),
        rate_limit,
    }
}

/// Whether an attempt with no HTTP answer may be retried: every transport error but a refused
/// certificate (spec §8). A body over the limit is not retried either: it would be again.
#[must_use]
pub const fn transport_retryable(error: TransportError) -> bool {
    matches!(error, TransportError::Connect | TransportError::Io)
}

/// The rate limit `response` reports, if any (spec §8): a 429, or a 403 with `Retry-After`,
/// `X-RateLimit-Remaining: 0` or a body mentioning a secondary rate limit.
fn rate_limit(response: &HttpResponse, has_retry_after: bool) -> Option<RateLimit> {
    let exhausted = response
        .header("x-ratelimit-remaining")
        .is_some_and(|v| v.trim() == "0");
    let secondary = String::from_utf8_lossy(response.body_without_bom())
        .to_ascii_lowercase()
        .contains("secondary rate limit");
    let limited = match response.status {
        429 => true,
        403 => has_retry_after || exhausted || secondary,
        _ => false,
    };
    if !limited {
        return None;
    }
    let kind = if secondary {
        RateLimitKind::Secondary
    } else if exhausted {
        RateLimitKind::Primary
    } else {
        RateLimitKind::Unspecified
    };
    let reset_at = response
        .header("x-ratelimit-reset")
        .and_then(|v| v.trim().parse::<u64>().ok())
        .and_then(|secs| secs.checked_mul(1000))
        .map(Millis::new);
    Some(RateLimit { kind, reset_at })
}

/// A `Retry-After` value as a wait: whole seconds, or an HTTP date (a past date waits 0).
#[must_use]
pub fn parse_retry_after(value: &str, now: Millis) -> Option<Millis> {
    let value = value.trim();
    if let Ok(secs) = value.parse::<u64>() {
        return Some(Millis::new(secs.saturating_mul(1000)));
    }
    parse_http_date(value).map(|at| Millis::new(at.get().saturating_sub(now.get())))
}

/// The exponential backoff before retry number `retry` (1 for the first): `first · 2^(retry-1)`
/// capped at `max`, plus a jitter of up to `jitter_percent` of it from `rng`.
#[must_use]
pub fn backoff(policy: &RetryPolicy, retry: u32, rng: &impl Rng) -> Millis {
    let doubled = policy.first_backoff.get().saturating_mul(
        1u64.checked_shl(retry.saturating_sub(1))
            .unwrap_or(u64::MAX),
    );
    let base = doubled.min(policy.max_backoff.get());
    let jitter_bound = base.saturating_mul(policy.jitter_percent) / 100;
    Millis::new(base.saturating_add(rng.below(jitter_bound.saturating_add(1))))
}

/// The wait before retry number `retry` after an answer judged `verdict`: its `Retry-After`
/// when it named one (uncapped, spec §8); else the backoff, raised to the primary limit's reset
/// or to the least secondary-limit wait.
#[must_use]
pub fn wait_before_retry(
    policy: &RetryPolicy,
    retry: u32,
    verdict: Option<&Verdict>,
    now: Millis,
    rng: &impl Rng,
) -> Millis {
    if let Some(after) = verdict.and_then(|v| v.retry_after) {
        return after;
    }
    let backoff = backoff(policy, retry, rng);
    let floor = match verdict.and_then(|v| v.rate_limit) {
        Some(RateLimit {
            kind: RateLimitKind::Secondary,
            ..
        }) => policy.secondary_rate_limit_wait.get(),
        Some(RateLimit {
            kind: RateLimitKind::Primary,
            reset_at: Some(reset),
        }) => reset.get().saturating_sub(now.get()),
        _ => 0,
    };
    Millis::new(backoff.get().max(floor))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FixedRng(u64);

    impl Rng for FixedRng {
        fn below(&self, bound: u64) -> u64 {
            self.0.min(bound.saturating_sub(1))
        }
    }

    fn answer(status: u16, headers: &[(&str, &str)], body: &str) -> HttpResponse {
        HttpResponse {
            status,
            headers: headers
                .iter()
                .map(|(n, v)| ((*n).to_owned(), (*v).to_owned()))
                .collect(),
            body: body.as_bytes().to_vec(),
        }
    }

    const NOW: Millis = Millis::new(1_794_312_000_000);

    #[test]
    fn should_classify_each_answer_when_the_table_is_applied() {
        // given: (answer, phase) -> (class, retryable, retry_after ms, rate-limit kind)
        use Phase::{AdminExchange as X, Ordinary as O};
        use RateLimitKind::{Primary, Secondary, Unspecified};
        use StatusClass::{ClientError, RateLimited, ServerError, Success, Unexpected};
        let secondary = r#"{"message":"You have exceeded a secondary rate limit."}"#;
        let table = [
            (answer(200, &[], ""), O, (Success, false, None, None)),
            (answer(201, &[], ""), X, (Success, false, None, None)),
            (answer(302, &[], ""), O, (Unexpected, false, None, None)),
            (answer(400, &[], ""), X, (ClientError, false, None, None)),
            (answer(401, &[], ""), O, (ClientError, false, None, None)),
            (answer(401, &[], ""), X, (ClientError, true, None, None)),
            (answer(403, &[], ""), O, (ClientError, false, None, None)),
            (answer(403, &[], ""), X, (ClientError, true, None, None)),
            (answer(404, &[], ""), X, (ClientError, false, None, None)),
            (answer(409, &[], ""), O, (ClientError, false, None, None)),
            (answer(422, &[], ""), O, (ClientError, false, None, None)),
            (
                answer(429, &[], ""),
                O,
                (RateLimited, true, None, Some(Unspecified)),
            ),
            (
                answer(429, &[("retry-after", "7")], ""),
                O,
                (RateLimited, true, Some(7_000), Some(Unspecified)),
            ),
            (
                answer(429, &[("retry-after", "Tue, 10 Nov 2026 12:00:30 GMT")], ""),
                O,
                (RateLimited, true, Some(30_000), Some(Unspecified)),
            ),
            (
                answer(403, &[("retry-after", "5")], ""),
                O,
                (RateLimited, false, Some(5_000), Some(Unspecified)),
            ),
            (
                answer(403, &[("retry-after", "5")], ""),
                X,
                (RateLimited, true, Some(5_000), Some(Unspecified)),
            ),
            (
                answer(403, &[("x-ratelimit-remaining", "0")], ""),
                O,
                (RateLimited, false, None, Some(Primary)),
            ),
            (
                answer(403, &[("x-ratelimit-remaining", "12")], ""),
                O,
                (ClientError, false, None, None),
            ),
            (
                answer(403, &[], secondary),
                O,
                (RateLimited, false, None, Some(Secondary)),
            ),
            (
                answer(403, &[("x-ratelimit-remaining", "0")], secondary),
                X,
                (RateLimited, true, None, Some(Secondary)),
            ),
            (answer(500, &[], ""), O, (ServerError, true, None, None)),
            (
                answer(500, &[("retry-after", "9")], ""),
                O,
                (ServerError, true, None, None),
            ),
            (answer(501, &[], ""), X, (ServerError, false, None, None)),
            (answer(502, &[], ""), O, (ServerError, true, None, None)),
            (
                answer(503, &[("retry-after", "11")], ""),
                O,
                (ServerError, true, Some(11_000), None),
            ),
            (answer(504, &[], ""), O, (ServerError, true, None, None)),
            (answer(599, &[], ""), O, (ServerError, true, None, None)),
            (answer(101, &[], ""), O, (Unexpected, false, None, None)),
        ];

        // when
        let got: Vec<_> = table
            .iter()
            .map(|(response, phase, _)| {
                let v = classify_response(response, *phase, NOW);
                (
                    v.status_class,
                    v.retryable,
                    v.retry_after.map(Millis::get),
                    v.rate_limit.map(|r| r.kind),
                )
            })
            .collect();

        // then
        let expected: Vec<_> = table.iter().map(|(_, _, e)| *e).collect();
        assert_eq!(got, expected);
    }

    #[test]
    fn should_retry_transport_errors_when_they_are_not_certificates() {
        // given
        let errors = [
            TransportError::Connect,
            TransportError::Io,
            TransportError::Certificate,
            TransportError::BodyTooLarge,
            TransportError::InvalidRequest,
        ];

        // when
        let got: Vec<bool> = errors.iter().map(|e| transport_retryable(*e)).collect();

        // then
        assert_eq!(got, [true, true, false, false, false]);
    }

    #[test]
    fn should_double_to_the_cap_with_bounded_jitter_when_backing_off() {
        // given
        let policy = DEFAULT_RETRY_POLICY;
        let none = FixedRng(0);
        let most = FixedRng(u64::MAX);

        // when
        let low: Vec<u64> = (1..=7).map(|r| backoff(&policy, r, &none).get()).collect();
        let high: Vec<u64> = (1..=7).map(|r| backoff(&policy, r, &most).get()).collect();
        let far = backoff(&policy, 200, &none).get();

        // then
        assert_eq!(low, [1_000, 2_000, 4_000, 8_000, 16_000, 30_000, 30_000]);
        assert_eq!(high, [1_500, 3_000, 6_000, 12_000, 24_000, 45_000, 45_000]);
        assert_eq!(far, 30_000);
    }

    #[test]
    fn should_wait_for_the_limit_when_a_rate_limit_names_no_retry_after() {
        // given
        let policy = DEFAULT_RETRY_POLICY;
        let rng = FixedRng(0);
        let reset = (NOW.get() / 1000 + 90).to_string();
        let primary = classify_response(
            &answer(
                403,
                &[
                    ("x-ratelimit-remaining", "0"),
                    ("x-ratelimit-reset", &reset),
                ],
                "",
            ),
            Phase::AdminExchange,
            NOW,
        );
        let secondary = classify_response(
            &answer(403, &[], "secondary rate limit"),
            Phase::AdminExchange,
            NOW,
        );
        let asked = classify_response(
            &answer(429, &[("retry-after", "120")], ""),
            Phase::Ordinary,
            NOW,
        );
        let unlimited = classify_response(&answer(500, &[], ""), Phase::Ordinary, NOW);

        // when
        let waits: Vec<u64> = [
            Some(&primary),
            Some(&secondary),
            Some(&asked),
            Some(&unlimited),
            None,
        ]
        .iter()
        .map(|v| wait_before_retry(&policy, 1, *v, NOW, &rng).get())
        .collect();

        // then
        assert_eq!(waits, [90_000, 60_000, 120_000, 1_000, 1_000]);
    }

    #[test]
    fn should_read_seconds_or_a_date_when_parsing_retry_after() {
        // given
        let values = [
            "0",
            " 15 ",
            "Tue, 10 Nov 2026 12:01:00 GMT",
            "Tue, 10 Nov 2026 11:59:00 GMT",
            "-3",
            "later",
        ];

        // when
        let got: Vec<_> = values
            .iter()
            .map(|v| parse_retry_after(v, NOW).map(Millis::get))
            .collect();

        // then
        assert_eq!(
            got,
            [Some(0), Some(15_000), Some(60_000), Some(0), None, None]
        );
    }
}
