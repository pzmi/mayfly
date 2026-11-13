//! The daemon's end of the operator socket (plan §11, RT-03): a `0600` Unix socket, the
//! peer's uid checked on every connection, one request per connection.
//!
//! **Who may ask.** The socket file is `0600` in a directory private to the daemon's user (the
//! state directory, or `$XDG_RUNTIME_DIR`), and the peer's uid, read from the connection by the
//! kernel (`SO_PEERCRED` on Linux, `getpeereid` on macOS, through tokio's `peer_cred`), must be
//! the daemon's own effective uid ([`Operators`]): root is refused unless the daemon runs as
//! root. There is no configuration key for other uids yet (OD20's socket-authorisation item
//! stays a planner default). Pinned by
//! `tests/operator.rs::should_refuse_every_request_when_the_peer_is_not_an_operator`.
//!
//! **The actor is the connection's.** This module is the only production code that builds
//! `Actor::Operator` (`ARCH-operator-actor`, KER-10F), from the peer uid above; nothing a
//! client sends names an actor, and a field claiming one is ignored like any unknown field.
//! Pinned by `tests/operator.rs::should_decide_under_the_peers_uid_when_the_body_claims_another_operator`.
//!
//! **Bounded and supervised.** The server is one of the daemon's supervised futures, polled by
//! its own task, never spawned (`crate::supervise`): at most [`MAX_CONNECTIONS`] connections
//! are served at once (one more is answered `busy` without being read), a request is at most
//! [`MAX_REQUEST`] bytes and must arrive within [`REQUEST_DEADLINE`], and every answer line must
//! be written within [`WRITE_DEADLINE`]. On stop, it accepts nothing more, ends every `events`
//! stream at its next line (a send in progress is abandoned), lets each command in flight finish
//! its save for at most `crate::daemon::SHUTDOWN_DEADLINE`, drops what is left, then removes its
//! socket file: a client that stops reading cannot keep the daemon from exiting. Pinned by
//! `tests/operator.rs::should_return_when_stopped_while_a_follower_never_reads`,
//! `should_answer_busy_when_every_connection_slot_is_taken`,
//! `should_refuse_a_request_when_it_is_longer_than_the_limit` and
//! `should_refuse_a_request_when_it_does_not_arrive_in_time`.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::future::Future;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::task::Poll;
use std::time::Duration;

use mayfly_control::commands::{
    CommandError, HostAction, PoolAction, host_action, pool_action, reset_breakers,
};
use mayfly_control::journal::{Journal, StreamId, StreamRevision};
use mayfly_control::projection::ProjectionStore;
use mayfly_control::queries;
use mayfly_control::store::{AggregateStore, Clock, Committed, Rng, StoreNotifier};
use mayfly_control_domain::Actor;
use mayfly_control_domain::operations::hostrun::{HostRun, HostRunState, RunPhase};
use mayfly_domain::{Epoch, HostId, ImageDigest, Millis, PoolName};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::unix::OwnedWriteHalf;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, watch};

use super::status;
use super::wire::{
    API, Answer, COMMANDS, DEFAULT_DRAIN_SECS, EventLine, Following, Lagged, MAX_SINCE_SECS,
    PoolList, Request, Reset, Status, VmList,
};
use crate::sizing::SizingReport;
use crate::supervise::ShutdownSignal;

/// Connections served at once.
pub const MAX_CONNECTIONS: usize = 16;

/// The longest request line, in bytes.
pub const MAX_REQUEST: u64 = 64 * 1024;

/// How long a connection may take to send its request.
pub const REQUEST_DEADLINE: Duration = Duration::from_secs(10);

/// How long one answer line may take to be written: a client that stops reading is dropped.
pub const WRITE_DEADLINE: Duration = Duration::from_secs(10);

pub use crate::paths::SOCKET_PATH_MAX;

/// How long the server waits after an `accept` error before accepting again.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);

/// Events `events --since` sends at most: the newest of them.
pub const MAX_HISTORY: usize = 10_000;

/// Streams read per page when `events --since` enumerates the journal.
const STREAM_PAGE: u32 = 256;

