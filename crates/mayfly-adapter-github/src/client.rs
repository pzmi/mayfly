//! One GitHub call: attempts under a deadline, retried by the policy of spec §8, counted.

use std::sync::Arc;
use std::time::Duration;

use mayfly_control::store::{Clock, Rng};
use mayfly_domain::{Deadline, Millis};

use crate::error::{ErrorKind, GitHubError, redacted_endpoint};
use crate::http::{HttpRequest, HttpResponse, Transport};
use crate::metrics::{Operation, RequestMetrics, StatusClass};
use crate::retry::{
    DEFAULT_RETRY_POLICY, Phase, RetryPolicy, Verdict, classify_response, transport_retryable,
    wait_before_retry,
};

/// How long calls may take, as plain values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CallPolicy {
    /// Retries and backoff (spec §8).
    pub retry: RetryPolicy,
    /// The longest one attempt may take.
    pub attempt_timeout: Millis,
    /// The whole call's budget, retries and waits included: a wait that would outlast it is not
    /// taken (spec §8).
    pub deadline: Millis,
}

/// Spec §8's retries; 30 s an attempt; two minutes a call, enough for the four retries' backoff
/// (1 + 2 + 4 + 8 s, plus jitter) and a short `Retry-After`.
pub const DEFAULT_CALL_POLICY: CallPolicy = CallPolicy {
    retry: DEFAULT_RETRY_POLICY,
    attempt_timeout: Millis::new(30_000),
    deadline: Millis::new(120_000),
};

/// Runs calls over a [`Transport`], reading time from the [`Clock`] port and jitter from the
/// [`Rng`] port (ADR 0020, ver §3.3): waits go through `Clock::sleep`, so a fake clock makes a
/// backoff observable without waiting; an attempt's timeout runs on the Tokio timer, which a
/// paused test clock fires at once.
#[derive(Debug)]
pub struct ApiClient<T, C, R> {
    transport: T,
    clock: C,
    rng: R,
    policy: CallPolicy,
    metrics: Arc<RequestMetrics>,
}

impl<T: Transport, C: Clock, R: Rng> ApiClient<T, C, R> {
    /// A client with its own counters.
    pub fn new(transport: T, clock: C, rng: R, policy: CallPolicy) -> Self {
        Self {
            transport,
            clock,
            rng,
            policy,
            metrics: Arc::default(),
        }
    }

    /// The counters of every call made so far.
    #[must_use]
    pub fn metrics(&self) -> &Arc<RequestMetrics> {
        &self.metrics
    }

    /// The clock calls read.
    pub const fn clock(&self) -> &C {
        &self.clock
    }

    /// Sends the request `build` makes (once per attempt: a credential is never cloned) until
    /// a 2xx answer, a final refusal, the retries' end or the deadline. Pinned by
    /// `tests/client/retry.rs`.
    ///
    /// # Errors
    /// [`ErrorKind::Status`], [`ErrorKind::Transport`] or [`ErrorKind::DeadlineExceeded`].
    pub async fn call(
        &self,
        op: Operation,
        phase: Phase,
        build: impl Fn() -> HttpRequest,
    ) -> Result<HttpResponse, GitHubError> {
        self.call_until(op, phase, self.deadline(), build).await
    }

    /// A deadline [`CallPolicy::deadline`] from now: one call's budget, or one budget shared
    /// by several calls that form one operation (the admin refresh's chain).
    #[must_use]
    pub fn deadline(&self) -> Deadline {
        Deadline::after(self.clock.now(), self.policy.deadline)
    }

    /// As [`Self::call`], under a `deadline` the caller chose (shared with other calls).
    ///
    /// # Errors
    /// As [`Self::call`].
    pub async fn call_until(
        &self,
        op: Operation,
        phase: Phase,
        deadline: Deadline,
        build: impl Fn() -> HttpRequest,
    ) -> Result<HttpResponse, GitHubError> {
        let mut attempts = 0;
        loop {
            let request = build();
            let endpoint = redacted_endpoint(&request.url);
            let now = self.clock.now();
            if deadline.is_expired(now) {
                return Err(fail(op, ErrorKind::DeadlineExceeded { endpoint, attempts }));
            }
            let budget = deadline.remaining(now).min(self.policy.attempt_timeout);
            attempts += 1;
            let sent = tokio::time::timeout(
                Duration::from_millis(budget.get()),
                self.transport.send(request),
            )
            .await;
            let (verdict, failure) = match sent {
                Err(_elapsed) => {
                    self.metrics.record(op, StatusClass::Timeout);
                    (None, ErrorKind::DeadlineExceeded { endpoint, attempts })
                }
                Ok(Err(error)) => {
                    self.metrics.record(op, StatusClass::Transport);
                    if !transport_retryable(error) {
                        return Err(fail(op, transport(endpoint, error, attempts)));
                    }
                    (None, transport(endpoint, error, attempts))
                }
                Ok(Ok(response)) => {
                    let verdict = classify_response(&response, phase, self.clock.now());
                    self.metrics.record(op, verdict.status_class);
                    if verdict.status_class == StatusClass::Success {
                        return Ok(response);
                    }
                    let failure = ErrorKind::Status {
                        endpoint,
                        status: response.status,
                        rate_limit: verdict.rate_limit,
                        attempts,
                    };
                    if !verdict.retryable {
                        return Err(fail(op, failure));
                    }
                    (Some(verdict), failure)
                }
            };
            if attempts > self.policy.retry.max_retries {
                return Err(fail(op, failure));
            }
            let now = self.clock.now();
            let wait = wait_before_retry(
                &self.policy.retry,
                attempts,
                verdict.as_ref(),
                now,
                &self.rng,
            );
            if now.get().saturating_add(wait.get()) > deadline.instant().get() {
                return Err(fail(op, failure));
            }
            log_retry(op, verdict.as_ref(), attempts, wait);
            self.clock.sleep(wait).await;
        }
    }
}

fn log_retry(op: Operation, verdict: Option<&Verdict>, attempt: u32, wait: Millis) {
    let status_class = verdict.map_or("transport_or_timeout", |v| v.status_class.as_str());
    tracing::warn!(
        op = op.as_str(),
        status_class,
        attempt,
        wait_ms = wait.get(),
        "github_request_retried"
    );
}

const fn fail(op: Operation, kind: ErrorKind) -> GitHubError {
    GitHubError { op, kind }
}

const fn transport(
    endpoint: String,
    error: crate::http::TransportError,
    attempts: u32,
) -> ErrorKind {
    ErrorKind::Transport {
        endpoint,
        error,
        attempts,
    }
}
