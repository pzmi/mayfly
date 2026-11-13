//! The registry served over HTTPS on a loopback port, from a thread of its own.

use std::future::IntoFuture as _;
use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderValue, Method, StatusCode, Uri, header};
use axum::response::Response;
use serde_json::json;
use tokio::sync::oneshot;
use tokio_rustls::TlsAcceptor;

use crate::image::Images;
use crate::tls::{Authority, TlsListener};

/// One request the registry answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Served {
    /// The method, like `GET`.
    pub method: String,
    /// The path, without a query.
    pub path: String,
    /// The status answered.
    pub status: u16,
}

#[derive(Debug)]
struct Shared {
    images: Images,
    served: Mutex<Vec<Served>>,
}

/// A running registry. It serves on its own thread and current-thread runtime, so a test that
/// blocks its own thread (on a child's output, say) is still answered. [`FakeRegistry::stop`]
/// and dropping it both stop the runtime, which drops every open connection, and join the
/// thread: no registry outlives its test.
#[derive(Debug)]
pub struct FakeRegistry {
    addr: SocketAddr,
    ca_pem: String,
    ca_der: Vec<u8>,
    shared: Arc<Shared>,
    shutdown: Option<oneshot::Sender<()>>,
    thread: Option<JoinHandle<io::Result<()>>>,
}

impl FakeRegistry {
    /// Serves `images` over HTTPS on `127.0.0.1` at a port the system picks, under a CA made
    /// for this registry alone. Pinned by `tests/registry.rs`.
    ///
    /// # Errors
    /// When the certificates, the port, the thread or the runtime cannot be had.
    pub fn start(images: Images) -> io::Result<Self> {
        let authority = Authority::generate()?;
        let config = authority.server_config()?;
        let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        listener.set_nonblocking(true)?;
        let addr = listener.local_addr()?;
        let shared = Arc::new(Shared {
            images,
            served: Mutex::new(Vec::new()),
        });
        let (tx, rx) = oneshot::channel::<()>();
        let state = Arc::clone(&shared);
        let thread = std::thread::Builder::new()
            .name("mayfly-fake-registry".to_owned())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_io()
                    .build()?;
                runtime.block_on(async move {
                    let listener = TlsListener {
                        tcp: tokio::net::TcpListener::from_std(listener)?,
                        acceptor: TlsAcceptor::from(Arc::new(config)),
                    };
                    let app = Router::new().fallback(handle).with_state(state);
                    tokio::select! {
                        served = axum::serve(listener, app).into_future() => served,
                        _ = rx => Ok(()),
                    }
                })
            })?;
        Ok(Self {
            addr,
            ca_pem: authority.ca_pem,
            ca_der: authority.ca_der.to_vec(),
            shared,
            shutdown: Some(tx),
            thread: Some(thread),
        })
    }

    /// `127.0.0.1:<port>`: the registry part of an image reference.
    #[must_use]
    pub fn host(&self) -> String {
        self.addr.to_string()
    }

    /// The address the registry listens on.
    #[must_use]
    pub const fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// The CA certificate in PEM, for a client's trust store (`SSL_CERT_FILE`).
    #[must_use]
    pub fn ca_pem(&self) -> &str {
        &self.ca_pem
    }

    /// The CA certificate in DER.
    #[must_use]
    pub fn ca_der(&self) -> &[u8] {
        &self.ca_der
    }

    /// Every request answered so far, in order.
    #[must_use]
    pub fn served(&self) -> Vec<Served> {
        self.shared
            .served
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Stops serving and waits for the thread.
    ///
    /// # Errors
    /// What the server ended with; a panicked thread as [`io::ErrorKind::Other`].
    pub fn stop(mut self) -> io::Result<()> {
        self.halt()
    }

    fn halt(&mut self) -> io::Result<()> {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        match self.thread.take() {
            Some(thread) => thread
                .join()
                .unwrap_or_else(|_| Err(io::Error::other("the registry thread panicked"))),
            None => Ok(()),
        }
    }
}

impl Drop for FakeRegistry {
    fn drop(&mut self) {
        let _ = self.halt();
    }
}

async fn handle(State(shared): State<Arc<Shared>>, method: Method, uri: Uri) -> Response {
    let path = uri.path().to_owned();
    let response = answer(&shared.images, &method, &path);
    shared
        .served
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push(Served {
            method: method.as_str().to_owned(),
            path,
            status: response.status().as_u16(),
        });
    response
}

fn respond(
    status: StatusCode,
    content_type: &str,
    digest: Option<&str>,
    body: Vec<u8>,
) -> Response {
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = status;
    let headers = response.headers_mut();
    if let Ok(value) = HeaderValue::from_str(content_type) {
        headers.insert(header::CONTENT_TYPE, value);
    }
    if let Some(value) = digest.and_then(|d| HeaderValue::from_str(d).ok()) {
        headers.insert("docker-content-digest", value);
    }
    headers.insert(
        "docker-distribution-api-version",
        HeaderValue::from_static("registry/2.0"),
    );
    response
}