/// Why the socket could not be opened.
#[derive(Debug)]
pub enum BindError {
    /// The socket's directory is not private to this user (ADR 0087's layout rule).
    Directory(mayfly_adapter_tart::verify::UnsafeLayout),
    /// The same, for the macOS fallback place in `$TMPDIR` (`crate::paths::socket`).
    FallbackDirectory(mayfly_adapter_tart::verify::UnsafeLayout),
    /// Something other than a stale socket of this user is at the path (another file, a link,
    /// another user's socket, or a socket a live daemon answers on): nothing was removed.
    Occupied(PathBuf),
    /// The path is longer than [`SOCKET_PATH_MAX`] bytes.
    TooLong(PathBuf),
    /// The operating system refused.
    Io(std::io::Error),
}

impl std::fmt::Display for BindError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Directory(e) => write!(
                f,
                "the operator socket's directory is not private to this user: {e:?}"
            ),
            Self::FallbackDirectory(e) => write!(
                f,
                "the operator socket's fallback directory ($TMPDIR, chosen because the default \
                 socket path is too long for a socket address) is not private to this user: \
                 {e:?}; configure a short state_dir instead"
            ),
            Self::Occupied(path) => write!(
                f,
                "{} is in use: another mayflyd answers there, or it is not this user's socket; \
                 stop that daemon, remove the file, or name another state_dir",
                path.display()
            ),
            Self::TooLong(path) => write!(
                f,
                "the operator socket path {} is longer than {SOCKET_PATH_MAX} bytes, the most a \
                 socket address holds; configure a short state_dir (the socket goes in it) and \
                 reach it with mayfly --config, or with mayfly --socket <path>",
                path.display()
            ),
            Self::Io(e) => write!(f, "cannot open the operator socket: {e}"),
        }
    }
}

impl std::error::Error for BindError {}

/// The listening socket; its file is removed when it is dropped, if it is still this one.
#[derive(Debug)]
pub struct Listener {
    inner: UnixListener,
    path: PathBuf,
    /// The socket file's device and inode, so a drop never removes another daemon's socket.
    identity: (u64, u64),
}

