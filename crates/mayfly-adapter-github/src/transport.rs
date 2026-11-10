//! [`ReqwestTransport`]: the network side of the client, `reqwest` over `rustls` with the
//! `aws-lc-rs` provider and the platform's certificate verifier (ADR 0064). No OpenSSL, no
//! native-tls, no bundled root store: the operating system decides trust (on macOS the
//! Security framework, with Keychain and MDM trust settings, constraints and revocation), so an
//! operator's corporate CA for GHES works as it does for every other program on the host; a CA
//! the host does not trust can be added per client ([`ReqwestTransport::with_extra_roots`]).

use std::error::Error as _;
use std::sync::Arc;
use std::time::Duration;

use reqwest::header::{AUTHORIZATION, HeaderValue};
use rustls::ClientConfig;
use rustls_pki_types::CertificateDer;
use rustls_platform_verifier::Verifier;

use crate::http::{
    HttpRequest, HttpResponse, MAX_RESPONSE_BYTES, Method, Transport, TransportError,
};

/// How long a connection may take to open; the request's own deadline bounds the rest.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// The `User-Agent` every request carries (GitHub's REST API requires one [docs]).
pub const USER_AGENT: &str = concat!("mayfly/", env!("CARGO_PKG_VERSION"));

/// The TLS setup failed: the platform verifier or the client could not be built.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TlsSetupError;

impl std::fmt::Display for TlsSetupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the TLS client could not be set up from the system's trust store")
    }
}

impl std::error::Error for TlsSetupError {}

/// `reqwest` with redirects off (a redirect would carry a credential to a host the config did
/// not name), no proxy read from the environment (config, never the environment, says where
/// GitHub is), and a bounded answer body. Pinned by
/// `tests/client/socket.rs::should_not_follow_a_redirect_when_the_server_answers_one` and
/// `::should_refuse_the_answer_when_its_body_is_over_the_limit`.
#[derive(Debug, Clone)]
pub struct ReqwestTransport {
    client: reqwest::Client,
}

impl ReqwestTransport {
    /// A transport trusting what the operating system trusts.
    ///
    /// # Errors
    /// [`TlsSetupError`] when the verifier or the client cannot be built.
    pub fn new() -> Result<Self, TlsSetupError> {
        Self::with_extra_roots(Vec::new())
    }

    /// As [`Self::new`], also trusting `extra_roots`: a GHES private CA the host does not
    /// trust. Reading the CA bundle from config is RT-01's (a path, not a secret).
    ///
    /// # Errors
    /// [`TlsSetupError`] when the verifier or the client cannot be built.
    pub fn with_extra_roots(
        extra_roots: Vec<CertificateDer<'static>>,
    ) -> Result<Self, TlsSetupError> {
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let verifier = Verifier::new_with_extra_roots(extra_roots, Arc::clone(&provider))
            .map_err(|_| TlsSetupError)?;
        let tls = ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(|_| TlsSetupError)?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(verifier))
            .with_no_client_auth();
        let client = reqwest::Client::builder()
            .tls_backend_preconfigured(tls)
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .connect_timeout(CONNECT_TIMEOUT)
            .user_agent(USER_AGENT)
            .build()
            .map_err(|_| TlsSetupError)?;
        Ok(Self { client })
    }
}

impl Transport for ReqwestTransport {
    async fn send(&self, request: HttpRequest) -> Result<HttpResponse, TransportError> {
        let method = match request.method {
            Method::Get => reqwest::Method::GET,
            Method::Post => reqwest::Method::POST,
            Method::Patch => reqwest::Method::PATCH,
            Method::Delete => reqwest::Method::DELETE,
        };
        let mut builder = self.client.request(method, request.url);
        for (name, value) in request.headers {
            builder = builder.header(name, value);
        }
        if let Some(credential) = &request.authorization {
            let mut value = HeaderValue::from_str(credential.expose())
                .map_err(|_| TransportError::InvalidRequest)?;
            value.set_sensitive(true);
            builder = builder.header(AUTHORIZATION, value);
        }
        if let Some(body) = request.body {
            builder = builder.body(body);
        }
        let mut response = builder.send().await.map_err(|e| classify(&e))?;
        let status = response.status().as_u16();
        let headers = response
            .headers()
            .iter()
            .filter_map(|(n, v)| Some((n.as_str().to_owned(), v.to_str().ok()?.to_owned())))
            .collect();
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|e| classify(&e))? {
            if body.len() + chunk.len() > MAX_RESPONSE_BYTES {
                return Err(TransportError::BodyTooLarge);
            }
            body.extend_from_slice(&chunk);
        }
        Ok(HttpResponse {
            status,
            headers,
            body,
        })
    }
}

/// Maps a `reqwest` error to a kind, dropping its message (it names the URL). A refused
/// certificate is found by walking the source chain to `rustls`'s error.
fn classify(error: &reqwest::Error) -> TransportError {
    let mut source = error.source();
    while let Some(cause) = source {
        let tls = cause.downcast_ref::<rustls::Error>().or_else(|| {
            cause
                .downcast_ref::<std::io::Error>()
                .and_then(|io| io.get_ref())
                .and_then(|inner| inner.downcast_ref::<rustls::Error>())
        });
        if matches!(tls, Some(rustls::Error::InvalidCertificate(_))) {
            return TransportError::Certificate;
        }
        source = cause.source();
    }
    if error.is_builder() {
        TransportError::InvalidRequest
    } else if error.is_connect() {
        TransportError::Connect
    } else {
        TransportError::Io
    }
}