/// An OCI error answer: `{"errors":[{"code":..,"message":..}]}`.
fn error(status: StatusCode, code: &str, message: &str) -> Response {
    let body = serde_json::to_vec(&json!({"errors": [{"code": code, "message": message}]}))
        .unwrap_or_default();
    respond(status, "application/json", None, body)
}

/// What the registry answers `method path`: the OCI distribution API's pull subset (see the
/// crate docs). A `HEAD` gets the `GET`'s headers; the HTTP layer leaves the body out.
fn answer(images: &Images, method: &Method, path: &str) -> Response {
    if method != Method::GET && method != Method::HEAD {
        let mut refused = error(
            StatusCode::METHOD_NOT_ALLOWED,
            "UNSUPPORTED",
            "the registry is read only",
        );
        refused
            .headers_mut()
            .insert(header::ALLOW, HeaderValue::from_static("GET, HEAD"));
        return refused;
    }
    if path == "/v2/" || path == "/v2" {
        return respond(StatusCode::OK, "application/json", None, b"{}".to_vec());
    }
    let Some(rest) = path.strip_prefix("/v2/") else {
        return error(StatusCode::NOT_FOUND, "UNSUPPORTED", "not a registry route");
    };
    if let Some((name, reference)) = rest.rsplit_once("/manifests/")
        && !name.is_empty()
    {
        return match images.manifest(name, reference) {
            Some(published) => respond(
                StatusCode::OK,
                &published.media_type,
                Some(&published.digest),
                published.bytes.clone(),
            ),
            None if images.knows(name) => error(
                StatusCode::NOT_FOUND,
                "MANIFEST_UNKNOWN",
                "manifest unknown to registry",
            ),
            None => error(
                StatusCode::NOT_FOUND,
                "NAME_UNKNOWN",
                "repository name not known to registry",
            ),
        };
    }
    if let Some((name, digest)) = rest.rsplit_once("/blobs/")
        && !name.is_empty()
    {
        return match images.blob(name, digest) {
            Some(blob) => respond(
                StatusCode::OK,
                "application/octet-stream",
                Some(digest),
                blob.clone(),
            ),
            None => error(
                StatusCode::NOT_FOUND,
                "BLOB_UNKNOWN",
                "blob unknown to registry",
            ),
        };
    }
    error(StatusCode::NOT_FOUND, "UNSUPPORTED", "not a registry route")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image::{GuestOs, TartImage};

    fn code(response: &Response) -> u16 {
        response.status().as_u16()
    }

    fn header_of<'r>(response: &'r Response, name: &str) -> Option<&'r str> {
        response.headers().get(name).and_then(|v| v.to_str().ok())
    }

    #[test]
    fn should_answer_each_route_of_the_pull_subset_when_asked() {
        // given
        let image = TartImage::synthetic(b"runner-image", GuestOs::Linux);
        let images = Images::new().publish("mayfly/runner", &image);
        let blob = image.blobs().keys().next().expect("a blob").clone();
        let manifest = format!("/v2/mayfly/runner/manifests/{}", image.digest());
        let unknown = format!("sha256:{}", "0".repeat(64));
        let cases = [
            (Method::GET, "/v2/".to_owned(), 200, None),
            (Method::GET, manifest.clone(), 200, None),
            (Method::HEAD, manifest.clone(), 200, None),
            (
                Method::GET,
                format!("/v2/mayfly/runner/blobs/{blob}"),
                200,
                None,
            ),
            (
                Method::GET,
                format!("/v2/mayfly/runner/manifests/{unknown}"),
                404,
                Some("MANIFEST_UNKNOWN"),
            ),
            (
                Method::GET,
                format!("/v2/other/runner/manifests/{}", image.digest()),
                404,
                Some("NAME_UNKNOWN"),
            ),
            (
                Method::GET,
                format!("/v2/mayfly/runner/blobs/{unknown}"),
                404,
                Some("BLOB_UNKNOWN"),
            ),
            (
                Method::GET,
                "/v2/manifests/x".to_owned(),
                404,
                Some("UNSUPPORTED"),
            ),
            (Method::GET, "/other".to_owned(), 404, Some("UNSUPPORTED")),
            (Method::PUT, manifest, 405, Some("UNSUPPORTED")),
        ];

        for (method, path, status, error_code) in cases {
            // when
            let response = answer(&images, &method, &path);

            // then
            assert_eq!(code(&response), status, "{method} {path}");
            assert_eq!(
                header_of(&response, "docker-distribution-api-version"),
                Some("registry/2.0")
            );
            match error_code {
                Some(_) => assert_eq!(
                    header_of(&response, "content-type"),
                    Some("application/json"),
                    "{method} {path}"
                ),
                None if path.contains("/manifests/") => {
                    assert_eq!(
                        header_of(&response, "content-type"),
                        Some("application/vnd.oci.image.manifest.v1+json")
                    );
                    assert_eq!(
                        header_of(&response, "docker-content-digest"),
                        Some(image.digest())
                    );
                }
                None => {}
            }
            if status == 405 {
                assert_eq!(header_of(&response, "allow"), Some("GET, HEAD"));
            }
        }
    }
}