impl Listener {
    /// Opens the socket at `path`, mode `0600`, in a directory private to this user. A socket
    /// this user left there that nothing answers on (a daemon that was killed) is replaced;
    /// anything else is refused: another file, a link, another user's socket, and a socket a
    /// live daemon answers on (the state-dir lock does not cover a socket in the runtime
    /// directory, which two daemons of one user with different states could both name). Must
    /// run inside the Tokio runtime. Pinned by
    /// `tests/operator.rs::should_create_the_socket_private_when_the_server_binds`,
    /// `should_replace_a_stale_socket_and_refuse_anything_else_when_binding` and
    /// `should_refuse_a_socket_path_when_it_is_too_long`.
    ///
    /// # Errors
    /// [`BindError`].
    pub fn bind(path: &Path) -> Result<Self, BindError> {
        if !crate::paths::fits(path) {
            return Err(BindError::TooLong(path.to_path_buf()));
        }
        let dir = path
            .parent()
            .ok_or_else(|| BindError::Occupied(path.to_path_buf()))?;
        mayfly_adapter_tart::verify::check_private(dir).map_err(|e| {
            if crate::paths::is_fallback(path) {
                BindError::FallbackDirectory(e)
            } else {
                BindError::Directory(e)
            }
        })?;
        match std::fs::symlink_metadata(path) {
            Ok(meta) => {
                use std::os::unix::fs::FileTypeExt as _;
                let ours = meta.file_type().is_socket()
                    && meta.uid() == rustix::process::geteuid().as_raw();
                if !ours {
                    return Err(BindError::Occupied(path.to_path_buf()));
                }
                // stale only when nothing answers: a live daemon's socket is never unlinked
                if !stale(path) {
                    return Err(BindError::Occupied(path.to_path_buf()));
                }
                std::fs::remove_file(path).map_err(BindError::Io)?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(BindError::Io(e)),
        }
        let inner = UnixListener::bind(path).map_err(BindError::Io)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(BindError::Io)?;
        let meta = std::fs::symlink_metadata(path).map_err(BindError::Io)?;
        Ok(Self {
            inner,
            path: path.to_path_buf(),
            identity: (meta.dev(), meta.ino()),
        })
    }

    /// Where it listens.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Whether the socket at `path` is stale: a non-blocking connect is refused. A connect that
/// succeeds, would block (a live daemon whose backlog is full) or fails otherwise is not.
fn stale(path: &Path) -> bool {
    use rustix::net::{AddressFamily, SocketAddrUnix, SocketType};
    let Ok(address) = SocketAddrUnix::new(path) else {
        return false;
    };
    // `socket` then `FIONBIO`: macOS has no `SOCK_NONBLOCK`; the descriptor is closed on return
    let Ok(socket) = rustix::net::socket(AddressFamily::UNIX, SocketType::STREAM, None) else {
        return false;
    };
    if rustix::io::ioctl_fionbio(&socket, true).is_err() {
        return false;
    }
    matches!(
        rustix::net::connect(&socket, &address),
        Err(rustix::io::Errno::CONNREFUSED)
    )
}

impl Drop for Listener {
    fn drop(&mut self) {
        let still_ours = std::fs::symlink_metadata(&self.path)
            .is_ok_and(|meta| (meta.dev(), meta.ino()) == self.identity);
        if still_ours {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Who may use the socket: the daemon's own effective uid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Operators {
    uid: u32,
}

impl Operators {
    /// The daemon's own effective uid.
    #[must_use]
    pub fn this_user() -> Self {
        Self {
            uid: rustix::process::geteuid().as_raw(),
        }
    }

    /// Only `uid` (tests name another uid to see a peer refused).
    #[must_use]
    pub const fn only(uid: u32) -> Self {
        Self { uid }
    }

    /// Whether `peer` may use the socket.
    #[must_use]
    pub const fn allows(self, peer: u32) -> bool {
        peer == self.uid
    }
}

/// What the server reads and writes through: the daemon's store, its host, its build, its
/// start-up sizing, its ready signal and its after-commit follow channel.
pub struct Service<'a, J, C, R, N> {
    /// The store commands go through; its journal is also the projection store.
    pub store: &'a AggregateStore<J, C, R, N>,
    /// This daemon's host.
    pub host: &'a HostId,
    /// This daemon's build version.
    pub version: &'a str,
    /// The sizing in effect at start-up.
    pub sizing: &'a SizingReport,
    /// `true` once the current run reported its ready line.
    pub ready: &'a watch::Sender<bool>,
    /// The store notifier's follow channel (`crate::supervise::Notifier::follow`).
    pub follow: &'a broadcast::Sender<Committed>,
    /// Who may ask.
    pub operators: Operators,
}

impl<J, C, R, N> std::fmt::Debug for Service<'_, J, C, R, N> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Service")
            .field("host", self.host)
            .field("operators", &self.operators)
            .finish_non_exhaustive()
    }
}

type Connection<'a> = Pin<Box<dyn Future<Output = ()> + 'a>>;

/// Polls every connection once; ready when none is left.
async fn drive(connections: &mut Vec<Connection<'_>>) {
    std::future::poll_fn(|cx| {
        connections.retain_mut(|connection| connection.as_mut().poll(cx).is_pending());
        if connections.is_empty() {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    })
    .await;
}

/// Serves `listener` until `stop`, then lets each command in flight finish for at most
/// `crate::daemon::SHUTDOWN_DEADLINE` (an `events` stream ends at once) and drops the rest.
pub async fn serve<J, C, R, N>(
    listener: &Listener,
    service: &Service<'_, J, C, R, N>,
    stop: ShutdownSignal,
) where
    J: Journal + ProjectionStore,
    C: Clock,
    R: Rng,
    N: StoreNotifier,
{
    let mut connections: Vec<Connection<'_>> = Vec::new();
    let mut cancel = stop.clone();
    let mut backoff: Option<tokio::time::Instant> = None;
    loop {
        let accepted = tokio::select! {
            () = cancel.cancelled() => break,
            accepted = listener.inner.accept(), if backoff.is_none() => Some(accepted),
            () = tokio::time::sleep_until(backoff.unwrap_or_else(tokio::time::Instant::now)),
                if backoff.is_some() => {
                backoff = None;
                None
            }
            () = drive(&mut connections), if !connections.is_empty() => None,
        };
        match accepted {
            Some(Ok((stream, _))) if connections.len() < MAX_CONNECTIONS => {
                connections.push(Box::pin(service.connection(stream, stop.clone())));
            }
            Some(Ok((stream, _))) => {
                tracing::warn!(limit = MAX_CONNECTIONS, "operator_connection_refused");
                busy(&stream);
            }
            Some(Err(_)) => {
                tracing::warn!("operator_accept_failed");
                backoff = Some(tokio::time::Instant::now() + ACCEPT_BACKOFF);
            }
            None => {}
        }
    }
    if !connections.is_empty()
        && tokio::time::timeout(crate::daemon::SHUTDOWN_DEADLINE, drive(&mut connections))
            .await
            .is_err()
    {
        tracing::warn!(left = connections.len(), "operator_connections_dropped");
    }
}

/// Answers `busy` to a connection over the limit without waiting on it: one non-blocking
/// write, then the connection is closed.
fn busy(stream: &UnixStream) {
    let answer = Answer::failed(
        "busy",
        format!("mayflyd serves {MAX_CONNECTIONS} connections at once; try again"),
    );
    if let Ok(mut line) = serde_json::to_vec(&answer) {
        line.push(b'\n');
        let _ = stream.try_write(&line);
    }
}

/// Writes one answer line within [`WRITE_DEADLINE`]; `false` when it could not be.
async fn send(write: &mut OwnedWriteHalf, answer: &Answer) -> bool {
    let Ok(mut line) = serde_json::to_vec(answer) else {
        return false;
    };
    line.push(b'\n');
    let written = tokio::time::timeout(WRITE_DEADLINE, async {
        write.write_all(&line).await?;
        write.flush().await
    })
    .await;
    matches!(written, Ok(Ok(())))
}

/// [`send`], abandoned when `stop` comes first: `false` then.
async fn send_unless_stopped(
    write: &mut OwnedWriteHalf,
    answer: &Answer,
    stop: &mut ShutdownSignal,
) -> bool {
    tokio::select! {
        () = stop.cancelled() => false,
        sent = send(write, answer) => sent,
    }
}

/// Reads one request line: `Err` with the answer to send when it is not one.
fn parse(line: &str) -> Result<Request, Answer> {
    let document: Value = serde_json::from_str(line)
        .map_err(|_| Answer::failed("bad_request", "a request is one JSON object per line"))?;
    let api = document.get("api").and_then(Value::as_u64).unwrap_or(0);
    if api == 0 {
        return Err(Answer::failed(
            "bad_request",
            "the request names no \"api\"",
        ));
    }
    if api > u64::from(API) {
        return Err(Answer::failed(
            "unsupported_api",
            format!("this mayflyd speaks API {API}; upgrade mayflyd or use a matching mayfly"),
        ));
    }
    let command = document
        .get("command")
        .and_then(Value::as_str)
        .unwrap_or("");
    if !COMMANDS.contains(&command) {
        return Err(Answer::failed(
            "unknown_command",
            format!("this mayflyd does not know `{command}`; upgrade mayflyd"),
        ));
    }
    serde_json::from_value(document)
        .map_err(|e| Answer::failed("bad_request", format!("the request is malformed: {e}")))
}

fn refused(error: &CommandError) -> Answer {
    match error {
        CommandError::Refused(code) => Answer::failed(code, format!("refused: {code}")),
        CommandError::Save(e) => Answer::failed("save_failed", format!("not decided: {e}")),
    }
}

impl<J, C, R, N> Service<'_, J, C, R, N>
where
    J: Journal + ProjectionStore,
    C: Clock,
    R: Rng,
    N: StoreNotifier,
{
    async fn connection(&self, stream: UnixStream, mut stop: ShutdownSignal) {
        let peer = stream.peer_cred().map(|cred| cred.uid());
        let (read, mut write) = stream.into_split();
        let Ok(peer) = peer else {
            let _ = send(
                &mut write,
                &Answer::failed("unauthorised", "no peer credentials"),
            )
            .await;
            return;
        };
        if !self.operators.allows(peer) {
            tracing::warn!(peer, "operator_peer_refused");
            let _ = send(
                &mut write,
                &Answer::failed(
                    "unauthorised",
                    "only the user mayflyd runs as may use its socket",
                ),
            )
            .await;
            return;
        }
        let mut reader = BufReader::new(read.take(MAX_REQUEST));
        let mut line = String::new();
        let read_line = tokio::time::timeout(REQUEST_DEADLINE, reader.read_line(&mut line)).await;
        if !matches!(read_line, Ok(Ok(n)) if n > 0 && line.ends_with('\n')) {
            let _ = send(
                &mut write,
                &Answer::failed("bad_request", "send one request line, newline-terminated"),
            )
            .await;
            return;
        }
        let request = match parse(line.trim_end()) {
            Ok(request) => request,
            Err(answer) => {
                let _ = send(&mut write, &answer).await;
                return;
            }
        };
        if let Request::Events { follow, since_secs } = request {
            let read = reader.into_inner().into_inner();
            self.events(read, &mut write, follow, since_secs, &mut stop)
                .await;
            return;
        }
        let answer = self.answer(request, peer).await;
        let _ = send(&mut write, &answer).await;
    }

    /// The answer to a request other than `events`, decided under the peer's uid.
    async fn answer(&self, request: Request, peer: u32) -> Answer {
        match request {
            Request::Status => self.status().await,
            Request::PoolList => match queries::fleet(self.store.journal()).await {
                Ok(view) => Answer::served(&PoolList {
                    pools: status::pools(&view),
                }),
                Err(e) => Answer::failed("query_failed", e.to_string()),
            },
            Request::VmList => match queries::fleet(self.store.journal()).await {
                Ok(view) => Answer::served(&VmList {
                    vms: status::vms(&view),
                }),
                Err(e) => Answer::failed("query_failed", e.to_string()),
            },
            Request::Events { .. } => Answer::failed("bad_request", "events streams"),
            mutating => self.command(mutating, peer).await,
        }
    }

    /// The run state, or `None` when it does not read.
    async fn run_state(&self) -> Option<HostRunState> {
        self.store.load::<HostRun>(self.host.as_str()).await.ok()
    }

    async fn status(&self) -> Answer {
        let Some(run) = self.run_state().await else {
            return Answer::failed("query_failed", "the host run does not read");
        };
        let journal = self.store.journal();
        let (view, effects) = match (
            queries::fleet(journal).await,
            queries::pending_effects(journal).await,
        ) {
            (Ok(view), Ok((_, unknown))) => (view, unknown),
            (Err(e), _) | (_, Err(e)) => return Answer::failed("query_failed", e.to_string()),
        };
        Answer::served(&Status {
            host: self.host.as_str().to_owned(),
            version: self.version.to_owned(),
            run: status::run(&run, *self.ready.borrow()),
            sizing: status::sizing(self.sizing),
            unmeasured_images: self.sizing.unmeasured_images.clone(),
            pools: status::pools(&view),
            hosts: status::hosts(&view),
            attempts: status::attempts(&view, effects),
        })
    }

    /// A mutating command, decided under `Actor::Operator { uid: peer }` and the run's epoch.
    async fn command(&self, request: Request, peer: u32) -> Answer {
        let epoch = match self.run_state().await {
            Some(HostRunState {
                phase: RunPhase::Recovering | RunPhase::Operating,
                epoch,
                ..
            }) => epoch,
            Some(_) | None => {
                return Answer::failed(
                    "not_running",
                    "mayflyd has no run that takes commands now; try again once it is ready",
                );
            }
        };
        let actor = Actor::Operator { uid: peer };
        let pool = |name: &str| {
            PoolName::new(name).map_err(|_| Answer::failed("bad_request", "not a pool name"))
        };
        let done = Answer::served(&serde_json::json!({}));
        match request {
            Request::PoolPause { pool: name } => {
                self.pool(pool(&name), PoolAction::Pause, actor, epoch, done)
                    .await
            }
            Request::PoolResume { pool: name } => {
                self.pool(pool(&name), PoolAction::Resume, actor, epoch, done)
                    .await
            }
            Request::PoolDrain { pool: name } => {
                self.pool(pool(&name), PoolAction::Drain, actor, epoch, done)
                    .await
            }
            Request::PoolResetBreaker { pool: name, digest } => {
                let name = match pool(&name) {
                    Ok(name) => name,
                    Err(answer) => return answer,
                };
                let digest = match digest.as_deref().map(ImageDigest::new).transpose() {
                    Ok(digest) => digest,
                    Err(_) => return Answer::failed("bad_request", "not an image digest"),
                };
                match reset_breakers(self.store, &name, digest.as_ref(), actor, epoch).await {
                    Ok(reset) => Answer::served(&Reset {
                        reset: reset.iter().map(|d| d.as_str().to_owned()).collect(),
                    }),
                    Err(e) if e.reset.is_empty() => refused(&e.error),
                    Err(e) => {
                        let mut answer = refused(&e.error);
                        if let Some(failure) = answer.error.as_mut() {
                            let done: Vec<&str> = e.reset.iter().map(ImageDigest::as_str).collect();
                            failure.message = format!(
                                "{}; reset before it: {}",
                                failure.message,
                                done.join(", ")
                            );
                        }
                        answer
                    }
                }
            }
            Request::HostDrain { deadline_secs } => {
                let secs = deadline_secs.unwrap_or(DEFAULT_DRAIN_SECS);
                let within = Millis::new(secs.saturating_mul(1000));
                self.host_action(HostAction::Drain { within }, actor, epoch, done)
                    .await
            }
            Request::HostResume => {
                self.host_action(HostAction::Resume, actor, epoch, done)
                    .await
            }
            Request::Status | Request::VmList | Request::PoolList | Request::Events { .. } => {
                Answer::failed("bad_request", "not a command")
            }
        }
    }

    async fn pool(
        &self,
        name: Result<PoolName, Answer>,
        action: PoolAction,
        actor: Actor,
        epoch: Epoch,
        done: Answer,
    ) -> Answer {
        let name = match name {
            Ok(name) => name,
            Err(answer) => return answer,
        };
        match pool_action(self.store, &name, action, actor, epoch).await {
            Ok(()) => done,
            Err(e) => refused(&e),
        }
    }

    async fn host_action(
        &self,
        action: HostAction,
        actor: Actor,
        epoch: Epoch,
        done: Answer,
    ) -> Answer {
        match host_action(self.store, self.host, action, actor, epoch).await {
            Ok(()) => done,
            Err(e) => refused(&e),
        }
    }

    /// `events`: with `follow`, a [`Following`] line once subscribed; then the history of the
    /// last `since_secs` (at most [`MAX_SINCE_SECS`], every stream read, ordered by
    /// `recorded_at`, the newest [`MAX_HISTORY`]); then, with `follow`, each committed unit's
    /// events as the store notifies them, until the client closes, a line cannot be written in
    /// time or the daemon stops (every send races the stop). Subscribed before the history is
    /// read, so an event committed meanwhile is sent (possibly twice), never missed while the
    /// follower keeps up.
    async fn events(
        &self,
        mut read: tokio::net::unix::OwnedReadHalf,
        write: &mut OwnedWriteHalf,
        follow: bool,
        since_secs: Option<u64>,
        stop: &mut ShutdownSignal,
    ) {
        if !follow && since_secs.is_none() {
            let _ = send(
                write,
                &Answer::failed("bad_request", "events needs --follow or --since"),
            )
            .await;
            return;
        }
        let mut followed = follow.then(|| self.follow.subscribe());
        if followed.is_some()
            && !send_unless_stopped(write, &Answer::served(&Following { following: true }), stop)
                .await
        {
            return;
        }
        if let Some(secs) = since_secs {
            let from = self
                .store
                .clock()
                .now()
                .get()
                .saturating_sub(secs.min(MAX_SINCE_SECS).saturating_mul(1000));
            match self.history(from, stop).await {
                Ok(lines) => {
                    for line in &lines {
                        if !send_unless_stopped(write, &Answer::served(line), stop).await {
                            return;
                        }
                    }
                }
                Err(answer) => {
                    let _ = send_unless_stopped(write, &answer, stop).await;
                    return;
                }
            }
        }
        let Some(followed) = followed.as_mut() else {
            return;
        };
        let mut probe = [0_u8; 1];
        loop {
            let received = tokio::select! {
                () = stop.cancelled() => return,
                // the client closed (or sent something it should not): stop following
                _ = read.read(&mut probe) => return,
                received = followed.recv() => received,
            };
            let lines = match received {
                Ok(commit) => self.committed(&commit).await,
                Err(broadcast::error::RecvError::Lagged(lost)) => {
                    if !send_unless_stopped(write, &Answer::served(&Lagged { lagged: lost }), stop)
                        .await
                    {
                        return;
                    }
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => return,
            };
            for line in &lines {
                if !send_unless_stopped(write, &Answer::served(line), stop).await {
                    return;
                }
            }
        }
    }

    /// The events of one committed unit, in its streams' order.
    async fn committed(&self, commit: &Committed) -> Vec<EventLine> {
        let mut lines = Vec::new();
        for appended in &commit.appended.streams {
            let after = appended.first.get().checked_sub(1).map(StreamRevision::new);
            let read = self
                .store
                .journal()
                .read_stream(&appended.stream, after, Some(appended.last))
                .await;
            match read {
                Ok(events) => lines.extend(events.iter().map(line_of)),
                Err(_) => tracing::warn!("operator_follow_read_failed"),
            }
        }
        lines
    }

    /// The newest [`MAX_HISTORY`] events recorded at or after `from`, ordered by `recorded_at`
    /// then stream and revision. Every stream is read, a page of [`STREAM_PAGE`] at a time,
    /// yielding between pages and stopping early on `stop`; at most [`MAX_HISTORY`] lines are
    /// held at once (a min-heap of the newest), whatever the journal's size. There is no
    /// journal query by time yet (RT-09). The bound is pinned by
    /// `tests::should_keep_the_newest_lines_when_more_than_the_limit_are_pushed`.
    async fn history(&self, from: u64, stop: &ShutdownSignal) -> Result<Vec<EventLine>, Answer> {
        let journal = self.store.journal();
        let mut newest: BinaryHeap<Reverse<Newest>> = BinaryHeap::new();
        let mut after: Option<StreamId> = None;
        loop {
            if stop.is_cancelled() {
                return Ok(Vec::new());
            }
            let heads = journal
                .streams(after.as_ref(), STREAM_PAGE)
                .await
                .map_err(|e| Answer::failed("query_failed", e.to_string()))?;
            let Some(last) = heads.last() else {
                break;
            };
            after = Some(last.stream.clone());
            for head in &heads {
                let events = journal
                    .read_stream(&head.stream, None, None)
                    .await
                    .map_err(|e| Answer::failed("query_failed", e.to_string()))?;
                for event in events.iter().filter(|e| e.recorded_at.get() >= from) {
                    keep(&mut newest, line_of(event), MAX_HISTORY);
                }
            }
            tokio::task::yield_now().await;
        }
        let mut lines: Vec<EventLine> = newest.into_iter().map(|Reverse(n)| n.0).collect();
        lines.sort_by(|a, b| key(a).cmp(&key(b)));
        Ok(lines)
    }
}

/// Pushes `line` into `newest`, then drops the oldest while more than `limit` are held.
fn keep(newest: &mut BinaryHeap<Reverse<Newest>>, line: EventLine, limit: usize) {
    newest.push(Reverse(Newest(line)));
    while newest.len() > limit {
        newest.pop();
    }
}

/// The display order of an event line: `recorded_at`, then stream, then revision.
fn key(line: &EventLine) -> (u64, &str, u64) {
    (line.recorded_at, line.stream.as_str(), line.revision)
}

/// An event line ordered by [`key`], for the history's heap.
struct Newest(EventLine);

impl PartialEq for Newest {
    fn eq(&self, other: &Self) -> bool {
        key(&self.0) == key(&other.0)
    }
}

impl Eq for Newest {}

impl PartialOrd for Newest {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Newest {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        key(&self.0).cmp(&key(&other.0))
    }
}

fn line_of(event: &mayfly_control::journal::RecordedEvent) -> EventLine {
    EventLine {
        recorded_at: event.recorded_at.get(),
        stream: event.stream.as_str().to_owned(),
        revision: event.revision.get(),
        kind: event.kind.event_type.clone(),
        schema_version: event.kind.schema_version,
        payload: serde_json::from_str(event.payload.as_str())
            .unwrap_or_else(|_| Value::String(event.payload.as_str().to_owned())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(recorded_at: u64) -> EventLine {
        EventLine {
            recorded_at,
            stream: "pool-mac".to_owned(),
            revision: recorded_at,
            kind: "pool_paused".to_owned(),
            schema_version: 1,
            payload: Value::Null,
        }
    }

    #[test]
    fn should_keep_the_newest_lines_when_more_than_the_limit_are_pushed() {
        // given
        let mut newest = BinaryHeap::new();

        // when: ten lines out of order through a heap of three
        for at in [5, 1, 9, 3, 7, 10, 2, 8, 4, 6] {
            keep(&mut newest, line(at), 3);
        }

        // then
        let mut kept: Vec<u64> = newest
            .into_iter()
            .map(|Reverse(n)| n.0.recorded_at)
            .collect();
        kept.sort_unstable();
        assert_eq!(kept, [8, 9, 10]);
    }
}
