//! The fake as a process: its ready line, its control API over the socket, its stop on EOF.
//! A process, because end-to-end runs and the fault lab start the fake as one.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, ExitStatus, Stdio};

use serde_json::json;

use mayfly_fake_github::contract::{Socket, WireRequest, WireTarget};
use mayfly_fake_github::{FakeConfig, FakeGithub, FakeServer, Scope};

/// Owns the fake's process: dropped, it closes the stdin, kills the process and reaps it, so no
/// fake outlives its test, whatever the test's outcome.
struct Running(Option<Child>);

impl Running {
    fn child(&mut self) -> &mut Child {
        self.0.as_mut().expect("the fake is running")
    }

    /// Closes the stdin and waits for the exit. No deadline (ADR 0023): the fake exits on EOF,
    /// and a fake that does not is ended by the test run's own timeout and this guard.
    fn exit(mut self) -> Option<ExitStatus> {
        let mut child = self.0.take()?;
        drop(child.stdin.take());
        child.wait().ok()
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            drop(child.stdin.take());
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[tokio::test]
async fn should_serve_the_control_api_until_stdin_closes_when_run_as_a_process() {
    // given
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("fake.json");
    let config = FakeConfig {
        app_client_id: "Iv1.process".to_owned(),
        app_public_key_pem: "no key".to_owned(),
        installation_id: 1,
        scope: Scope::Organization("acme".to_owned()),
        web_base: "https://github.com".to_owned(),
        service_url: "http://fake.invalid/service".to_owned(),
        admin_token_lifetime_secs: 60,
        admin_token_key_pem: "unused: this test runs no exchange".to_owned(),
        permissions: Default::default(),
    };
    std::fs::write(
        &path,
        json!({"config": config, "knobs": {"flow": "acquire"}}).to_string(),
    )
    .expect("write config");
    let mut fake = Running(Some(
        Command::new(env!("CARGO_BIN_EXE_mayfly-fake-github"))
            .arg("--config")
            .arg(&path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn"),
    ));
    let mut ready = String::new();
    BufReader::new(fake.child().stdout.take().expect("stdout"))
        .read_line(&mut ready)
        .expect("ready line");
    let addr = ready
        .trim()
        .strip_prefix("listening http://")
        .expect("ready line")
        .parse()
        .expect("addr");

    // when
    let state = Socket(addr)
        .send(WireRequest {
            method: "GET".to_owned(),
            url: "http://fake.invalid/_fake/state".to_owned(),
            headers: Vec::new(),
            body: Vec::new(),
        })
        .await
        .expect("state");
    let status = fake.exit();

    // then
    assert_eq!(state.status, 200);
    assert_eq!(state.json()["knobs"]["flow"], "acquire");
    assert!(status.is_some_and(|s| s.success()), "exit {status:?}");
}

#[test]
fn should_exit_2_when_the_config_is_missing() {
    // given
    let binary = env!("CARGO_BIN_EXE_mayfly-fake-github");

    // when
    let output = Command::new(binary).output().expect("run");

    // then
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("--config is required"));
}

#[tokio::test]
async fn should_serve_the_service_on_its_own_base_when_built_from_it() {
    // given: a fake whose service URL is built from the base it is served on
    let mut seen = String::new();
    let server = FakeServer::start_with(|base| {
        seen = base.to_owned();
        FakeGithub::new(
            FakeConfig {
                app_client_id: "Iv1.self".to_owned(),
                app_public_key_pem: "no key".to_owned(),
                installation_id: 1,
                scope: Scope::Organization("acme".to_owned()),
                web_base: "https://github.com".to_owned(),
                service_url: format!("{base}/service"),
                admin_token_lifetime_secs: 60,
                admin_token_key_pem: "unused: this test runs no exchange".to_owned(),
                permissions: Default::default(),
            },
            std::sync::Arc::new(|| 0),
        )
    })
    .await
    .expect("bind");
    let addr = server
        .base_url()
        .strip_prefix("http://")
        .expect("a base")
        .parse()
        .expect("addr");

    // when
    let state = Socket(addr)
        .send(WireRequest {
            method: "GET".to_owned(),
            url: format!("{}/_fake/state", server.base_url()),
            headers: Vec::new(),
            body: Vec::new(),
        })
        .await
        .expect("state");
    let base = server.base_url();
    server.stop().await.expect("stop");

    // then
    assert_eq!(seen, base);
    assert_eq!(state.status, 200);
}
