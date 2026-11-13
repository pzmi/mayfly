//! The operator socket's server (`mayfly_composition::operator::server`, RT-03) over the
//! in-memory journal (component level): a real Unix socket in a tempdir, the server and a
//! client polled together on one current-thread runtime, nothing spawned.

use std::os::unix::fs::{FileTypeExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use mayfly_composition::operator::server::{
    BindError, Listener, MAX_CONNECTIONS, MAX_REQUEST, Operators, REQUEST_DEADLINE,
    SOCKET_PATH_MAX, Service, serve,
};
use mayfly_composition::operator::wire::{
    Answer, EventLine, PoolList, Request, Reset, Status, VmList,
};
use mayfly_composition::sizing::SizingReport;
use mayfly_composition::supervise::{Notifier, Shutdown};
use mayfly_control::journal::Journal as _;
use mayfly_control::projection::control;
use mayfly_control::store::{AggregateStore, Outcome, SaveRequest, decode_event, stream_of};
use mayfly_control_domain::attempt::GuestOs;
use mayfly_control_domain::operations::hostrun::{HostRun, HostRunCommand, Verification};
use mayfly_control_domain::pool::{
    ConfigurePool, Pool, PoolCommand, PoolEvent, TripReason, TrustTier,
};
use mayfly_control_domain::{Actor, Aggregate, Command, ControlAggregates};
use mayfly_domain::{CommandId, Epoch, HostId, ImageDigest, Millis, Resources, Sample as _};
use mayfly_testkit::clock::{FakeClock, SeededRng};
use mayfly_testkit::journal::MemoryJournal;
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::UnixStream;

type Store = AggregateStore<MemoryJournal, FakeClock, SeededRng, Notifier>;

const T0: Millis = Millis::new(1_700_000_000_000);

/// A store with the control projections, its notifier, and a host whose run minted epoch 1,
/// with pool `mac` configured and its sample digest's breaker tripped.
struct Kit {
    store: Store,
    notifier: Notifier,
    host: HostId,
    sizing: SizingReport,
    ready: tokio::sync::watch::Sender<bool>,
    dir: tempfile::TempDir,
    next: std::cell::Cell<u128>,
}

impl Kit {
    fn new() -> Self {
        let kit = Self::without_run();
        kit.start_run();
        kit
    }

    /// The kit's store and socket with nothing written.
    fn without_run() -> Self {
        let notifier = Notifier::new();
        let store = AggregateStore::new::<ControlAggregates>(
            MemoryJournal::new(),
            FakeClock::at(T0),
            SeededRng::new(3),
            notifier.clone(),
        )
        .unwrap_or_else(|e| panic!("{e}"))
        .with_projections(control().unwrap_or_else(|e| panic!("{e}")));
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))
            .unwrap_or_else(|e| panic!("{e}"));
        Self {
            store,
            notifier,
            host: HostId::sample(),
            sizing: SizingReport {
                values: Vec::new(),
                unmeasured_images: Vec::new(),
            },
            ready: tokio::sync::watch::channel(true).0,
            dir,
            next: std::cell::Cell::new(1),
        }
    }

    /// A run that minted epoch 1, pool `mac` configured and its sample digest's breaker tripped.
    fn start_run(&self) {
        self.save::<HostRun>(
            self.host.as_str(),
            HostRunCommand::StartRun {
                boot_id: "boot".to_owned(),
                version: "0.1.0".to_owned(),
            },
        );
        self.save::<HostRun>(
            self.host.as_str(),
            HostRunCommand::MintEpoch {
                floor: Epoch::new(0),
                verification: Verification::Verified,
                crash_loop: false,
            },
        );
        self.save::<Pool>(
            "mac",
            PoolCommand::ConfigurePool(ConfigurePool {
                revision: 1,
                tier: TrustTier::Trusted,
                os: GuestOs::Linux,
                vm_resources: Resources::new(1, 1),
                max_running: 2,
                min_prepared: Some(0),
                breaker_threshold: 1,
                breaker_window: Millis::new(60_000),
            }),
        );
        self.save::<Pool>(
            "mac",
            PoolCommand::TripBreaker {
                digest: ImageDigest::sample(),
                reason: TripReason::Failures,
                at: T0,
            },
        );
    }

    fn save<A: Aggregate>(&self, instance: &str, body: A::Body) {
        let id = self.next.get();
        self.next.set(id + 1);
        let command = Command::new(
            CommandId::from_u128(id),
            T0,
            Epoch::new(1),
            Actor::Startup,
            body,
        );
        let saved =
            mayfly_testkit::block_on(self.store.execute::<A>(SaveRequest::new(instance, command)));
        assert!(
            matches!(saved, Ok(Outcome::Accepted { .. })),
            "{instance}: {saved:?}"
        );
    }

    fn socket(&self) -> PathBuf {
        self.dir.path().join("mayfly.sock")
    }

    /// Serves with `operators` while `client` runs, then stops the server.
    fn with_server<F, T>(&self, operators: Operators, client: impl FnOnce(PathBuf) -> F) -> T
    where
        F: std::future::Future<Output = T>,
    {
        self.serving(false, operators, client).0
    }

    /// [`Kit::with_server`] on a runtime whose clock is paused when `paused` (it jumps to the
    /// next timer whenever nothing else can run), with the runtime's time from the stop to
    /// `serve`'s return.
    fn serving<F, T>(
        &self,
        paused: bool,
        operators: Operators,
        client: impl FnOnce(PathBuf) -> F,
    ) -> (T, std::time::Duration)
    where
        F: std::future::Future<Output = T>,
    {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .start_paused(paused)
            .build()
            .unwrap_or_else(|e| panic!("{e}"));
        runtime.block_on(async {
            let listener = Listener::bind(&self.socket()).unwrap_or_else(|e| panic!("{e}"));
            let service = Service {
                store: &self.store,
                host: &self.host,
                version: "0.1.0",
                sizing: &self.sizing,
                ready: &self.ready,
                follow: &self.notifier.follow,
                operators,
            };
            let stop = Shutdown::new();
            let script = async {
                let out = client(self.socket()).await;
                stop.cancel();
                (out, tokio::time::Instant::now())
            };
            let ((), (out, stopped)) =
                tokio::join!(serve(&listener, &service, stop.signal()), script);
            (out, stopped.elapsed())
        })
    }
}

