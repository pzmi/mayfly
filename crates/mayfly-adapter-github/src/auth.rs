//! The auth chain of spec §1: App JWT → installation token → registration token → admin token
//! and Actions service URL, with the admin token refreshed lazily and once for all callers.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use mayfly_control::secrets::SecretProvider;
use mayfly_control::store::{Clock, Rng};
use mayfly_domain::{Deadline, Millis, Secret};
use serde::Deserialize;
use tokio::sync::Mutex;
use url::Url;

use crate::client::ApiClient;
use crate::config::{AppConfig, ConfigError, credential_base_problem};
use crate::error::{ErrorKind, GitHubError, redacted_endpoint};
use crate::http::{HttpRequest, HttpResponse, Method, Transport};
use crate::jwt::{AppKey, app_jwt, token_expiry};
use crate::metrics::{Operation, RequestMetrics};
use crate::permissions::GrantedPermissions;
use crate::retry::Phase;
use crate::timefmt::parse_rfc3339;

/// The admin token is replaced when it expires within this margin (spec §1 "Refresh").
pub const REFRESH_MARGIN: Millis = Millis::new(60_000);

/// GitHub's REST media type and API version [docs].
const REST_HEADERS: [(&str, &str); 2] = [
    ("accept", "application/vnd.github+json"),
    ("x-github-api-version", "2022-11-28"),
];

/// An installation token (spec §1 step 2) and what its answer says about the installation.
#[derive(Debug)]
pub struct InstallationToken {
    /// The token.
    pub token: Secret<String>,
    /// When it expires; `None` when the answer's `expires_at` did not parse (it is not cached,
    /// so nothing depends on it).
    pub expires_at: Option<Millis>,
    /// The permissions the installation grants (ADR 0045).
    pub permissions: GrantedPermissions,
}

/// The admin connection to the Actions service (spec §1 step 4): every Actions call (GH-03)
/// carries its token as `Bearer`.
///
/// The token's lifetime is [open: HW-02] (spec §12 item 5), so nothing assumes one: the
/// refresh reads only its `exp` claim, a token without `exp` is refused, and so is a token that
/// arrives already inside the refresh margin, which would otherwise start a refresh on every
/// call (pinned by `tests/client/auth.rs::should_refuse_the_admin_token_when_it_has_no_expiry`,
/// `::should_refuse_the_admin_token_when_it_expires_within_the_margin` and
/// `::should_refresh_only_within_the_margin_when_the_admin_token_is_short_lived`).
///
/// The service URL passed the same rule as the configured bases
/// ([`crate::credential_base_problem`]) before the connection was made. Its `Debug` prints the
/// URL without query and never the token.
pub struct AdminConnection {
    /// The Actions service's base URL, opaque to mayfly.
    pub service_url: Url,
    /// The admin token.
    pub token: Secret<String>,
    /// Its `exp` claim, a Unix-epoch instant.
    pub expires_at: Millis,
}

impl fmt::Debug for AdminConnection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AdminConnection")
            .field("service_url", &redacted_endpoint(&self.service_url))
            .field("token", &self.token)
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

impl AdminConnection {
    /// Whether it is still good at `now`: it expires more than [`REFRESH_MARGIN`] later.
    #[must_use]
    pub const fn fresh_at(&self, now: Millis) -> bool {
        self.expires_at.get() > now.get().saturating_add(REFRESH_MARGIN.get())
    }
}

/// The GitHub App's credentials for one installation.
///
/// The App key is asked of its [`SecretProvider`] on every refresh and dropped after signing,
/// so a rotated key needs no restart and mayfly keeps no reference to a key between refreshes
/// (ADR 0033; pinned by
/// `tests/client/auth.rs::should_ask_the_provider_again_when_the_admin_token_is_refreshed`).
/// Dropped is not always wiped: the key's DER buffer is zeroized (`jwt.rs`) and `aws-lc` frees
/// its parsed key with a cleansing free, but the PEM text arrives in a `Secret<String>`, which
/// frees without wiping (its documented limitation), so a crash dump could hold it.
pub struct AppAuth<T, C, R, P> {
    config: AppConfig,
    client: ApiClient<T, C, R>,
    secrets: P,
    admin: Mutex<AdminSlot>,
    /// Refreshes finished, successful or not; written under `admin`'s lock, read before it so
    /// a caller can tell that a refresh ended while it waited.
    refreshes: AtomicU64,
}

/// The admin connection and how the last refresh ended.
#[derive(Default)]
struct AdminSlot {
    current: Option<Arc<AdminConnection>>,
    last_failure: Option<GitHubError>,
}

