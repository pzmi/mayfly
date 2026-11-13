//! The fake served over HTTP/1.1 on a loopback port.

use std::io;
use std::net::SocketAddr;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::{Request, State};
use axum::http::{HeaderName, HeaderValue, StatusCode};
use axum::response::Response;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use crate::state::{FakeGithub, FakeRequest};

/// The largest request body the fake reads.
const MAX_BODY: usize = 1024 * 1024;

/// A running server. [`FakeServer::stop`] shuts it down and waits for it; dropping it aborts
/// the task, so no server outlives its test.
#[derive(Debug)]
pub struct FakeServer {
    addr: SocketAddr,
    fake: FakeGithub,
    shutdown: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<io::Result<()>>>,
}

impl FakeServer {
    /// Serves `fake` on `127.0.0.1` at a port the system picks, on the current runtime.
    ///
    /// # Errors
    /// When the port cannot be bound.
    pub async fn start(fake: FakeGithub) -> io::Result<Self> {
        Self::start_on(fake, 0).await
    }

    /// Serves `fake` on `127.0.0.1:port` (0: a port the system picks), on the current runtime.
    ///
    /// # Errors
    /// When the port cannot be bound.
    pub async fn start_on(fake: FakeGithub, port: u16) -> io::Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", port)).await?;
        Self::serve(listener, fake)
    }

    /// Binds a port the system picks on `127.0.0.1`, then serves the fake `build` makes from
    /// its base URL (`http://127.0.0.1:<port>`), on the current runtime: a fake whose service
    /// URL is its own base, so a client over a real socket reaches the Actions service through
    /// it too (a daemon test, RT-07). Pinned by
    /// `tests/wire/process.rs::should_serve_the_service_on_its_own_base_when_built_from_it`.
    ///
    /// # Errors
    /// When the port cannot be bound.
    pub async fn start_with(build: impl FnOnce(&str) -> FakeGithub) -> io::Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
        let base = format!("http://{}", listener.local_addr()?);
        Self::serve(listener, build(&base))
    }

    fn serve(listener: TcpListener, fake: FakeGithub) -> io::Result<Self> {
        let addr = listener.local_addr()?;
        let app = Router::new().fallback(serve).with_state(fake.clone());
        let (tx, rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = rx.await;
                })
                .await
        });
        Ok(Self {
            addr,
            fake,
            shutdown: Some(tx),
            task: Some(task),
        })
    }

    /// `http://127.0.0.1:<port>`.
    #[must_use]
    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Stops accepting, ends stalled and held requests with 503 ([`FakeGithub::close`]),
    /// finishes the requests in flight and waits for the task.
    ///
    /// # Errors
    /// What the server ended with.
    ///
    /// # Panics
    /// When the server task panicked.
    pub async fn stop(mut self) -> io::Result<()> {
        self.fake.close();
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        match self.task.take() {
            Some(task) => task.await.expect("the fake server task panicked"),
            None => Ok(()),
        }
    }
}

impl Drop for FakeServer {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

async fn serve(State(fake): State<FakeGithub>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let Ok(body) = to_bytes(body, MAX_BODY).await else {
        return plain(StatusCode::PAYLOAD_TOO_LARGE);
    };
    let headers = parts
        .headers
        .iter()
        .filter_map(|(n, v)| Some((n.as_str().to_owned(), v.to_str().ok()?.to_owned())))
        .collect();
    let answer = fake
        .handle(FakeRequest {
            method: parts.method.as_str().to_owned(),
            path: parts
                .uri
                .path_and_query()
                .map_or_else(|| parts.uri.path().to_owned(), |pq| pq.as_str().to_owned()),
            headers,
            body: body.to_vec(),
        })
        .await;
    let mut response = Response::new(Body::from(answer.body));
    *response.status_mut() =
        StatusCode::from_u16(answer.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    for (name, value) in answer.headers {
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(&value),
        ) {
            response.headers_mut().insert(name, value);
        }
    }
    response
}

fn plain(status: StatusCode) -> Response {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = status;
    response
}
