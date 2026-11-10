//! The fake's auth-chain routes refuse what GitHub refuses (spec §1).

use std::collections::BTreeMap;
use std::sync::Arc;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rsa::PrivateKey;
use rsa::pkcs1::DecodeRsaPrivateKey;
use rsa::sha2::Sha256;

use mayfly_fake_github::{
    FakeConfig, FakeGithub, FakeRequest, FakeResponse, Fault, Route, Scope, generate_app_key,
};

const NOW_MS: u64 = 1_000_000_000;
const ACCESS: &str = "/app/installations/7/access_tokens";
const EXCHANGE: &str = "/actions/runner-registration";

fn fake(scope: Scope) -> FakeGithub {
    fake_with(scope, "no key")
}

fn fake_with(scope: Scope, public_pem: &str) -> FakeGithub {
    FakeGithub::new(
        FakeConfig {
            app_client_id: "app".to_owned(),
            app_public_key_pem: public_pem.to_owned(),
            installation_id: 7,
            scope,
            web_base: "https://github.com".to_owned(),
            service_url: "https://pipelines.example/x".to_owned(),
            admin_token_lifetime_secs: 60,
            permissions: BTreeMap::new(),
        },
        Arc::new(|| NOW_MS),
    )
}

fn post(path: &str, authorization: &str, body: &str) -> FakeRequest {
    FakeRequest {
        method: "POST".to_owned(),
        path: path.to_owned(),
        headers: vec![("Authorization".to_owned(), authorization.to_owned())],
        body: body.as_bytes().to_vec(),
    }
}

fn app_jwt(private_pem: &str, iat: u64, exp: u64) -> String {
    let key = PrivateKey::from_pkcs1_pem(private_pem).expect("key");
    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256","typ":"JWT"}"#);
    let claims = URL_SAFE_NO_PAD.encode(format!(r#"{{"iat":{iat},"exp":{exp},"iss":"app"}}"#));
    let signed = format!("{header}.{claims}");
    let signature = key
        .pkcs1v15_sign::<Sha256>(signed.as_bytes())
        .expect("sign");
    format!("{signed}.{}", URL_SAFE_NO_PAD.encode(signature))
}

fn claims(jwt: &str) -> String {
    let part = jwt.split('.').nth(1).expect("claims");
    String::from_utf8(URL_SAFE_NO_PAD.decode(part).expect("base64")).expect("utf-8")
}

fn token(answer: &FakeResponse) -> String {
    let body: serde_json::Value = serde_json::from_slice(&answer.body).expect("json");
    body["token"].as_str().expect("token").to_owned()
}

#[tokio::test]
async fn should_refuse_credentials_when_they_were_not_issued_by_the_previous_step() {
    // given
    let fake = fake(Scope::Repository("acme".to_owned(), "widgets".to_owned()));
    let registration = "/repos/acme/widgets/actions/runners/registration-token";

    // when
    let answers = [
        fake.handle(post(ACCESS, "Bearer x.y.z", "")).await,
        fake.handle(post(ACCESS, "token abc", "")).await,
        fake.handle(post(registration, "Bearer ghs_unknown", ""))
            .await,
        fake.handle(post(EXCHANGE, "RemoteAuth unknown", "")).await,
        fake.handle(post(
            "/orgs/acme/actions/runners/registration-token",
            "",
            "",
        ))
        .await,
        fake.handle(FakeRequest {
            method: "GET".to_owned(),
            ..post(registration, "", "")
        })
        .await,
    ];

    // then
    let statuses: Vec<u16> = answers.iter().map(|a| a.status).collect();
    assert_eq!(statuses, [401, 401, 401, 401, 404, 404]);
    assert_eq!(fake.counts().requests, 6);
    assert!(format!("{:?}", answers[0]).contains("body_bytes"));
}

#[tokio::test]
async fn should_answer_the_exchange_only_when_its_body_names_the_scope() {
    // given: an App JWT signed here, and the chain's first two tokens
    let keys = generate_app_key();
    let fake = fake_with(Scope::Organization("acme".to_owned()), &keys.public_pem);
    let now = NOW_MS / 1000;
    let jwt = app_jwt(&keys.private_pem, now - 60, now + 540);
    let installation = token(
        &fake
            .handle(post(ACCESS, &format!("Bearer {jwt}"), ""))
            .await,
    );
    let registration = token(
        &fake
            .handle(post(
                "/orgs/acme/actions/runners/registration-token",
                &format!("Bearer {installation}"),
                "",
            ))
            .await,
    );
    let remote = format!("RemoteAuth {registration}");
    let body = |url: &str| format!(r#"{{"url":"{url}","runner_event":"register"}}"#);

    // when
    let wrong = fake
        .handle(post(EXCHANGE, &remote, &body("https://github.com/other")))
        .await;
    fake.fail_next(Route::RunnerRegistration, Fault::AdminTokenWithoutExp);
    let without_exp = fake
        .handle(post(EXCHANGE, &remote, &body("https://github.com/acme")))
        .await;
    let right = fake
        .handle(post(EXCHANGE, &remote, &body("https://github.com/acme")))
        .await;

    // then
    assert_eq!(
        (wrong.status, without_exp.status, right.status),
        (400, 200, 200)
    );
    assert!(!claims(&token(&without_exp)).contains("exp"));
    assert!(claims(&token(&right)).contains(&format!(r#""exp":{}"#, now + 60)));
    let counts = fake.counts();
    assert_eq!(
        (
            counts.installation_tokens,
            counts.registration_tokens,
            counts.admin_exchanges
        ),
        (1, 1, 2)
    );
    assert_eq!(fake.issued_tokens().len(), 4);
    assert!(fake.last_app_jwt().is_some());
}
