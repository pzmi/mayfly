//! A fake GitHub wired to an `AppAuth` in process, on a fake clock both read.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use mayfly_adapter_github::{
    ApiClient, AppAuth, AppConfig, CallPolicy, DEFAULT_CALL_POLICY, HttpRequest, HttpResponse,
    Scope, Transport, TransportError,
};
use mayfly_control::store::Clock;
use mayfly_domain::{Millis, SecretRef};
use mayfly_fake_github::{AppKeyPair, FakeConfig, FakeGithub, FakeRequest, generate_app_key};
use mayfly_testkit::clock::{FakeClock, SeededRng};
use mayfly_testkit::secrets::MemorySecretProvider;
use url::Url;

/// 2026-11-10T12:00:00Z.
pub const START: Millis = Millis::new(1_794_312_000_000);

pub const CLIENT_ID: &str = "Iv1.synthetic";
pub const INSTALLATION: u64 = 4242;
pub const ORG: &str = "acme";
pub const SERVICE_URL: &str = "https://pipelines.example/synthetic-service";

/// One synthetic key pair per test binary: generating one takes about a second.
pub fn keys() -> &'static AppKeyPair {
    static KEYS: OnceLock<AppKeyPair> = OnceLock::new();
    KEYS.get_or_init(generate_app_key)
}

pub fn key_ref() -> SecretRef {
    SecretRef::File {
        path: PathBuf::from("/synthetic/app-key.pem"),
    }
}

/// The fake, answered in process.
#[derive(Debug, Clone)]
pub struct InProcess(pub FakeGithub);

impl Transport for InProcess {
    async fn send(&self, request: HttpRequest) -> Result<HttpResponse, TransportError> {
        let mut headers: Vec<(String, String)> = request
            .headers
            .iter()
            .map(|(n, v)| ((*n).to_owned(), v.clone()))
            .collect();
        if let Some(credential) = &request.authorization {
            headers.push(("authorization".to_owned(), credential.expose().clone()));
        }
        let answer = self
            .0
            .handle(FakeRequest {
                method: request.method.as_str().to_owned(),
                path: request.url.path().to_owned(),
                headers,
                body: request.body.unwrap_or_default(),
            })
            .await;
        Ok(HttpResponse {
            status: answer.status,
            headers: answer.headers,
            body: answer.body,
        })
    }
}

pub type Auth<T = InProcess> = AppAuth<T, FakeClock, SeededRng, MemorySecretProvider>;

pub struct Harness<T = InProcess> {
    pub fake: FakeGithub,
    pub clock: FakeClock,
    pub secrets: MemorySecretProvider,
    pub auth: Auth<T>,
}

pub fn fake_config(api_web_base: &str, admin_lifetime_secs: u64) -> FakeConfig {
    FakeConfig {
        app_client_id: CLIENT_ID.to_owned(),
        app_public_key_pem: keys().public_pem.clone(),
        installation_id: INSTALLATION,
        scope: mayfly_fake_github::Scope::Organization(ORG.to_owned()),
        web_base: api_web_base.to_owned(),
        service_url: SERVICE_URL.to_owned(),
        admin_token_lifetime_secs: admin_lifetime_secs,
        permissions: BTreeMap::from([
            ("metadata".to_owned(), "read".to_owned()),
            (
                "organization_self_hosted_runners".to_owned(),
                "write".to_owned(),
            ),
        ]),
    }
}

pub fn app_config(api_base: &str) -> AppConfig {
    AppConfig {
        api_base: Url::parse(api_base).expect("url"),
        web_base: Url::parse("https://github.com").expect("url"),
        client_id: CLIENT_ID.to_owned(),
        installation_id: INSTALLATION,
        scope: Scope::Organization {
            org: ORG.to_owned(),
        },
        app_key: key_ref(),
    }
}

/// A fake whose admin tokens live `admin_lifetime_secs`, and a chain against it in process.
pub fn harness(admin_lifetime_secs: u64) -> Harness {
    harness_with(admin_lifetime_secs, DEFAULT_CALL_POLICY, |inner| inner)
}

/// As [`harness`], with a call policy and a transport wrapped around the in-process fake.
pub fn harness_with<T: Transport>(
    admin_lifetime_secs: u64,
    policy: CallPolicy,
    wrap: impl FnOnce(InProcess) -> T,
) -> Harness<T> {
    let clock = FakeClock::at(START);
    let reader = clock.clone();
    let fake = FakeGithub::new(
        fake_config("https://github.com", admin_lifetime_secs),
        Arc::new(move || reader.now().get()),
    );
    let secrets = MemorySecretProvider::new();
    secrets.set(&key_ref(), keys().private_pem.as_bytes());
    let client = ApiClient::new(
        wrap(InProcess(fake.clone())),
        clock.clone(),
        SeededRng::new(7),
        policy,
    );
    let auth = AppAuth::new(
        app_config("https://api.github.com"),
        client,
        secrets.clone(),
    )
    .expect("valid config");
    Harness {
        fake,
        clock,
        secrets,
        auth,
    }
}
