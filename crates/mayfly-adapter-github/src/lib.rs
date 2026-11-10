//! `mayfly-adapter-github` (control plane): Control-plane adapter: GitHub App JWT and
//! installation tokens, Scale Set client, JIT runner configs and classic runner registration.
//!
//! Our own client, written from `doc/github-scaleset.md` (ADR 0048). GH-02 brings the parts
//! every later call stands on:
//!
//! - [`AppAuth`]: the auth chain of spec §1, from the App key (asked of its `SecretProvider`
//!   on each refresh, ADR 0033) to the admin token and Actions service URL, refreshed lazily
//!   60 s before expiry and once for all concurrent callers.
//! - [`ApiClient`]: one call under a deadline, retried by spec §8's policy
//!   ([`DEFAULT_RETRY_POLICY`], plain values), counted in [`RequestMetrics`] with bounded labels.
//! - [`Transport`]: requests as data; [`ReqwestTransport`] is `reqwest` over `rustls` with
//!   `aws-lc-rs` and the platform verifier (ADR 0064).
//! - [`check_permissions`]: the installation's grants against the published list (ADR 0045).
//!
//! Every credential is a `Secret`; no `Debug`, `Display` or error carries a token, a key, a
//! response body or a URL's query (ADR 0014, spec §2). Nothing is sent anywhere the operator
//! did not configure (ADR 0036).

mod auth;
mod client;
mod config;
mod error;
mod http;
mod jwt;
mod metrics;
mod permissions;
mod retry;
mod timefmt;
mod transport;

pub use auth::{AdminConnection, AppAuth, InstallationToken, REFRESH_MARGIN};
pub use client::{ApiClient, CallPolicy, DEFAULT_CALL_POLICY};
pub use config::{AppConfig, BaseProblem, ConfigError, Scope, credential_base_problem};
pub use error::{ErrorKind, GitHubError, redacted_endpoint};
pub use http::{HttpRequest, HttpResponse, MAX_RESPONSE_BYTES, Method, Transport, TransportError};
pub use jwt::{AppKey, JWT_BACKDATE_SECS, JWT_LIFETIME_SECS, JwtError, app_jwt, token_expiry};
pub use metrics::{Operation, RequestCount, RequestMetrics, StatusClass};
pub use permissions::{
    Access, Excess, GrantedPermissions, PUBLISHED_PERMISSIONS, PermissionCheck,
    PublishedPermissions, RequiredPermission, check_permissions,
};
pub use retry::{
    DEFAULT_RETRY_POLICY, Phase, RateLimit, RateLimitKind, RetryPolicy, Verdict, backoff,
    classify_response, parse_retry_after, transport_retryable, wait_before_retry,
};
pub use timefmt::{parse_http_date, parse_rfc3339};
pub use transport::{CONNECT_TIMEOUT, ReqwestTransport, TlsSetupError, USER_AGENT};
