//! The raw manifest of an image, fetched by digest independently of Tart (ADR 0087).
//!
//! Tart 2.28.0 never compares the manifest it is served with the digest it was asked for, and
//! re-serializes the copy it caches, so the image store fetches the manifest's exact bytes itself
//! and checks them against the digest before it trusts any layout. [`ManifestSource`] is the
//! seam: [`RegistryManifests`] over HTTPS in production; the tests' source reads the fake
//! registry.
//!
//! [`RegistryManifests`] speaks the OCI distribution API directly: `GET /v2/<name>/manifests/
//! <digest>`, and on a `401` with a `Bearer` challenge the anonymous token flow. HTTPS only, no
//! redirect followed, the whole fetch (token included) bounded by one deadline. The body is read
//! chunk by chunk and abandoned as soon as it passes [`MANIFEST_LIMIT`] (a stated
//! `Content-Length` above it is refused before any byte is read), so a hostile or broken registry
//! cannot make the daemon buffer more than the limit ([`read_capped`]).
//!
//! **The challenge is the registry's word, never trusted as a destination** ([`token_url`]): its
//! realm must be HTTPS on the registry's own host or a host the operator listed, never an IP
//! literal; its scope must be exactly `repository:<path>:pull`; and the token request resolves
//! its host only to public addresses ([`public`]): a name that resolves to a loopback,
//! link-local, private, shared or unique-local address is refused. The manifest request itself
//! goes to the configured registry, which may be private. Registry credentials are a broker
//! secret class (domain-model §9.3), wired when a private registry needs them; the anonymous
//! token is a [`Secret`], sent only as the bearer credential and never logged.

use std::future::Future;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;

use mayfly_domain::{ImageDigest, Secret};
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use reqwest::header::{ACCEPT, HeaderMap, WWW_AUTHENTICATE};
use reqwest::{StatusCode, Url};
use rustls::ClientConfig;
use rustls_platform_verifier::Verifier;

use crate::process::blocking::blocking;
use crate::verify::{MANIFEST_LIMIT, MANIFEST_V1, sha256_hex};

/// The most bytes of a token answer read.
const TOKEN_LIMIT: usize = 64 << 10;

/// How long one fetch (with its token round trip) may take.
const FETCH_DEADLINE: Duration = Duration::from_secs(60);

/// Why a manifest was not fetched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchError {
    /// The registry does not hold it.
    NotFound,
    /// The answer was larger than the limit; reading stopped there.
    TooLarge,
    /// The registry could not be reached, refused, or answered something else.
    Failed,
}

/// Where the image store reads a raw manifest from.
pub trait ManifestSource: Send + Sync {
    /// The manifest of `<repository>@<digest>`, exactly as the registry serves it. The caller
    /// checks its hash: a source may return bytes that do not match.
    ///
    /// # Errors
    /// [`FetchError`].
    fn fetch(
        &self,
        repository: &str,
        digest: &ImageDigest,
    ) -> impl Future<Output = Result<Vec<u8>, FetchError>> + Send;
}

/// Whether `raw` is the manifest `digest` names and within the size limit. Pinned by
/// `tests::should_accept_a_manifest_only_when_its_exact_bytes_hash_to_the_digest`.
#[must_use]
pub fn matches(raw: &[u8], digest: &ImageDigest) -> bool {
    raw.len() <= MANIFEST_LIMIT
        && digest.as_str().strip_prefix("sha256:") == Some(sha256_hex(raw).as_str())
}

/// A body read chunk by chunk.
pub trait Chunks {
    /// The next chunk, `None` at the end.
    fn next_chunk(&mut self) -> impl Future<Output = Result<Option<Vec<u8>>, FetchError>>;
}

impl Chunks for reqwest::Response {
    async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, FetchError> {
        self.chunk()
            .await
            .map(|chunk| chunk.map(|bytes| bytes.to_vec()))
            .map_err(|_| FetchError::Failed)
    }
}

