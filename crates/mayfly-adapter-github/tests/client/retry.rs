//! Spec §8's policy applied to real calls: what is retried, how long it waits, and the
//! deadline every call carries. Waits go through the fake clock, which records them.

use std::sync::Mutex;

use mayfly_adapter_github::{
    CallPolicy, DEFAULT_CALL_POLICY, ErrorKind, HttpRequest, HttpResponse, Operation, Transport,
    TransportError,
};
use mayfly_domain::Millis;
use mayfly_fake_github::{Fault, Route};

use crate::support::{InProcess, harness, harness_with};

fn respond(status: u16, headers: &[(&str, &str)]) -> Fault {
    Fault::Respond {
        status,
        headers: headers
            .iter()
            .map(|(n, v)| ((*n).to_owned(), (*v).to_owned()))
            .collect(),
        body: r#"{"message":"synthetic"}"#.to_owned(),
    }
}

/// Fails its first sends with the queued transport errors, then sends through the fake.
#[derive(Debug)]
struct Flaky {
    inner: InProcess,
    failures: Mutex<Vec<TransportError>>,
}

impl Transport for Flaky {
    async fn send(&self, request: HttpRequest) -> Result<HttpResponse, TransportError> {
        let failure = self
            .failures
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop();
        match failure {
            Some(error) => Err(error),
            None => self.inner.send(request).await,
        }
    }
}

fn flaky(failures: Vec<TransportError>) -> impl FnOnce(InProcess) -> Flaky {
    move |inner| Flaky {
        inner,
        failures: Mutex::new(failures),
    }
}

#[tokio::test(start_paused = true)]
async fn should_wait_as_long_as_retry_after_says_when_rate_limited() {
    // given
    let h = harness(3600);
    h.fake.fail_next(
        Route::InstallationToken,
        respond(429, &[("retry-after", "7")]),
    );
    h.fake.fail_next(
        Route::RegistrationToken,
        respond(503, &[("retry-after", "Tue, 10 Nov 2026 12:00:20 GMT")]),
    );

    // when
    let got = h.auth.admin_connection().await;

    // then
    assert!(got.is_ok());
    assert_eq!(h.clock.sleeps(), [Millis::new(7_000), Millis::new(13_000)]);
}

#[tokio::test(start_paused = true)]
async fn should_retry_refusals_only_when_exchanging_for_the_admin_token() {
    // given
    let rest = harness(3600);
    rest.fake
        .fail_next(Route::InstallationToken, respond(401, &[]));
    let exchange = harness(3600);
    exchange
        .fake
        .fail_next(Route::RunnerRegistration, respond(401, &[]));
    exchange
        .fake
        .fail_next(Route::RunnerRegistration, respond(403, &[]));

    // when
    let refused = rest.auth.admin_connection().await;
    let retried = exchange.auth.admin_connection().await;

    // then
    let error = refused.expect_err("401 is final on REST");
    assert!(matches!(
        error.kind,
        ErrorKind::Status {
            status: 401,
            attempts: 1,
            ..
        }
    ));
    assert!(rest.clock.sleeps().is_empty());
    assert!(retried.is_ok());
    let sleeps = exchange.clock.sleeps();
    assert_eq!(sleeps.len(), 2);
    assert!((1_000..=1_500).contains(&sleeps[0].get()));
    assert!((2_000..=3_000).contains(&sleeps[1].get()));
}

#[tokio::test(start_paused = true)]
async fn should_give_up_at_once_when_a_rest_rate_limit_is_a_403() {
    // given
    let h = harness(3600);
    h.fake.fail_next(
        Route::RegistrationToken,
        respond(
            403,
            &[
                ("x-ratelimit-remaining", "0"),
                ("x-ratelimit-reset", "1794312060"),
            ],
        ),
    );

    // when
    let got = h.auth.admin_connection().await;

    // then
    let error = got.expect_err("rate limited");
    assert_eq!(error.op, Operation::RegistrationToken);
    assert!(matches!(
        error.kind,
        ErrorKind::Status {
            status: 403,
            rate_limit: Some(_),
            attempts: 1,
            ..
        }
    ));
    assert!(error.to_string().contains("rate limited"));
}

