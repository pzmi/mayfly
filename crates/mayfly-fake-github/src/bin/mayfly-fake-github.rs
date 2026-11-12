//! The fake as a process, for end-to-end runs and the fault lab (ver §2, §7.3).
//!
//! `mayfly-fake-github --config <file> [--port <n>]` reads a JSON file
//! `{"config": <FakeConfig>, "knobs": <Knobs, optional>}`, serves the fake on `127.0.0.1` (port 0
//! by default: the system picks), writes one line `listening http://127.0.0.1:<port>` to stdout
//! once it accepts connections, and serves until its stdin closes. The control API under
//! `/_fake/` scripts it and dumps its state; it is unauthenticated, so the fake binds loopback
//! only and is never exposed beyond the host. Its clock is the system's. Pinned by:
//! `tests/wire/process.rs::should_serve_the_control_api_until_stdin_closes_when_run_as_a_process`.

use std::io::{BufRead, Write};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Deserialize;

use mayfly_fake_github::{FakeConfig, FakeGithub, FakeServer, Knobs};

#[derive(Deserialize)]
struct File {
    config: FakeConfig,
    #[serde(default)]
    knobs: Knobs,
}

fn usage(why: &str) -> ExitCode {
    eprintln!("mayfly-fake-github: {why}\nusage: mayfly-fake-github --config <file> [--port <n>]");
    ExitCode::from(2)
}

fn main() -> ExitCode {
    let mut config = None;
    let mut port = 0_u16;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match (arg.as_str(), args.next()) {
            ("--config", Some(path)) => config = Some(path),
            ("--port", Some(n)) => match n.parse() {
                Ok(n) => port = n,
                Err(_) => return usage("--port takes a number"),
            },
            _ => return usage("unknown argument"),
        }
    }
    let Some(path) = config else {
        return usage("--config is required");
    };
    let file: File = match std::fs::read(&path)
        .map_err(|e| e.to_string())
        .and_then(|bytes| serde_json::from_slice(&bytes).map_err(|e| e.to_string()))
    {
        Ok(file) => file,
        Err(why) => return usage(&format!("cannot read the config: {why}")),
    };
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(why) => return usage(&format!("cannot start a runtime: {why}")),
    };
    runtime.block_on(serve(file, port))
}

async fn serve(file: File, port: u16) -> ExitCode {
    let now_ms = Arc::new(|| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
    });
    let fake = FakeGithub::with_knobs(file.config, file.knobs, now_ms);
    let server = match FakeServer::start_on(fake, port).await {
        Ok(server) => server,
        Err(why) => return usage(&format!("cannot bind: {why}")),
    };
    let mut stdout = std::io::stdout();
    if writeln!(stdout, "listening {}", server.base_url())
        .and_then(|()| stdout.flush())
        .is_err()
    {
        return ExitCode::FAILURE;
    }
    let _ = tokio::task::spawn_blocking(|| {
        let mut line = String::new();
        let mut stdin = std::io::stdin().lock();
        while stdin.read_line(&mut line).is_ok_and(|n| n > 0) {
            line.clear();
        }
    })
    .await;
    match server.stop().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(_) => ExitCode::FAILURE,
    }
}
