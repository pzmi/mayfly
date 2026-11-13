//! `operate` and `doctor` in process (unit level), against a canned server: a Unix socket in a
//! tempdir served by a test thread that answers each connection with the lines it was given,
//! so every rendering and exit code is pinned without a daemon (the black-box tests in
//! `tests/operator.rs` run the real one).

use std::collections::BTreeMap;
use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;

use mayfly_composition::operator::wire::{
    self, Answer, AttemptStatus, Desync, EventLine, Following, HostStatus, Lagged, PoolList,
    PoolOnHost, PoolStatus, Request, Reset, RunStatus, SizingValue, Status, Tripped, Value, Vm,
    VmList,
};
use mayfly_composition::paths::PlatformEnv;

use super::{BUSY, REFUSED, Target, UNAVAILABLE, USAGE, doctor, operate};

/// A socket that answers its n-th connection with the n-th list of lines, then closes it.
struct Canned {
    _dir: tempfile::TempDir,
    socket: PathBuf,
    thread: Option<std::thread::JoinHandle<Vec<String>>>,
}

impl Canned {
    fn new(answers: Vec<Vec<String>>) -> Self {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let socket = dir.path().join("mayfly.sock");
        let listener = UnixListener::bind(&socket).unwrap_or_else(|e| panic!("{e}"));
        let thread = std::thread::spawn(move || {
            let mut requests = Vec::new();
            for lines in answers {
                let (stream, _) = listener.accept().unwrap_or_else(|e| panic!("{e}"));
                let mut reader = BufReader::new(stream);
                let mut request = String::new();
                let _ = reader.read_line(&mut request);
                requests.push(request);
                let mut stream = reader.into_inner();
                for line in lines {
                    let _ = writeln!(stream, "{line}");
                }
            }
            requests
        });
        Self {
            _dir: dir,
            socket,
            thread: Some(thread),
        }
    }

    fn target(&self) -> Target {
        Target {
            socket: Some(self.socket.clone()),
            config: None,
        }
    }

    /// The request lines the server read.
    fn requests(mut self) -> Vec<String> {
        self.thread
            .take()
            .map(|t| t.join().unwrap_or_else(|_| panic!("the canned server")))
            .unwrap_or_default()
    }
}

fn line(answer: &Answer) -> String {
    wire::compact(answer)
}

/// The served answer line of a result.
macro_rules! served {
    ($result:expr) => {
        line(&Answer::served(&$result))
    };
}

fn failed(code: &str) -> String {
    line(&Answer::failed(code, format!("refused: {code}")))
}

fn value(text: &str) -> Value {
    text.parse().unwrap_or_else(|e| panic!("{e}"))
}

fn status() -> Status {
    Status {
        host: "h1".to_owned(),
        version: "9.9.9".to_owned(),
        run: RunStatus {
            number: 2,
            epoch: 3,
            phase: "operating".to_owned(),
            ready: false,
            safe_mode: Some("epoch_below_floor".to_owned()),
        },
        sizing: vec![
            SizingValue {
                key: "vm_slots".to_owned(),
                value: "unknown".to_owned(),
                source: "measured".to_owned(),
                reason: Some("cpus_unreadable".to_owned()),
            },
            SizingValue {
                key: "macos_guests".to_owned(),
                value: "2".to_owned(),
                source: "apple_limit".to_owned(),
                reason: None,
            },
        ],
        unmeasured_images: vec!["sha256:ab".to_owned()],
        pools: vec![PoolStatus {
            name: "mac".to_owned(),
            mode: "active".to_owned(),
            image: None,
            max_running: Some(2),
            min_prepared: Some(1),
            tripped: vec![Tripped {
                digest: "sha256:cd".to_owned(),
                at: 1,
            }],
            hosts: vec![PoolOnHost {
                host: "h1".to_owned(),
                terms_open: false,
                placement: None,
                leases: 0,
            }],
        }],
        hosts: vec![HostStatus {
            host: "h1".to_owned(),
            admission_open: false,
            drain: "draining".to_owned(),
            leases: 1,
        }],
        attempts: AttemptStatus {
            by_phase: BTreeMap::new(),
            unknown_effects: 1,
            desync: vec![Desync {
                attempt: "a1".to_owned(),
                pool: "mac".to_owned(),
                vm: "mf-1".to_owned(),
                discovered_by: "inventory".to_owned(),
                note: "vanished".to_owned(),
            }],
        },
    }
}

fn run(target: &Target, request: &Request, json: bool) -> (u8, String, String) {
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = operate(
        target,
        request,
        json,
        &PlatformEnv::default(),
        &mut out,
        &mut err,
    );
    (
        code,
        String::from_utf8_lossy(&out).into_owned(),
        String::from_utf8_lossy(&err).into_owned(),
    )
}