#[tokio::test(start_paused = true)]
async fn should_back_off_until_the_retries_end_when_the_service_keeps_failing() {
    // given
    let h = harness(3600);
    for _ in 0..5 {
        h.fake
            .fail_next(Route::InstallationToken, respond(500, &[]));
    }

    // when
    let got = h.auth.installation_token().await;

    // then
    let error = got.expect_err("out of retries");
    assert!(matches!(
        error.kind,
        ErrorKind::Status {
            status: 500,
            attempts: 5,
            ..
        }
    ));
    let sleeps: Vec<u64> = h.clock.sleeps().iter().map(|m| m.get()).collect();
    assert_eq!(sleeps.len(), 4);
    for (i, wait) in sleeps.iter().enumerate() {
        let base = 1_000 << i;
        assert!((base..=base + base / 2).contains(wait), "{sleeps:?}");
    }
}

#[tokio::test(start_paused = true)]
async fn should_not_retry_when_the_status_is_501() {
    // given
    let h = harness(3600);
    h.fake
        .fail_next(Route::InstallationToken, respond(501, &[]));

    // when
    let got = h.auth.installation_token().await;

    // then
    assert!(matches!(
        got.expect_err("final").kind,
        ErrorKind::Status {
            status: 501,
            attempts: 1,
            ..
        }
    ));
    assert!(h.clock.sleeps().is_empty());
}

#[tokio::test(start_paused = true)]
async fn should_not_take_a_wait_when_it_would_outlast_the_deadline() {
    // given: a two-minute call deadline and a five-minute Retry-After
    let h = harness(3600);
    h.fake.fail_next(
        Route::InstallationToken,
        respond(429, &[("retry-after", "300")]),
    );

    // when
    let got = h.auth.installation_token().await;

    // then
    assert!(matches!(
        got.expect_err("deadline").kind,
        ErrorKind::Status {
            status: 429,
            attempts: 1,
            ..
        }
    ));
    assert!(h.clock.sleeps().is_empty());
}

#[tokio::test(start_paused = true)]
async fn should_retry_after_a_timeout_when_an_attempt_stalls() {
    // given
    let h = harness(3600);
    h.fake.fail_next(Route::InstallationToken, Fault::Stall);

    // when
    let got = h.auth.installation_token().await;

    // then
    assert!(got.is_ok());
    let counts: Vec<(&str, u64)> = h
        .auth
        .metrics()
        .snapshot()
        .iter()
        .map(|c| (c.status_class.as_str(), c.count))
        .collect();
    assert_eq!(counts, [("2xx", 1), ("timeout", 1)]);
}

#[tokio::test(start_paused = true)]
async fn should_end_with_the_deadline_when_every_attempt_stalls() {
    // given: a call budget the backoff exhausts
    let policy = CallPolicy {
        deadline: Millis::new(5_000),
        ..DEFAULT_CALL_POLICY
    };
    let h = harness_with(3600, policy, |inner| inner);
    for _ in 0..5 {
        h.fake.fail_next(Route::InstallationToken, Fault::Stall);
    }

    // when
    let got = h.auth.installation_token().await;

    // then
    assert!(matches!(
        got.expect_err("deadline").kind,
        ErrorKind::DeadlineExceeded { attempts, .. } if attempts >= 2
    ));
    let slept: u64 = h.clock.sleeps().iter().map(|m| m.get()).sum();
    assert!(slept <= 5_000);
}

#[tokio::test(start_paused = true)]
async fn should_retry_transport_errors_when_they_are_not_certificates() {
    // given
    let recovers = harness_with(
        3600,
        DEFAULT_CALL_POLICY,
        flaky(vec![TransportError::Io, TransportError::Connect]),
    );
    let refused = harness_with(
        3600,
        DEFAULT_CALL_POLICY,
        flaky(vec![TransportError::Certificate]),
    );

    // when
    let recovered = recovers.auth.installation_token().await;
    let failed = refused.auth.installation_token().await;

    // then
    assert!(recovered.is_ok());
    assert_eq!(recovers.clock.sleeps().len(), 2);
    let error = failed.expect_err("certificate");
    assert!(matches!(
        error.kind,
        ErrorKind::Transport {
            error: TransportError::Certificate,
            attempts: 1,
            ..
        }
    ));
    assert!(error.to_string().contains("certificate"));
}

#[tokio::test(start_paused = true)]
async fn should_end_on_the_last_transport_error_when_retries_run_out() {
    // given
    let h = harness_with(
        3600,
        DEFAULT_CALL_POLICY,
        flaky(vec![TransportError::Io; 5]),
    );

    // when
    let got = h.auth.installation_token().await;

    // then
    assert!(matches!(
        got.expect_err("out of retries").kind,
        ErrorKind::Transport {
            error: TransportError::Io,
            attempts: 5,
            ..
        }
    ));
}
