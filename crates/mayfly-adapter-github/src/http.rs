//! Requests and answers as plain data, and the `Transport` seam they cross.
//!
//! The client's policy (retries, deadlines, classification, metrics) is written over these
//! values, not over `reqwest`'s types, so it runs in process against the fake under a paused
//! clock; [`crate::ReqwestTransport`] is the one implementation that touches the network.

use std::fmt;
use std::future::Future;

use mayfly_domain::Secret;
use url::Url;

/// The largest answer body read; a larger one is refused, never truncated (foundation.md §4.5:
/// upstream bodies are bounded). The auth chain's answers are well under a kilobyte.
pub const MAX_RESPONSE_BYTES: usize = 1024 * 1024;

/// An HTTP method the client uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    /// `GET`.
    Get,
    /// `POST`.
    Post,
    /// `PATCH`.
    Patch,
    /// `DELETE`.
    Delete,
}

impl Method {
    /// The method's name on the wire.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Patch => "PATCH",
            Self::Delete => "DELETE",
        }
    }
}

/// A request. Its credential is a [`Secret`] apart from the other headers, so `Debug` never
/// prints it.
#[derive(Debug)]
pub struct HttpRequest {
    /// The method.
    pub method: Method,
    /// The URL.
    pub url: Url,
    /// The whole `Authorization` value (`Bearer …`, `RemoteAuth …`).
    pub authorization: Option<Secret<String>>,
    /// Other headers, never a credential.
    pub headers: Vec<(&'static str, String)>,
    /// A JSON body.
    pub body: Option<Vec<u8>>,
}

/// An answer: status, headers with lowercase names, and a body of at most
/// [`MAX_RESPONSE_BYTES`]. Its `Debug` prints the body's length only, since a body may carry a
/// token.
#[derive(Clone, PartialEq, Eq)]
pub struct HttpResponse {
    /// The status code.
    pub status: u16,
    /// Headers, lowercase names.
    pub headers: Vec<(String, String)>,
    /// The body.
    pub body: Vec<u8>,
}

impl fmt::Debug for HttpResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpResponse")
            .field("status", &self.status)
            .field("body_bytes", &self.body.len())
            .finish_non_exhaustive()
    }
}

impl HttpResponse {
    /// The first value of header `name` (lowercase).
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    /// The body with a UTF-8 byte-order mark trimmed (spec §2: the service sometimes sends one).
    #[must_use]
    pub fn body_without_bom(&self) -> &[u8] {
        self.body
            .strip_prefix(&[0xEF, 0xBB, 0xBF])
            .unwrap_or(&self.body)
    }
}

/// A request that got no HTTP answer. Carries a kind only: a transport library's message may
/// hold the URL with its query (spec §2), so none is kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportError {
    /// No connection could be made (refused, unreachable, DNS).
    Connect,
    /// The server's certificate was refused. Never retried (spec §8): a retry cannot fix it.
    Certificate,
    /// The connection broke while sending or receiving.
    Io,
    /// The answer's body was larger than [`MAX_RESPONSE_BYTES`].
    BodyTooLarge,
    /// The request could not be built: a credential or header value is not a valid header
    /// value (a line break, a control byte). Nothing was sent; a retry would fail alike.
    InvalidRequest,
}

impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Connect => "no connection could be made",
            Self::Certificate => "the server's certificate was refused",
            Self::Io => "the connection broke",
            Self::BodyTooLarge => "the answer was too large",
            Self::InvalidRequest => "the request could not be built (an invalid header value)",
        })
    }
}

/// Sends one request and reads its whole answer. No retry, no deadline: the client adds both.
pub trait Transport: Send + Sync {
    /// Sends `request`.
    ///
    /// # Errors
    /// [`TransportError`] when no complete HTTP answer arrived.
    fn send(
        &self,
        request: HttpRequest,
    ) -> impl Future<Output = Result<HttpResponse, TransportError>> + Send;
}
