//! The client's errors. None carries a token, a key, a response body or a URL's query or user
//! info (spec §2): an operator pastes errors into bug reports. Pinned by
//! `tests/client/auth.rs::should_keep_credentials_out_of_debug_and_errors_when_the_chain_runs_and_fails`.

use std::fmt;

use mayfly_control::secrets::SecretError;
use mayfly_domain::Millis;
use url::Url;

use crate::config::ConfigError;
use crate::http::TransportError;
use crate::jwt::JwtError;
use crate::metrics::Operation;
use crate::retry::RateLimit;

/// A failed GitHub call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitHubError {
    /// What the call was for.
    pub op: Operation,
    /// Why it failed.
    pub kind: ErrorKind,
}

/// Why a call failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ErrorKind {
    /// The configuration is unusable; no request was made.
    Config(ConfigError),
    /// The App key's reference did not resolve.
    Secret(SecretError),
    /// The App key or a token could not be used.
    Jwt(JwtError),
    /// The last attempt got an answer that is not a success.
    Status {
        /// Where, as [`redacted_endpoint`] writes it.
        endpoint: String,
        /// The status code.
        status: u16,
        /// Set when it was a rate limit (spec §8).
        rate_limit: Option<RateLimit>,
        /// Attempts made.
        attempts: u32,
    },
    /// The last attempt got no answer.
    Transport {
        /// Where.
        endpoint: String,
        /// What broke.
        error: TransportError,
        /// Attempts made.
        attempts: u32,
    },
    /// The call's deadline passed, during an attempt or before a wait it would have outlasted.
    DeadlineExceeded {
        /// Where.
        endpoint: String,
        /// Attempts made.
        attempts: u32,
    },
    /// The exchange answered an admin token that expires within [`crate::REFRESH_MARGIN`]:
    /// using it would start a refresh on every call.
    AdminTokenTooShortLived {
        /// How long it had left when it arrived.
        remaining: Millis,
    },
    /// A success answer without a field the protocol requires, or not the JSON expected.
    Decode {
        /// Where.
        endpoint: String,
        /// The field, or `body` when the body did not parse.
        field: &'static str,
    },
}

impl fmt::Display for GitHubError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "github {} failed: ", self.op.as_str())?;
        match &self.kind {
            ErrorKind::Config(e) => write!(f, "{e}"),
            ErrorKind::Secret(e) => write!(f, "the App key: {e}"),
            ErrorKind::Jwt(e) => write!(f, "{e}"),
            ErrorKind::Status {
                endpoint,
                status,
                rate_limit,
                attempts,
            } => {
                write!(f, "{endpoint} answered {status}")?;
                if rate_limit.is_some() {
                    f.write_str(" (rate limited)")?;
                }
                write!(f, " after {attempts} attempt(s)")
            }
            ErrorKind::Transport {
                endpoint,
                error,
                attempts,
            } => write!(f, "{endpoint}: {error} after {attempts} attempt(s)"),
            ErrorKind::DeadlineExceeded { endpoint, attempts } => {
                write!(
                    f,
                    "{endpoint}: deadline exceeded after {attempts} attempt(s)"
                )
            }
            ErrorKind::AdminTokenTooShortLived { remaining } => write!(
                f,
                "the admin token expires in {} ms, within the {} s refresh margin",
                remaining.get(),
                crate::REFRESH_MARGIN.get() / 1000
            ),
            ErrorKind::Decode { endpoint, field } => {
                write!(f, "{endpoint}: the answer lacks a usable `{field}`")
            }
        }
    }
}

impl std::error::Error for GitHubError {}

/// `url` as error text: scheme, host, port and path only, never the query (a token or id may
/// sit there), the fragment or user info. Pinned by
/// `error::tests::should_drop_query_and_user_info_when_writing_an_endpoint`.
#[must_use]
pub fn redacted_endpoint(url: &Url) -> String {
    let host = url.host_str().unwrap_or("");
    match url.port() {
        Some(port) => format!("{}://{host}:{port}{}", url.scheme(), url.path()),
        None => format!("{}://{host}{}", url.scheme(), url.path()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::retry::RateLimitKind;

    #[test]
    fn should_drop_query_and_user_info_when_writing_an_endpoint() {
        // given
        let urls = [
            "https://user:pw@pipelines.example/_apis/x?api-version=6.0-preview&token=abc#frag",
            "http://127.0.0.1:8080/app/installations/1/access_tokens",
        ];

        // when
        let written: Vec<String> = urls
            .iter()
            .map(|u| redacted_endpoint(&Url::parse(u).expect("url")))
            .collect();

        // then
        assert_eq!(
            written,
            [
                "https://pipelines.example/_apis/x",
                "http://127.0.0.1:8080/app/installations/1/access_tokens",
            ]
        );
    }

    #[test]
    fn should_name_the_operation_and_cause_when_displayed() {
        // given
        let endpoint = "https://api.github.com/actions/runner-registration".to_owned();
        let errors = [
            ErrorKind::Config(ConfigError::ClientId),
            ErrorKind::Jwt(JwtError::TokenWithoutExpiry),
            ErrorKind::Status {
                endpoint: endpoint.clone(),
                status: 429,
                rate_limit: Some(RateLimit {
                    kind: RateLimitKind::Unspecified,
                    reset_at: None,
                }),
                attempts: 5,
            },
            ErrorKind::Status {
                endpoint: endpoint.clone(),
                status: 404,
                rate_limit: None,
                attempts: 1,
            },
            ErrorKind::Transport {
                endpoint: endpoint.clone(),
                error: TransportError::Connect,
                attempts: 2,
            },
            ErrorKind::DeadlineExceeded {
                endpoint: endpoint.clone(),
                attempts: 3,
            },
            ErrorKind::Decode {
                endpoint,
                field: "token",
            },
            ErrorKind::AdminTokenTooShortLived {
                remaining: Millis::new(30_000),
            },
        ];

        // when
        let texts: Vec<String> = errors
            .into_iter()
            .map(|kind| {
                GitHubError {
                    op: Operation::AdminExchange,
                    kind,
                }
                .to_string()
            })
            .collect();

        // then
        assert_eq!(
            texts,
            [
                "github admin_exchange failed: github.client_id is empty",
                "github admin_exchange failed: the admin token carries no exp claim",
                "github admin_exchange failed: https://api.github.com/actions/runner-registration answered 429 (rate limited) after 5 attempt(s)",
                "github admin_exchange failed: https://api.github.com/actions/runner-registration answered 404 after 1 attempt(s)",
                "github admin_exchange failed: https://api.github.com/actions/runner-registration: no connection could be made after 2 attempt(s)",
                "github admin_exchange failed: https://api.github.com/actions/runner-registration: deadline exceeded after 3 attempt(s)",
                "github admin_exchange failed: https://api.github.com/actions/runner-registration: the answer lacks a usable `token`",
                "github admin_exchange failed: the admin token expires in 30000 ms, within the 60 s refresh margin",
            ]
        );
    }
}