#[test]
fn should_print_each_answer_for_a_person_when_the_daemon_serves_it() {
    // given
    let vm = Vm {
        attempt: "a1".to_owned(),
        pool: "mac".to_owned(),
        host: "h1".to_owned(),
        vm: "mf-1".to_owned(),
        phase: "lost".to_owned(),
        pending: Some("stop_vm".to_owned()),
        pending_unknown: true,
        job: None,
        desync: Some("stop_vm".to_owned()),
        note: Some("vanished".to_owned()),
    };
    let canned = Canned::new(vec![
        vec![served!(status())],
        vec![served!(PoolList {
            pools: status().pools,
        })],
        vec![served!(VmList { vms: vec![vm] })],
        vec![served!(VmList { vms: Vec::new() })],
        vec![served!(Reset {
            reset: vec!["sha256:cd".to_owned()],
        })],
        vec![served!(value("{}"))],
        vec![served!(value("{}"))],
        vec![served!(value("{}"))],
        vec![served!(value("{}"))],
        vec![served!(value("{}"))],
        vec![served!(value(r#"{"unexpected": true}"#))],
    ]);
    let target = canned.target();
    let pool = || "mac".to_owned();

    // when
    let printed: Vec<(u8, String, String)> = [
        Request::Status,
        Request::PoolList,
        Request::VmList,
        Request::VmList,
        Request::PoolResetBreaker {
            pool: pool(),
            digest: None,
        },
        Request::PoolPause { pool: pool() },
        Request::PoolResume { pool: pool() },
        Request::PoolDrain { pool: pool() },
        Request::HostDrain {
            deadline_secs: None,
        },
        Request::HostResume,
        Request::Status,
    ]
    .iter()
    .map(|request| run(&target, request, false))
    .collect();

    // then
    assert!(printed.iter().all(|(code, _, _)| *code == 0), "{printed:?}");
    let out: Vec<&str> = printed.iter().map(|(_, out, _)| out.as_str()).collect();
    assert!(
        out[0].starts_with(
            "host h1  mayflyd 9.9.9  run 2 epoch 3 operating not ready  SAFE MODE (epoch_below_floor)\n"
        ),
        "{}",
        out[0]
    );
    for line in [
        "pool mac: active, image none, breaker tripped for sha256:cd\n",
        "  on host h1: terms closed, placement none, leases 0\n",
        "host h1: admission closed, drain draining, leases 1\n",
        "attempts: none; unknown effects 1\n",
        "desync: attempt a1 (pool mac, vm mf-1): vanished\n",
        "sizing: vm_slots=unknown (measured, cpus_unreadable), macos_guests=2 (apple_limit)\n",
        "sizing: image sha256:ab not measured (its disk is unknown)\n",
    ] {
        assert!(out[0].contains(line), "{line} in {}", out[0]);
    }
    assert!(out[1].starts_with("pool mac: active"), "{}", out[1]);
    assert_eq!(
        out[2],
        "a1 mac h1 mf-1 lost waiting on stop_vm (outcome unknown): vanished\n"
    );
    assert_eq!(out[3], "no VMs\n");
    assert_eq!(out[4], "pool mac: breaker reset for sha256:cd\n");
    assert_eq!(
        &out[5..10],
        [
            "pool mac paused\n",
            "pool mac resumed\n",
            "pool mac draining\n",
            "host draining\n",
            "host resumed\n"
        ]
    );
    // a result this build cannot read for a person is printed as its JSON
    assert!(out[10].contains("\"unexpected\": true"), "{}", out[10]);
    let requests = canned.requests();
    assert!(
        requests[0].contains(r#""command":"status""#),
        "{requests:?}"
    );
    assert!(requests.iter().all(|r| r.contains(r#""api":1"#)));
}

#[test]
fn should_stream_events_and_end_with_the_refusal_when_the_daemon_stops_the_stream() {
    // given: the follow's first line, a lag, an event, then a refusal
    let event = EventLine {
        recorded_at: 5,
        stream: "pool-mac".to_owned(),
        revision: 2,
        kind: "pool_paused".to_owned(),
        schema_version: 1,
        payload: Value::Null,
    };
    let lines = vec![
        served!(Following { following: true }),
        served!(Lagged { lagged: 3 }),
        served!(event),
        failed("busy"),
    ];
    let canned = Canned::new(vec![lines.clone(), lines]);
    let target = canned.target();
    let request = Request::Events {
        follow: true,
        since_secs: Some(60),
    };

    // when
    let human = run(&target, &request, false);
    let json = run(&target, &request, true);

    // then
    assert_eq!(human.0, BUSY);
    assert_eq!(
        human.1,
        "(3 committed units not shown: this follower fell behind)\n5 pool-mac@2 pool_paused v1 null\n"
    );
    assert_eq!(human.2, "mayfly: busy: refused: busy\n");
    assert_eq!(json.0, BUSY);
    assert!(json.1.starts_with(r#"{"lagged":3}"#), "{}", json.1);
    assert!(json.1.contains(r#""kind":"pool_paused""#), "{}", json.1);
    assert!(json.1.contains(r#""code": "busy""#), "{}", json.1);
    drop(canned.requests());
}

#[test]
fn should_exit_with_the_matching_code_when_the_daemon_cannot_serve() {
    // given: a refusal, an answer with no failure document, an unreadable line
    let canned = Canned::new(vec![
        vec![failed("already_paused")],
        vec![r#"{"api":1,"ok":false}"#.to_owned()],
        vec!["not json".to_owned()],
    ]);
    let target = canned.target();
    let pause = Request::PoolPause {
        pool: "mac".to_owned(),
    };
    let nowhere = Target {
        socket: Some("/nonexistent/mayfly.sock".into()),
        config: None,
    };
    let bad_config = Target {
        socket: None,
        config: Some("/nonexistent/config.toml".into()),
    };

    // when
    let refused = run(&target, &pause, true);
    let bare = run(&target, &pause, false);
    let garbled = run(&target, &pause, false);
    let unreachable = run(&nowhere, &Request::Status, false);
    let unloadable = run(&bad_config, &Request::Status, false);
    let homeless = run(&Target::default(), &Request::Status, false);

    // then
    assert_eq!(refused.0, REFUSED);
    assert!(
        refused.1.contains(r#""code": "already_paused""#),
        "{}",
        refused.1
    );
    assert_eq!(
        bare,
        (REFUSED, String::new(), "mayfly: refused\n".to_owned())
    );
    assert_eq!(garbled.0, REFUSED);
    assert!(
        garbled
            .2
            .starts_with("mayfly: mayflyd answered something unreadable")
    );
    assert_eq!(unreachable.0, UNAVAILABLE);
    assert_eq!(unloadable.0, 78);
    assert_eq!(homeless.0, USAGE);
    drop(canned.requests());
}

#[test]
fn should_report_each_check_when_doctor_meets_a_daemon_in_trouble() {
    // given: a daemon in safe mode, not ready, a breaker tripped and a desync
    let canned = Canned::new(vec![
        vec![served!(status())],
        vec![served!(status())],
        vec![failed("unauthorised")],
    ]);
    let named = canned.target();
    let bin = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let tart = bin.path().join("tart");
    std::fs::write(&tart, "#!/bin/sh\n").unwrap_or_else(|e| panic!("{e}"));
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&tart, std::fs::Permissions::from_mode(0o755))
            .unwrap_or_else(|e| panic!("{e}"));
    }
    let path = bin.path().as_os_str().to_owned();
    let doctor_of = |target: &Target, json: bool, path: Option<&std::ffi::OsStr>| {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = doctor(
            target,
            json,
            &PlatformEnv::default(),
            path,
            &mut out,
            &mut err,
        );
        (code, String::from_utf8_lossy(&out).into_owned())
    };
    let bad_config = Target {
        socket: None,
        config: Some("/nonexistent/config.toml".into()),
    };

    // when
    let human = doctor_of(&named, false, Some(&path));
    let json = doctor_of(&named, true, None);
    let refused = doctor_of(&named, false, Some(&path));
    let homeless = doctor_of(&bad_config, false, None);

    // then
    assert_eq!(human.0, REFUSED);
    assert_eq!(
        human.1,
        "warn config: no --config given: not checked; the socket is the one --socket names\n\
         ok   tart: found on PATH\n\
         ok   daemon: mayflyd 9.9.9 answers: run 2, epoch 3, operating\n\
         warn ready: the run has not reported ready yet\n\
         FAIL safe_mode: in safe mode (epoch_below_floor): nothing is created or destroyed\n\
         warn breakers: tripped in mac: `mayfly pool reset-breaker <pool>` once fixed\n\
         warn desync: 1 VM(s) vanished outside mayfly: a desync; send the diagnostics bundle\n"
    );
    assert!(json.1.contains(r#""verdict": "fail""#), "{}", json.1);
    assert!(
        refused.1.contains("FAIL daemon: refused: unauthorised"),
        "{}",
        refused.1
    );
    // an unloadable configuration and no HOME: nowhere to look for the socket
    assert_eq!(homeless.0, USAGE);
    drop(canned.requests());
}