/// Reads `body` until it ends, refusing [`FetchError::TooLarge`] as soon as more than `limit`
/// bytes arrived: nothing past the chunk that crossed it is requested. Pinned by
/// `tests::should_stop_reading_when_the_body_passes_the_limit`.
///
/// # Errors
/// [`FetchError::TooLarge`], or what the body failed with.
pub async fn read_capped<C: Chunks>(
    limit: usize,
    body_chunks: &mut C,
) -> Result<Vec<u8>, FetchError> {
    let mut body = Vec::new();
    while let Some(chunk) = body_chunks.next_chunk().await? {
        if body.len().saturating_add(chunk.len()) > limit {
            return Err(FetchError::TooLarge);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// A `Bearer` challenge's parameters (`realm`, `service`, `scope`); `None` for anything else.
/// Pinned by `tests::should_read_a_bearer_challenge_when_the_registry_asks_for_a_token`.
#[must_use]
pub fn bearer_challenge(header: &str) -> Option<(String, Vec<(String, String)>)> {
    let rest = header.strip_prefix("Bearer ")?;
    let mut realm = None;
    let mut params = Vec::new();
    for part in split_params(rest) {
        let (key, value) = part.split_once('=')?;
        let value = value.trim().trim_matches('"').to_owned();
        match key.trim() {
            "realm" => realm = Some(value),
            key @ ("service" | "scope") => params.push((key.to_owned(), value)),
            _ => {}
        }
    }
    Some((realm?, params))
}

/// Splits `a="x,y",b="z"` at the commas outside quotes.
fn split_params(text: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let (mut start, mut quoted) = (0, false);
    for (i, c) in text.char_indices() {
        match c {
            '"' => quoted = !quoted,
            ',' if !quoted => {
                parts.push(&text[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(&text[start..]);
    parts
}

/// Why a token challenge is refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChallengeRefusal {
    /// Not a `Bearer` challenge with a realm, or a realm that does not parse.
    Unparseable,
    /// The realm is not HTTPS.
    NotHttps,
    /// The realm's host is an IP literal.
    IpLiteral,
    /// The realm's host is neither the registry's nor one the operator listed.
    ForeignHost(String),
    /// The scope is not `repository:<path>:pull`.
    Scope(String),
}

/// The token URL a registry's challenge asks for, only when it stays where it may: HTTPS, on
/// `registry`'s host or one of `realm_hosts`, not an IP literal, and scoped to pulling `path`.
/// Pinned by `tests::should_refuse_a_challenge_when_its_realm_or_scope_goes_elsewhere`.
///
/// # Errors
/// [`ChallengeRefusal`].
pub fn token_url(
    registry: &str,
    path: &str,
    header: &str,
    realm_hosts: &[String],
) -> Result<Url, ChallengeRefusal> {
    let (realm, params) = bearer_challenge(header).ok_or(ChallengeRefusal::Unparseable)?;
    let url = Url::parse_with_params(&realm, &params).map_err(|_| ChallengeRefusal::Unparseable)?;
    if url.scheme() != "https" {
        return Err(ChallengeRefusal::NotHttps);
    }
    let host = url
        .host_str()
        .ok_or(ChallengeRefusal::Unparseable)?
        .to_ascii_lowercase();
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    if bare.parse::<IpAddr>().is_ok() {
        return Err(ChallengeRefusal::IpLiteral);
    }
    let registry_host = Url::parse(&format!("https://{registry}/"))
        .ok()
        .and_then(|u| u.host_str().map(str::to_ascii_lowercase));
    if registry_host.as_deref() != Some(host.as_str()) && !realm_hosts.contains(&host) {
        return Err(ChallengeRefusal::ForeignHost(host));
    }
    let wanted = format!("repository:{path}:pull");
    let scope = params
        .iter()
        .find(|(key, _)| key == "scope")
        .map(|(_, value)| value.clone())
        .unwrap_or_default();
    if scope != wanted {
        return Err(ChallengeRefusal::Scope(scope));
    }
    Ok(url)
}

/// Whether `ip` is a public unicast address: not loopback, unspecified, private, shared
/// (100.64/10), link-local, unique-local, multicast, broadcast or documentation, and not one of
/// those mapped into IPv6. Pinned by
/// `tests::should_call_an_address_public_when_it_is_public_unicast`.
#[must_use]
pub fn public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            let shared = a == 100 && (64..128).contains(&b);
            !(v4.is_loopback()
                || v4.is_unspecified()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_multicast()
                || v4.is_broadcast()
                || v4.is_documentation()
                || shared
                || a == 0)
        }
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or_else(
            || {
                !(v6.is_loopback()
                    || v6.is_unspecified()
                    || v6.is_multicast()
                    || v6.is_unique_local()
                    || v6.is_unicast_link_local())
            },
            |v4| public(IpAddr::V4(v4)),
        ),
    }
}

/// A resolver that answers only when every address a name resolves to is [`public`].
#[derive(Debug, Clone, Copy, Default)]
pub struct PublicOnly;

impl PublicOnly {
    /// The addresses of `host`, refused when any is not public.
    ///
    /// # Errors
    /// The name did not resolve, or resolved to an address that is not public.
    pub async fn addresses(host: String) -> Result<Vec<SocketAddr>, std::io::Error> {
        let resolved = blocking(move || {
            (host.as_str(), 443)
                .to_socket_addrs()
                .map(Iterator::collect::<Vec<_>>)
        })
        .await
        .map_err(|err| err.unwrap_or_else(|| std::io::Error::other("resolver lost")))?;
        if resolved.is_empty() || !resolved.iter().all(|addr| public(addr.ip())) {
            return Err(std::io::Error::other("refused: not a public address"));
        }
        Ok(resolved)
    }
}

impl Resolve for PublicOnly {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_owned();
        Box::pin(async move {
            let addresses = Self::addresses(host).await?;
            let addrs: Addrs = Box::new(addresses.into_iter());
            Ok(addrs)
        })
    }
}

/// Manifests from the registry over HTTPS.
#[derive(Debug, Clone)]
pub struct RegistryManifests {
    client: reqwest::Client,
    token_client: reqwest::Client,
    realm_hosts: Vec<String>,
}

/// The TLS setup could not be built.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TlsSetupError;

fn tls() -> Result<ClientConfig, TlsSetupError> {
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let verifier = Verifier::new_with_extra_roots(Vec::new(), Arc::clone(&provider))
        .map_err(|_| TlsSetupError)?;
    Ok(ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|_| TlsSetupError)?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth())
}

fn client(public_only: bool) -> Result<reqwest::Client, TlsSetupError> {
    let builder = reqwest::Client::builder()
        .tls_backend_preconfigured(tls()?)
        .https_only(true)
        .redirect(reqwest::redirect::Policy::none())
        .timeout(FETCH_DEADLINE);
    let builder = if public_only {
        builder.dns_resolver(Arc::new(PublicOnly))
    } else {
        builder
    };
    builder.build().map_err(|_| TlsSetupError)
}

impl RegistryManifests {
    /// A client trusting what the operating system trusts, HTTPS only, accepting a token realm
    /// on the registry's own host only.
    ///
    /// # Errors
    /// [`TlsSetupError`].
    pub fn new() -> Result<Self, TlsSetupError> {
        Self::with_realm_hosts(Vec::new())
    }

    /// As [`Self::new`], also accepting a token realm on each of `realm_hosts` (lowercase host
    /// names, from the operator's config: a registry whose tokens come from another host).
    ///
    /// # Errors
    /// [`TlsSetupError`].
    pub fn with_realm_hosts(realm_hosts: Vec<String>) -> Result<Self, TlsSetupError> {
        Ok(Self {
            client: client(false)?,
            token_client: client(true)?,
            realm_hosts,
        })
    }

    async fn body(mut response: reqwest::Response, limit: usize) -> Result<Vec<u8>, FetchError> {
        if response
            .content_length()
            .is_some_and(|len| len > u64::try_from(limit).unwrap_or(u64::MAX))
        {
            return Err(FetchError::TooLarge);
        }
        read_capped(limit, &mut response).await
    }

    async fn token(
        &self,
        registry: &str,
        path: &str,
        challenge: &HeaderMap,
    ) -> Result<Secret<String>, FetchError> {
        let header = challenge
            .get(WWW_AUTHENTICATE)
            .and_then(|v| v.to_str().ok())
            .ok_or(FetchError::Failed)?;
        let url =
            token_url(registry, path, header, &self.realm_hosts).map_err(|_| FetchError::Failed)?;
        let response = self
            .token_client
            .get(url)
            .send()
            .await
            .map_err(|_| FetchError::Failed)?;
        if !response.status().is_success() {
            return Err(FetchError::Failed);
        }
        let body = Self::body(response, TOKEN_LIMIT).await?;
        let answer: serde_json::Value =
            serde_json::from_slice(&body).map_err(|_| FetchError::Failed)?;
        answer["token"]
            .as_str()
            .or_else(|| answer["access_token"].as_str())
            .map(|token| Secret::new(token.to_owned()))
            .ok_or(FetchError::Failed)
    }

    async fn fetch_within(
        &self,
        repository: &str,
        digest: &ImageDigest,
    ) -> Result<Vec<u8>, FetchError> {
        let (registry, path) = repository.split_once('/').ok_or(FetchError::Failed)?;
        let url = format!("https://{registry}/v2/{path}/manifests/{digest}");
        let request = || self.client.get(&url).header(ACCEPT, MANIFEST_V1);
        let mut response = request().send().await.map_err(|_| FetchError::Failed)?;
        if response.status() == StatusCode::UNAUTHORIZED {
            let token = self.token(registry, path, response.headers()).await?;
            response = request()
                .bearer_auth(token.expose())
                .send()
                .await
                .map_err(|_| FetchError::Failed)?;
        }
        match response.status() {
            StatusCode::OK => Self::body(response, MANIFEST_LIMIT).await,
            StatusCode::NOT_FOUND => Err(FetchError::NotFound),
            _ => Err(FetchError::Failed),
        }
    }
}

impl ManifestSource for RegistryManifests {
    async fn fetch(&self, repository: &str, digest: &ImageDigest) -> Result<Vec<u8>, FetchError> {
        tokio::time::timeout(FETCH_DEADLINE, self.fetch_within(repository, digest))
            .await
            .unwrap_or(Err(FetchError::Failed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_accept_a_manifest_only_when_its_exact_bytes_hash_to_the_digest() {
        // given
        let raw = br#"{"schemaVersion":2}"#;
        let digest = ImageDigest::new(&format!("sha256:{}", sha256_hex(raw))).expect("a digest");
        let reserialized = b"{\n  \"schemaVersion\": 2\n}";

        // when
        let judged = [matches(raw, &digest), matches(reserialized, &digest)];

        // then
        assert_eq!(judged, [true, false]);
    }

    /// Chunks from a list, counting how many were asked for; endless when the list is.
    struct Listed {
        chunks: Vec<Option<Vec<u8>>>,
        endless: bool,
        asked: usize,
    }

    impl Chunks for Listed {
        async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, FetchError> {
            self.asked += 1;
            if self.endless {
                return Ok(Some(vec![0u8; 1024]));
            }
            Ok(self.chunks.remove(0))
        }
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("a runtime")
    }

    #[test]
    fn should_stop_reading_when_the_body_passes_the_limit() {
        // given: an endless body of 1 KiB chunks
        let mut body = Listed {
            chunks: Vec::new(),
            endless: true,
            asked: 0,
        };

        // when
        let read = runtime().block_on(read_capped(MANIFEST_LIMIT, &mut body));

        // then
        assert_eq!(read, Err(FetchError::TooLarge));
        assert_eq!(
            body.asked,
            MANIFEST_LIMIT / 1024 + 1,
            "no chunk past the one that crossed"
        );
    }

    #[test]
    fn should_return_the_body_when_it_ends_within_the_limit() {
        // given
        let mut body = Listed {
            chunks: vec![Some(b"ab".to_vec()), Some(b"cd".to_vec()), None],
            endless: false,
            asked: 0,
        };

        // when
        let read = runtime().block_on(read_capped(4, &mut body));

        // then
        assert_eq!(read, Ok(b"abcd".to_vec()));
    }

    #[test]
    fn should_refuse_a_challenge_when_its_realm_or_scope_goes_elsewhere() {
        // given
        let challenge = |realm: &str, scope: &str| {
            format!(r#"Bearer realm="{realm}",service="ghcr.io",scope="{scope}""#)
        };
        let pull = "repository:acme/runner:pull";
        let listed = vec!["auth.example".to_owned()];
        let cases = [
            (challenge("https://ghcr.io/token", pull), Ok(())),
            (challenge("https://auth.example/token", pull), Ok(())),
            (
                challenge("http://ghcr.io/token", pull),
                Err(ChallengeRefusal::NotHttps),
            ),
            (
                challenge("https://127.0.0.1/token", pull),
                Err(ChallengeRefusal::IpLiteral),
            ),
            (
                challenge("https://[::1]/token", pull),
                Err(ChallengeRefusal::IpLiteral),
            ),
            (
                challenge("https://metadata.internal/token", pull),
                Err(ChallengeRefusal::ForeignHost(
                    "metadata.internal".to_owned(),
                )),
            ),
            (
                challenge("https://ghcr.io/token", "repository:acme/runner:pull,push"),
                Err(ChallengeRefusal::Scope(
                    "repository:acme/runner:pull,push".to_owned(),
                )),
            ),
            (
                challenge("https://ghcr.io/token", "repository:other/image:pull"),
                Err(ChallengeRefusal::Scope(
                    "repository:other/image:pull".to_owned(),
                )),
            ),
            (
                r#"Basic realm="https://ghcr.io/""#.to_owned(),
                Err(ChallengeRefusal::Unparseable),
            ),
        ];

        for (header, expected) in cases {
            // when
            let judged = token_url("ghcr.io", "acme/runner", &header, &listed).map(drop);

            // then
            assert_eq!(judged, expected, "{header}");
        }
    }

    #[test]
    fn should_call_an_address_public_when_it_is_public_unicast() {
        // given
        let addresses = [
            ("140.82.112.3", true),
            ("2606:4700::1111", true),
            ("127.0.0.1", false),
            ("10.1.2.3", false),
            ("172.16.0.1", false),
            ("192.168.1.1", false),
            ("169.254.169.254", false),
            ("100.64.0.1", false),
            ("0.0.0.0", false),
            ("::1", false),
            ("fe80::1", false),
            ("fd00::1", false),
            ("::ffff:10.0.0.1", false),
        ];

        for (text, expected) in addresses {
            // when
            let judged = public(text.parse().expect("an address"));

            // then
            assert_eq!(judged, expected, "{text}");
        }
    }

    #[test]
    fn should_refuse_to_resolve_a_name_when_it_points_at_a_loopback_address() {
        // given
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("a runtime");

        // when
        let resolved = runtime.block_on(PublicOnly::addresses("localhost".to_owned()));

        // then
        assert!(resolved.is_err(), "{resolved:?}");
    }

    #[test]
    fn should_keep_the_token_out_of_debug_output_when_it_is_held() {
        // given
        let token = Secret::new("anonymous-token-value".to_owned());

        // when
        let shown = format!("{token:?}");

        // then
        assert!(!shown.contains("anonymous-token-value"));
    }

    #[test]
    fn should_read_a_bearer_challenge_when_the_registry_asks_for_a_token() {
        // given
        let header = r#"Bearer realm="https://ghcr.io/token",service="ghcr.io",scope="repository:a/b:pull,push""#;

        // when
        let parsed = bearer_challenge(header);
        let basic = bearer_challenge(r#"Basic realm="x""#);

        // then
        assert_eq!(
            parsed,
            Some((
                "https://ghcr.io/token".to_owned(),
                vec![
                    ("service".to_owned(), "ghcr.io".to_owned()),
                    ("scope".to_owned(), "repository:a/b:pull,push".to_owned()),
                ]
            ))
        );
        assert_eq!(basic, None);
    }
}