async fn ask(socket: &Path, line: &str) -> Answer {
    let mut stream = UnixStream::connect(socket)
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    stream
        .write_all(format!("{line}\n").as_bytes())
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    let mut answer = String::new();
    BufReader::new(stream)
        .read_line(&mut answer)
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    serde_json::from_str(&answer).unwrap_or_else(|e| panic!("{e}: {answer:?}"))
}

fn code(answer: &Answer) -> Option<&str> {
    answer.error.as_ref().map(|e| e.code.as_str())
}

#[test]
fn should_create_the_socket_private_when_the_server_binds() {
    // given
    let kit = Kit::new();

    // when
    let mode = kit.with_server(Operators::this_user(), |socket| async move {
        let meta = std::fs::symlink_metadata(&socket).unwrap_or_else(|e| panic!("{e}"));
        (
            meta.file_type().is_socket(),
            meta.permissions().mode() & 0o777,
        )
    });

    // then: a socket, readable and writable by its owner only, removed once the server stopped
    assert_eq!(mode, (true, 0o600));
    assert!(!kit.socket().exists());
}

#[test]
fn should_refuse_every_request_when_the_peer_is_not_an_operator() {
    // given: a server that allows only another uid than this process's
    let kit = Kit::new();
    let other = rustix::process::geteuid().as_raw().wrapping_add(1);

    // when
    let (status, pause) = kit.with_server(Operators::only(other), |socket| async move {
        (
            ask(&socket, r#"{"api":1,"command":"status"}"#).await,
            ask(&socket, r#"{"api":1,"command":"pool_pause","pool":"mac"}"#).await,
        )
    });

    // then
    assert_eq!(code(&status), Some("unauthorised"));
    assert_eq!(code(&pause), Some("unauthorised"));
    let pool =
        mayfly_testkit::block_on(kit.store.load::<Pool>("mac")).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(pool.mode, mayfly_control_domain::pool::PoolMode::Active);
}

#[test]
fn should_decide_under_the_peers_uid_when_the_body_claims_another_operator() {
    // given: a request naming an operator uid of its own choosing
    let kit = Kit::new();
    let claimed = r#"{"api":1,"command":"pool_reset_breaker","pool":"mac","actor":{"operator":{"uid":4242}},"uid":4242}"#;

    // when
    let answer = kit.with_server(Operators::this_user(), |socket| async move {
        ask(&socket, claimed).await
    });

    // then: decided, and recorded under the peer's uid read from the connection
    assert!(answer.ok, "{answer:?}");
    let stream = stream_of::<Pool>("mac").unwrap_or_else(|e| panic!("{e}"));
    let uids: Vec<u32> =
        mayfly_testkit::block_on(kit.store.journal().read_stream(&stream, None, None))
            .unwrap_or_else(|e| panic!("{e}"))
            .iter()
            .filter_map(|e| match decode_event::<PoolEvent>(e) {
                Ok(PoolEvent::PoolBreakerReset(reset)) => Some(reset.operator_uid),
                _ => None,
            })
            .collect();
    assert_eq!(uids, [rustix::process::geteuid().as_raw()]);
}

#[test]
fn should_ignore_an_unknown_field_when_a_newer_client_sends_one() {
    // given
    let kit = Kit::new();

    // when
    let answer = kit.with_server(Operators::this_user(), |socket| async move {
        ask(
            &socket,
            r#"{"api":1,"command":"status","colour":"blue","verbose":true}"#,
        )
        .await
    });

    // then
    assert!(answer.ok, "{answer:?}");
    let status: Status = answer
        .result
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_else(|| panic!("a status"));
    assert_eq!(status.run.number, 1);
    assert_eq!(status.run.epoch, 1);
    assert_eq!(status.run.phase, "recovering");
    assert!(status.run.ready);
    assert_eq!(status.pools.len(), 1);
    assert_eq!(status.pools[0].name, "mac");
    assert_eq!(status.pools[0].tripped.len(), 1);
}

#[test]
fn should_answer_unknown_command_when_the_daemon_does_not_know_it() {
    // given
    let kit = Kit::new();

    // when
    let (unknown, newer, garbage, no_api) =
        kit.with_server(Operators::this_user(), |socket| async move {
            (
                ask(&socket, r#"{"api":1,"command":"attempt_assert"}"#).await,
                ask(&socket, r#"{"api":2,"command":"status"}"#).await,
                ask(&socket, "status please").await,
                ask(&socket, r#"{"command":"status"}"#).await,
            )
        });

    // then
    assert_eq!(code(&unknown), Some("unknown_command"));
    assert_eq!(code(&newer), Some("unsupported_api"));
    assert_eq!(code(&garbage), Some("bad_request"));
    assert_eq!(code(&no_api), Some("bad_request"));
}

#[test]
fn should_send_each_committed_event_when_a_client_follows() {
    // given
    let kit = Kit::new();
    let request = Request::Events {
        follow: true,
        since_secs: None,
    };
    let line = serde_json::to_string(&request.to_document()).unwrap_or_else(|e| panic!("{e}"));

    // when: a follower is connected, then an operator pauses the pool
    let followed = kit.with_server(Operators::this_user(), |socket| async move {
        let mut follower = UnixStream::connect(&socket)
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        follower
            .write_all(format!("{line}\n").as_bytes())
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        // the documented first line: the daemon subscribed, so what commits next is sent
        let mut follower = BufReader::new(follower);
        let mut following = String::new();
        follower
            .read_line(&mut following)
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        let paused = ask(&socket, r#"{"api":1,"command":"pool_pause","pool":"mac"}"#).await;
        let mut first = String::new();
        follower
            .read_line(&mut first)
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        (following, paused, first)
    });

    // then
    let (following, paused, first) = followed;
    let following: Answer =
        serde_json::from_str(&following).unwrap_or_else(|e| panic!("{e}: {following}"));
    assert_eq!(
        following.result,
        Some(serde_json::json!({"following": true}))
    );
    assert!(paused.ok, "{paused:?}");
    let answer: Answer = serde_json::from_str(&first).unwrap_or_else(|e| panic!("{e}: {first}"));
    let event: EventLine = answer
        .result
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_else(|| panic!("an event: {first}"));
    assert_eq!(event.stream, "pool-mac");
    assert_eq!(event.kind, "pool_paused");
}

/// Connects, sends `request` and reads the `{"following":true}` line.
async fn follow(socket: &Path) -> BufReader<UnixStream> {
    let line = serde_json::to_string(
        &Request::Events {
            follow: true,
            since_secs: None,
        }
        .to_document(),
    )
    .unwrap_or_else(|e| panic!("{e}"));
    let mut follower = UnixStream::connect(socket)
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    follower
        .write_all(format!("{line}\n").as_bytes())
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    let mut follower = BufReader::new(follower);
    let mut first = String::new();
    follower
        .read_line(&mut first)
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    assert!(first.contains(r#""following":true"#), "{first}");
    follower
}

/// Negative control (run at RT-03's review): with the stop race taken out of the server's
/// `send_unless_stopped`, the server waits for its write deadline, and this test fails with 10 s
/// of paused time passed after the stop.
#[test]
fn should_return_when_stopped_while_a_follower_never_reads() {
    // given: a follower that stops reading once it followed
    let kit = Kit::new();
    let saver = &kit;

    // when: more events commit than the socket can buffer, then the server is stopped (the
    // script ends, which stops it). The clock is paused, so a wait on any timer (the write
    // deadline, the post-stop bound) would show as time passed after the stop
    let (held, after_stop) = kit.serving(true, Operators::this_user(), |socket| async move {
        let follower = follow(&socket).await;
        // one new pool stream per unit keeps each save cheap; 800 lines (~300 KiB) overflow the socket buffer
        for n in 0..800_u32 {
            saver.save::<Pool>(
                &format!("p{n}"),
                PoolCommand::ConfigurePool(ConfigurePool {
                    revision: 1,
                    tier: TrustTier::Trusted,
                    os: GuestOs::Linux,
                    vm_resources: Resources::new(1, 1),
                    max_running: 2,
                    min_prepared: Some(0),
                    breaker_threshold: 1,
                    breaker_window: Millis::new(60_000),
                }),
            );
            if n % 50 == 0 {
                // let the server write until the follower's buffer is full
                tokio::task::yield_now().await;
            }
        }
        follower
    });

    // then: the server returned at the stop, without waiting on any deadline, although the
    // follower still holds its connection unread
    assert_eq!(after_stop, std::time::Duration::ZERO);
    drop(held);
}

#[test]
fn should_answer_busy_when_every_connection_slot_is_taken() {
    // given
    let kit = Kit::new();

    // when: every slot held by a connection that sends nothing, then one more
    let busy = kit.with_server(Operators::this_user(), |socket| async move {
        let mut held = Vec::new();
        for _ in 0..MAX_CONNECTIONS {
            held.push(
                UnixStream::connect(&socket)
                    .await
                    .unwrap_or_else(|e| panic!("{e}")),
            );
        }
        // the server accepts in arrival order, so this one finds every slot taken
        let extra = UnixStream::connect(&socket)
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        let mut answer = String::new();
        BufReader::new(extra)
            .read_line(&mut answer)
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        drop(held);
        answer
    });

    // then
    let answer: Answer = serde_json::from_str(&busy).unwrap_or_else(|e| panic!("{e}: {busy}"));
    assert_eq!(code(&answer), Some("busy"));
}

#[test]
fn should_refuse_a_request_when_it_is_longer_than_the_limit() {
    // given: a line one byte over the limit, then its newline
    let kit = Kit::new();
    let long = format!(
        "{}\n",
        "a".repeat(usize::try_from(MAX_REQUEST).unwrap_or(usize::MAX) + 1)
    );

    // when
    let answer = kit.with_server(Operators::this_user(), |socket| async move {
        let mut stream = UnixStream::connect(&socket)
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        // the server stops reading at the limit, answers and closes: the rest of this write may
        // meet the closed end (EPIPE, or EPROTOTYPE on macOS, which no stable error kind names),
        // which is the documented behaviour, so its result is ignored; the answer is already in
        // this end's receive buffer either way
        let _ = stream.write_all(long.as_bytes()).await;
        let mut answer = String::new();
        // well under the request deadline, so only the limit (never the deadline) answers
        mayfly_testkit_shared::poll::within(
            REQUEST_DEADLINE / 4,
            "the server's answer to the oversized request",
            BufReader::new(stream).read_line(&mut answer),
        )
        .await
        .unwrap_or_else(|e| panic!("{e}"))
        .unwrap_or_else(|e| panic!("{e}"));
        answer
    });

    // then
    let answer: Answer = serde_json::from_str(&answer).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(code(&answer), Some("bad_request"));
}

#[test]
fn should_refuse_a_request_when_it_does_not_arrive_in_time() {
    // given: a paused clock, so the deadline passes as soon as nothing else can run
    let kit = Kit::new();

    // when: a connection that sends nothing
    let ((answer, waited), _) = kit.serving(true, Operators::this_user(), |socket| async move {
        let started = tokio::time::Instant::now();
        let stream = UnixStream::connect(&socket)
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        let mut answer = String::new();
        BufReader::new(stream)
            .read_line(&mut answer)
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        (answer, started.elapsed())
    });

    // then: refused once the deadline passed, not before
    let answer: Answer = serde_json::from_str(&answer).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(code(&answer), Some("bad_request"));
    assert!(waited >= REQUEST_DEADLINE, "{waited:?}");
}

#[test]
fn should_replace_a_stale_socket_and_refuse_anything_else_when_binding() {
    // given: a stale socket (its listener gone), a live one, a plain file and a link
    let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))
        .unwrap_or_else(|e| panic!("{e}"));
    let at = |name: &str| dir.path().join(name);
    drop(
        std::os::unix::net::UnixListener::bind(at("stale.sock")).unwrap_or_else(|e| panic!("{e}")),
    );
    let _live =
        std::os::unix::net::UnixListener::bind(at("live.sock")).unwrap_or_else(|e| panic!("{e}"));
    std::fs::write(at("file.sock"), "not a socket").unwrap_or_else(|e| panic!("{e}"));
    std::os::unix::fs::symlink(at("stale.sock"), at("link.sock")).unwrap_or_else(|e| panic!("{e}"));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap_or_else(|e| panic!("{e}"));

    // when
    let outcomes = runtime.block_on(async {
        ["stale.sock", "live.sock", "file.sock", "link.sock"].map(|name| {
            match Listener::bind(&at(name)) {
                Ok(_) => "bound",
                Err(BindError::Occupied(_)) => "occupied",
                Err(_) => "other",
            }
        })
    });

    // then
    assert_eq!(outcomes, ["bound", "occupied", "occupied", "occupied"]);
    assert!(at("file.sock").is_file());
}

#[test]
fn should_refuse_a_socket_path_when_it_is_too_long() {
    // given
    let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let path = dir.path().join("s".repeat(SOCKET_PATH_MAX));

    // when
    let bound = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap_or_else(|e| panic!("{e}"))
        .block_on(async { Listener::bind(&path).map(|_| ()) });

    // then
    assert!(matches!(bound, Err(BindError::TooLong(_))), "{bound:?}");
}

#[test]
fn should_answer_each_request_by_its_command_when_the_run_takes_commands() {
    // given: the kit's pool `mac` with the sample digest's breaker tripped, and its host not
    // registered (no data plane reported)
    let kit = Kit::new();
    let digest = ImageDigest::sample();
    let reset = format!(
        r#"{{"api":1,"command":"pool_reset_breaker","pool":"mac","digest":"{}"}}"#,
        digest.as_str()
    );

    // when
    let answers = kit.with_server(Operators::this_user(), |socket| async move {
        let mut answers = Vec::new();
        for line in [
            r#"{"api":1,"command":"pool_list"}"#,
            r#"{"api":1,"command":"vm_list"}"#,
            r#"{"api":1,"command":"pool_resume","pool":"mac"}"#,
            r#"{"api":1,"command":"pool_drain","pool":"mac"}"#,
            r#"{"api":1,"command":"pool_pause","pool":"Not A Pool!"}"#,
            r#"{"api":1,"command":"pool_reset_breaker","pool":"mac","digest":"sha256:nope"}"#,
            reset.as_str(),
            r#"{"api":1,"command":"pool_reset_breaker","pool":"mac"}"#,
            r#"{"api":1,"command":"host_drain","deadline_secs":60}"#,
            r#"{"api":1,"command":"host_resume"}"#,
            r#"{"api":1,"command":"events"}"#,
            r#"{"api":1,"command":"events","since_secs":3600}"#,
        ] {
            answers.push(ask(&socket, line).await);
        }
        answers
    });

    // then
    let codes: Vec<Option<&str>> = answers.iter().map(code).collect();
    assert_eq!(
        codes,
        [
            None,
            None,
            Some("not_paused"),
            None,
            Some("bad_request"),
            Some("bad_request"),
            None,
            Some("not_tripped"),
            Some("not_registered"),
            Some("not_registered"),
            Some("bad_request"),
            None,
        ]
    );
    let pools: PoolList = answers[0]
        .result
        .clone()
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_else(|| panic!("a pool list"));
    assert_eq!(pools.pools.len(), 1);
    assert_eq!(pools.pools[0].mode, "active");
    let vms: VmList = answers[1]
        .result
        .clone()
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_else(|| panic!("a vm list"));
    assert!(vms.vms.is_empty());
    let reset: Reset = answers[6]
        .result
        .clone()
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_else(|| panic!("a reset"));
    assert_eq!(reset.reset, [digest.as_str()]);
    let history: EventLine = answers[11]
        .result
        .clone()
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_else(|| panic!("an event"));
    assert!(history.stream.starts_with("hostrun-"), "{history:?}");
}

#[test]
fn should_answer_not_running_when_no_run_minted_an_epoch() {
    // given: a store with no run at all
    let kit = Kit::without_run();

    // when
    let (status, pause) = kit.with_server(Operators::this_user(), |socket| async move {
        (
            ask(&socket, r#"{"api":1,"command":"status"}"#).await,
            ask(&socket, r#"{"api":1,"command":"pool_pause","pool":"mac"}"#).await,
        )
    });

    // then: the status reads, the command is refused
    assert!(status.ok, "{status:?}");
    assert_eq!(code(&pause), Some("not_running"));
}