impl<T, C, R, P> fmt::Debug for AppAuth<T, C, R, P> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let admin = match self.admin.try_lock() {
            Ok(slot) => match &slot.current {
                Some(c) => format!("{c:?}"),
                None => "none".to_owned(),
            },
            Err(_) => "<refreshing>".to_owned(),
        };
        f.debug_struct("AppAuth")
            .field("api_base", &redacted_endpoint(&self.config.api_base))
            .field("installation_id", &self.config.installation_id)
            .field("app_key", &self.config.app_key)
            .field("admin", &admin)
            .finish_non_exhaustive()
    }
}

#[derive(Deserialize)]
struct TokenAnswer {
    token: Option<String>,
    expires_at: Option<String>,
    #[serde(default)]
    permissions: BTreeMap<String, String>,
}

#[derive(Deserialize)]
struct AdminAnswer {
    url: Option<String>,
    token: Option<String>,
}

impl<T: Transport, C: Clock, R: Rng, P: SecretProvider> AppAuth<T, C, R, P> {
    /// The chain for `config`, after checking it.
    ///
    /// # Errors
    /// [`ConfigError`] when the configuration is unusable.
    pub fn new(
        config: AppConfig,
        client: ApiClient<T, C, R>,
        secrets: P,
    ) -> Result<Self, ConfigError> {
        config.validate()?;
        Ok(Self {
            config,
            client,
            secrets,
            admin: Mutex::new(AdminSlot::default()),
            refreshes: AtomicU64::new(0),
        })
    }

    /// The client every call of this installation goes through (the Scale Set client's too).
    #[must_use]
    pub const fn client(&self) -> &ApiClient<T, C, R> {
        &self.client
    }

    /// The request counters (`mayfly_github_requests_total`).
    #[must_use]
    pub fn metrics(&self) -> &Arc<RequestMetrics> {
        self.client.metrics()
    }

    /// The admin connection, refreshed first when it expires within [`REFRESH_MARGIN`].
    ///
    /// One refresh runs at a time, and callers that waited on it share its outcome, success
    /// or failure: N callers during a refresh cause one chain of requests, never N in turn
    /// (pinned by `tests/client/auth.rs::should_refresh_once_when_callers_race` and
    /// `::should_share_a_failed_refresh_when_callers_waited_on_it`). A caller that arrives after
    /// a refresh ended starts its own. The whole chain runs under one deadline
    /// ([`crate::CallPolicy::deadline`]), so no caller waits longer than that for the lock's
    /// holder. The lock is Tokio's, held across the refresh's awaits on purpose; no std lock is.
    ///
    /// # Errors
    /// The first step of the chain that failed.
    pub async fn admin_connection(&self) -> Result<Arc<AdminConnection>, GitHubError> {
        let seen = self.refreshes.load(Ordering::Acquire);
        let mut slot = self.admin.lock().await;
        if let Some(current) = slot.current.as_ref()
            && current.fresh_at(self.client.clock().now())
        {
            return Ok(Arc::clone(current));
        }
        if self.refreshes.load(Ordering::Acquire) != seen
            && let Some(failure) = &slot.last_failure
        {
            return Err(failure.clone());
        }
        let outcome = self.exchange().await.map(Arc::new);
        match &outcome {
            Ok(fresh) => {
                slot.current = Some(Arc::clone(fresh));
                slot.last_failure = None;
            }
            Err(failure) => slot.last_failure = Some(failure.clone()),
        }
        self.refreshes.fetch_add(1, Ordering::Release);
        outcome
    }

    /// Drops `stale` so the next [`Self::admin_connection`] refreshes: for a caller whose
    /// request the service refused with 401 before the token's expiry. A connection another
    /// caller already replaced is left alone, so racing callers refresh once.
    pub async fn invalidate(&self, stale: &Arc<AdminConnection>) {
        let mut slot = self.admin.lock().await;
        if slot.current.as_ref().is_some_and(|c| Arc::ptr_eq(c, stale)) {
            slot.current = None;
        }
    }

    /// A fresh installation token (spec §1 step 2), never cached; its answer carries the
    /// installation's permissions, which [`crate::check_permissions`] compares with the
    /// published list (ADR 0045).
    ///
    /// # Errors
    /// The key's resolution, the JWT, or the call.
    pub async fn installation_token(&self) -> Result<InstallationToken, GitHubError> {
        self.installation_token_until(self.client.deadline()).await
    }

