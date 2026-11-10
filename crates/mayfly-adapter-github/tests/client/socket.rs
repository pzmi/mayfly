//! `ReqwestTransport` over a loopback socket to the fake's HTTP server. Real time here: a paused
//! Tokio clock would fire the attempt timeout while the socket waits (it advances whenever the
//! runtime idles), so these tests keep the default deadlines and never wait on a timer.

use std::sync::Arc;

use mayfly_adapter_github::{
    ApiClient, AppAuth, DEFAULT_CALL_POLICY, ErrorKind, HttpRequest, Method, ReqwestTransport,
    Transport, TransportError, transport_retryable,
};
use mayfly_control::store::Clock;
use mayfly_domain::Secret;
use mayfly_fake_github::{FakeGithub, FakeServer, Fault, Route};
use mayfly_testkit::clock::{FakeClock, SeededRng};
use mayfly_testkit::secrets::MemorySecretProvider;

use crate::support::{SERVICE_URL, START, app_config, fake_config, key_ref, keys};

async fn serve() -> (
    FakeGithub,
    FakeServer,
    AppAuth<ReqwestTransport, FakeClock, SeededRng, MemorySecretProvider>,
) {
    let clock = FakeClock::at(START);
    let reader = clock.clone();
    let fake = FakeGithub::new(
        fake_config("https://github.com", 3600),
        Arc::new(move || reader.now().get()),
    );
    let server = FakeServer::start(fake.clone()).await.expect("bind");
    let secrets = MemorySecretProvider::new();
    secrets.set(&key_ref(), keys().private_pem.as_bytes());
    let client = ApiClient::new(
        ReqwestTransport::new().expect("tls"),
        clock,
        SeededRng::new(1),
        DEFAULT_CALL_POLICY,
    );
    let auth = AppAuth::new(app_config(&server.base_url()), client, secrets).expect("config");
    (fake, server, auth)
}

#[tokio::test]
async fn should_complete_the_chain_when_the_fake_is_served_over_a_socket() {
    // given
    let (fake, server, auth) = serve().await;

    // when
    let connection = auth.admin_connection().await;

    // then
    server.stop().await.expect("stop");
    assert_eq!(
        connection.expect("connection").service_url.as_str(),
        SERVICE_URL
    );
    assert_eq!(fake.counts().admin_exchanges, 1);
}

#[tokio::test]
async fn should_refuse_the_answer_when_its_body_is_over_the_limit() {
    // given
    let (fake, server, auth) = serve().await;
    fake.fail_next(
        Route::InstallationToken,
        Fault::Respond {
            status: 200,
            headers: Vec::new(),
            body: "x".repeat(mayfly_adapter_github::MAX_RESPONSE_BYTES + 1),
        },
    );

    // when
    let got = auth.installation_token().await;

    // then
    server.stop().await.expect("stop");
    assert!(matches!(
        got.expect_err("too large").kind,
        ErrorKind::Transport {
            error: TransportError::BodyTooLarge,
            attempts: 1,
            ..
        }
    ));
}

#[tokio::test]
async fn should_answer_404_when_the_route_is_unknown() {
    // given: an installation the fake does not serve
    let (fake, server, _) = serve().await;
    let mut config = app_config(&server.base_url());
    config.installation_id += 1;
    let secrets = MemorySecretProvider::new();
    secrets.set(&key_ref(), keys().private_pem.as_bytes());
    let client = ApiClient::new(
        ReqwestTransport::new().expect("tls"),
        FakeClock::at(START),
        SeededRng::new(1),
        DEFAULT_CALL_POLICY,
    );
    let auth = AppAuth::new(config, client, secrets).expect("config");

    // when
    let got = auth.installation_token().await;

    // then
    server.stop().await.expect("stop");
    assert!(matches!(
        got.expect_err("unknown").kind,
        ErrorKind::Status { status: 404, .. }
    ));
    assert_eq!(fake.counts().requests, 1);
}

#[tokio::test]
async fn should_not_follow_a_redirect_when_the_server_answers_one() {
    // given: a redirect that would carry the App JWT to another host
    let (fake, server, auth) = serve().await;
    fake.fail_next(
        Route::InstallationToken,
        Fault::Respond {
            status: 307,
            headers: vec![("location".to_owned(), "http://127.0.0.2:9/steal".to_owned())],
            body: String::new(),
        },
    );

    // when
    let got = auth.installation_token().await;

    // then
    server.stop().await.expect("stop");
    assert!(matches!(
        got.expect_err("not followed").kind,
        ErrorKind::Status {
            status: 307,
            attempts: 1,
            ..
        }
    ));
    assert_eq!(fake.counts().requests, 1);
}

#[tokio::test]
async fn should_refuse_without_sending_when_a_header_value_is_invalid() {
    // given: a credential and a header with a line break, which no retry can fix
    let (fake, server, _) = serve().await;
    let transport = ReqwestTransport::new().expect("tls");
    let url = url::Url::parse(&format!(
        "{}/app/installations/1/access_tokens",
        server.base_url()
    ))
    .expect("url");
    let request = |authorization: &str, header: &str| HttpRequest {
        method: Method::Post,
        url: url.clone(),
        authorization: Some(Secret::new(authorization.to_owned())),
        headers: vec![("x-synthetic", header.to_owned())],
        body: None,
    };

    // when
    let credential = transport.send(request("Bearer a\nb", "ok")).await;
    let header = transport.send(request("Bearer ok", "a\nb")).await;

    // then
    server.stop().await.expect("stop");
    assert_eq!(credential, Err(TransportError::InvalidRequest));
    assert_eq!(header, Err(TransportError::InvalidRequest));
    assert!(!transport_retryable(TransportError::InvalidRequest));
    assert_eq!(fake.counts().requests, 0);
}
