//! The `mayfly` CLI's end of the operator socket: blocking, one request per connection (the
//! CLI only parses and delegates). Before it sends anything it checks who serves the socket:
//! the peer must run as this user or as root, so a request (and what it reveals) never goes to
//! a process another user put at the path. Reading the peer's uid portably (`SO_PEERCRED` on
//! Linux, `getpeereid` on macOS) is tokio's, so a one-shot current-thread runtime does it.
//! Pinned by `tests::should_trust_only_this_user_or_root_when_a_server_answers`; the check runs
//! on every connection of `crates/mayfly-cli/tests/operator.rs`.

use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::wire::{Answer, Request};

/// How long the CLI waits for a one-line answer.
pub const ANSWER_DEADLINE: Duration = Duration::from_secs(60);

/// Why the daemon gave no answer.
#[derive(Debug)]
pub enum ClientError {
    /// Nothing answers at the socket: mayflyd is not running, or runs with another state.
    Unreachable {
        /// The socket tried.
        path: PathBuf,
        /// The operating system's reason.
        source: std::io::Error,
    },
    /// The connection failed after it was made.
    Io(std::io::Error),
    /// The daemon sent something that is not an answer.
    Protocol(String),
    /// The process serving the socket runs as another user than this one or root.
    NotDaemon {
        /// The socket.
        path: PathBuf,
        /// The serving process's uid.
        uid: u32,
    },
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unreachable { path, source } => write!(
                f,
                "cannot reach mayflyd at {}: {source}; is it running, with this configuration? \
                 (--config <path> or --socket <path>)",
                path.display()
            ),
            Self::Io(e) => write!(f, "the connection to mayflyd failed: {e}"),
            Self::Protocol(what) => write!(f, "mayflyd answered something unreadable: {what}"),
            Self::NotDaemon { path, uid } => write!(
                f,
                "refusing to talk to {}: it is served by uid {uid}, neither this user nor root",
                path.display()
            ),
        }
    }
}

impl std::error::Error for ClientError {}

/// The uid of the process serving `stream`.
fn server_uid(stream: UnixStream) -> std::io::Result<(UnixStream, u32)> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .build()?;
    stream.set_nonblocking(true)?;
    let (stream, uid) = runtime.block_on(async {
        let stream = tokio::net::UnixStream::from_std(stream)?;
        let uid = stream.peer_cred()?.uid();
        Ok::<_, std::io::Error>((stream.into_std()?, uid))
    })?;
    stream.set_nonblocking(false)?;
    Ok((stream, uid))
}

/// Whether a server running as `uid` may be talked to by this process: it runs as this user
/// or as root.
#[must_use]
pub fn trusted_server(uid: u32, own: u32) -> bool {
    uid == own || uid == 0
}

fn connect(path: &Path, request: &Request) -> Result<UnixStream, ClientError> {
    let stream = UnixStream::connect(path).map_err(|source| ClientError::Unreachable {
        path: path.to_path_buf(),
        source,
    })?;
    let (mut stream, uid) = server_uid(stream).map_err(ClientError::Io)?;
    if !trusted_server(uid, rustix::process::geteuid().as_raw()) {
        return Err(ClientError::NotDaemon {
            path: path.to_path_buf(),
            uid,
        });
    }
    let mut line = serde_json::to_vec(&request.to_document())
        .map_err(|e| ClientError::Protocol(e.to_string()))?;
    line.push(b'\n');
    stream.write_all(&line).map_err(ClientError::Io)?;
    stream.flush().map_err(ClientError::Io)?;
    Ok(stream)
}

fn answer_of(line: &str) -> Result<Answer, ClientError> {
    serde_json::from_str(line).map_err(|e| ClientError::Protocol(e.to_string()))
}

/// Sends `request` to the daemon at `path` and reads its one answer.
///
/// # Errors
/// [`ClientError`].
///
/// # Panics
/// When called from inside a Tokio runtime: the server's uid is read on a runtime of its own,
/// which cannot be entered from another (the CLI calls this from plain `main`).
pub fn request(path: &Path, request: &Request) -> Result<Answer, ClientError> {
    let stream = connect(path, request)?;
    stream
        .set_read_timeout(Some(ANSWER_DEADLINE))
        .map_err(ClientError::Io)?;
    let mut line = String::new();
    BufReader::new(stream)
        .read_line(&mut line)
        .map_err(ClientError::Io)?;
    if line.is_empty() {
        return Err(ClientError::Protocol("the connection closed".to_owned()));
    }
    answer_of(line.trim_end())
}

/// Sends `request` (an `events` request) and hands each answer line to `each` until the daemon
/// closes the stream or `each` returns `false`.
///
/// # Errors
/// [`ClientError`].
///
/// # Panics
/// When called from inside a Tokio runtime, as [`request`].
pub fn stream(
    path: &Path,
    request: &Request,
    mut each: impl FnMut(Answer) -> bool,
) -> Result<(), ClientError> {
    let stream = connect(path, request)?;
    for line in BufReader::new(stream).lines() {
        let line = line.map_err(ClientError::Io)?;
        if !each(answer_of(&line)?) {
            break;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::trusted_server;

    #[test]
    fn should_trust_only_this_user_or_root_when_a_server_answers() {
        // given
        let own = 501;

        // when
        let verdicts = [
            trusted_server(501, own),
            trusted_server(0, own),
            trusted_server(502, own),
        ];

        // then
        assert_eq!(verdicts, [true, true, false]);
    }
}