    async fn installation_token_until(
        &self,
        deadline: Deadline,
    ) -> Result<InstallationToken, GitHubError> {
        let op = Operation::InstallationToken;
        let pem = self
            .secrets
            .resolve(&self.config.app_key)
            .await
            .map_err(|e| fail(op, ErrorKind::Secret(e)))?;
        let key = AppKey::from_pem(&pem).map_err(|e| fail(op, ErrorKind::Jwt(e)))?;
        drop(pem);
        let jwt = app_jwt(&key, &self.config.client_id, self.client.clock().now())
            .map_err(|e| fail(op, ErrorKind::Jwt(e)))?;
        drop(key);
        let url = self.config.installation_token_url();
        let answer = self
            .client
            .call_until(op, Phase::Ordinary, deadline, || {
                rest(&url, format!("Bearer {}", jwt.expose()))
            })
            .await?;
        let parsed: TokenAnswer = decode(op, &url, &answer)?;
        Ok(InstallationToken {
            token: required(op, &url, parsed.token, "token")?,
            expires_at: parsed.expires_at.as_deref().and_then(parse_rfc3339),
            permissions: GrantedPermissions(parsed.permissions),
        })
    }

    /// The installation's granted permissions (ADR 0045), read from a fresh installation
    /// token's answer; the token itself is dropped at once.
    ///
    /// # Errors
    /// As [`Self::installation_token`].
    pub async fn granted_permissions(&self) -> Result<GrantedPermissions, GitHubError> {
        Ok(self.installation_token().await?.permissions)
    }

    /// Spec §1 step 3: used only as the input of the exchange, never to register a runner
    /// (ADR 0028, ADR 0048).
    async fn registration_token(
        &self,
        installation: &Secret<String>,
        deadline: Deadline,
    ) -> Result<Secret<String>, GitHubError> {
        let op = Operation::RegistrationToken;
        let url = self.config.registration_token_url();
        let answer = self
            .client
            .call_until(op, Phase::Ordinary, deadline, || {
                rest(&url, format!("Bearer {}", installation.expose()))
            })
            .await?;
        let parsed: TokenAnswer = decode(op, &url, &answer)?;
        required(op, &url, parsed.token, "token")
    }

    /// Steps 1-4 of spec §1, under one deadline.
    async fn exchange(&self) -> Result<AdminConnection, GitHubError> {
        let deadline = self.client.deadline();
        let installation = self.installation_token_until(deadline).await?;
        let registration = self
            .registration_token(&installation.token, deadline)
            .await?;
        drop(installation);
        let op = Operation::AdminExchange;
        let url = self.config.runner_registration_url();
        let body = serde_json::json!({
            "url": self.config.config_url().as_str(),
            "runner_event": "register",
        })
        .to_string()
        .into_bytes();
        let answer = self
            .client
            .call_until(op, Phase::AdminExchange, deadline, || HttpRequest {
                method: Method::Post,
                url: url.clone(),
                authorization: Some(Secret::new(format!("RemoteAuth {}", registration.expose()))),
                headers: vec![("content-type", "application/json".to_owned())],
                body: Some(body.clone()),
            })
            .await?;
        let parsed: AdminAnswer = decode(op, &url, &answer)?;
        let service_url = parsed
            .url
            .as_deref()
            .and_then(|u| Url::parse(u).ok())
            .filter(|u| credential_base_problem(u).is_none())
            .ok_or_else(|| decode_error(op, &url, "url"))?;
        let token = required(op, &url, parsed.token, "token")?;
        let expires_at = token_expiry(&token).map_err(|e| fail(op, ErrorKind::Jwt(e)))?;
        let connection = AdminConnection {
            service_url,
            token,
            expires_at,
        };
        let now = self.client.clock().now();
        if !connection.fresh_at(now) {
            return Err(fail(
                op,
                ErrorKind::AdminTokenTooShortLived {
                    remaining: Millis::new(expires_at.get().saturating_sub(now.get())),
                },
            ));
        }
        Ok(connection)
    }
}

fn rest(url: &Url, authorization: String) -> HttpRequest {
    HttpRequest {
        method: Method::Post,
        url: url.clone(),
        authorization: Some(Secret::new(authorization)),
        headers: REST_HEADERS
            .iter()
            .map(|(n, v)| (*n, (*v).to_owned()))
            .collect(),
        body: None,
    }
}

fn decode<A: for<'de> Deserialize<'de>>(
    op: Operation,
    url: &Url,
    answer: &HttpResponse,
) -> Result<A, GitHubError> {
    serde_json::from_slice(answer.body_without_bom()).map_err(|_| decode_error(op, url, "body"))
}

fn required(
    op: Operation,
    url: &Url,
    value: Option<String>,
    field: &'static str,
) -> Result<Secret<String>, GitHubError> {
    value
        .filter(|v| !v.is_empty())
        .map(Secret::new)
        .ok_or_else(|| decode_error(op, url, field))
}

fn decode_error(op: Operation, url: &Url, field: &'static str) -> GitHubError {
    fail(
        op,
        ErrorKind::Decode {
            endpoint: redacted_endpoint(url),
            field,
        },
    )
}

const fn fail(op: Operation, kind: ErrorKind) -> GitHubError {
    GitHubError { op, kind }
}
