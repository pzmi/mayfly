//! The auth chain of spec §1 against the fake: minting, refresh, single flight, the key asked
//! again, refusals, redaction, metrics and the permission probe.

use std::sync::Arc;

use mayfly_adapter_github::{
    AdminConnection, ErrorKind, GrantedPermissions, JWT_BACKDATE_SECS, JWT_LIFETIME_SECS, JwtError,
    Operation, PUBLISHED_PERMISSIONS, PermissionCheck, StatusClass, check_permissions,
};
use mayfly_control::secrets::Refusal;
use mayfly_domain::Millis;
use mayfly_fake_github::{Fault, JwtClaims, Route};

use crate::support::{SERVICE_URL, START, harness, key_ref, keys};

#[tokio::test(start_paused = true)]
async fn should_mint_an_admin_connection_when_the_chain_succeeds() {
    // given
    let h = harness(3600);

    // when
    let connection = h.auth.admin_connection().await.expect("connection");

    // then
    assert_eq!(connection.service_url.as_str(), SERVICE_URL);
    assert_eq!(connection.expires_at, Millis::new(START.get() + 3_600_000));
    let counts = h.fake.counts();
    assert_eq!(
        (
            counts.installation_tokens,
            counts.registration_tokens,
            counts.admin_exchanges
        ),
        (1, 1, 1)
    );
    let issued_at = START.get() / 1000 - JWT_BACKDATE_SECS;
    assert_eq!(
        h.fake.last_app_jwt(),
        Some(JwtClaims {
            issued_at,
            expires_at: issued_at + JWT_LIFETIME_SECS,
        })
    );
}

#[tokio::test(start_paused = true)]
async fn should_refresh_only_within_the_margin_when_the_admin_token_is_short_lived() {
    // given: admin tokens live 120 s; the refresh margin is 60 s
    let h = harness(120);
    let first = h.auth.admin_connection().await.expect("first");

    // when
    h.clock.advance(Millis::new(59_000));
    let early = h.auth.admin_connection().await.expect("early");
    h.clock.advance(Millis::new(2_000));
    let late = h.auth.admin_connection().await.expect("late");

    // then
    assert!(Arc::ptr_eq(&first, &early));
    assert!(!Arc::ptr_eq(&first, &late));
    assert_ne!(first.token.expose(), late.token.expose());
    assert_eq!(late.expires_at, Millis::new(START.get() + 61_000 + 120_000));
    assert_eq!(h.fake.counts().admin_exchanges, 2);
}

#[tokio::test(start_paused = true)]
async fn should_refresh_once_when_callers_race() {
    // given: the exchange is held until every caller is waiting
    let h = harness(3600);
    let gate = h.fake.hold(Route::RunnerRegistration);

    // when
    let (a, b, c, d, ()) = tokio::join!(
        h.auth.admin_connection(),
        h.auth.admin_connection(),
        h.auth.admin_connection(),
        h.auth.admin_connection(),
        async {
            gate.arrived().await;
            gate.release();
        },
    );

    // then
    let all = [a, b, c, d].map(|r| r.expect("connection"));
    assert!(all.iter().all(|c| Arc::ptr_eq(c, &all[0])));
    assert_eq!(h.fake.counts().admin_exchanges, 1);
    assert_eq!(h.fake.counts().installation_tokens, 1);
}

#[tokio::test(start_paused = true)]
async fn should_ask_the_provider_again_when_the_admin_token_is_refreshed() {
    // given
    let h = harness(120);
    h.auth.admin_connection().await.expect("first");
    h.secrets.remove(&key_ref());
    h.clock.advance(Millis::new(61_000));

    // when
    let without_key = h.auth.admin_connection().await;
    h.secrets.set(&key_ref(), keys().private_pem.as_bytes());
    let with_key = h.auth.admin_connection().await;

    // then
    let error = without_key.expect_err("the key is gone");
    assert_eq!(error.op, Operation::InstallationToken);
    assert!(matches!(
        error.kind,
        ErrorKind::Secret(ref e) if e.refusal == Refusal::NotFound
    ));
    assert!(with_key.is_ok());
    assert_eq!(h.fake.counts().admin_exchanges, 2);
}

#[tokio::test(start_paused = true)]
async fn should_refresh_once_when_a_stale_connection_is_invalidated_twice() {
    // given
    let h = harness(3600);
    let stale = h.auth.admin_connection().await.expect("first");

    // when
    h.auth.invalidate(&stale).await;
    let renewed = h.auth.admin_connection().await.expect("renewed");
    h.auth.invalidate(&stale).await;
    let again = h.auth.admin_connection().await.expect("again");

    // then
    assert!(!Arc::ptr_eq(&stale, &renewed));
    assert!(Arc::ptr_eq(&renewed, &again));
    assert_eq!(h.fake.counts().admin_exchanges, 2);
}

#[tokio::test(start_paused = true)]
async fn should_refuse_the_admin_token_when_it_has_no_expiry() {
    // given
    let h = harness(3600);
    h.fake
        .fail_next(Route::RunnerRegistration, Fault::AdminTokenWithoutExp);

    // when
    let got = h.auth.admin_connection().await;

    // then
    let error = got.expect_err("refused");
    assert_eq!(error.op, Operation::AdminExchange);
    assert_eq!(error.kind, ErrorKind::Jwt(JwtError::TokenWithoutExpiry));
}

#[tokio::test(start_paused = true)]
async fn should_name_the_missing_field_when_an_answer_lacks_one() {
    // given
    let h = harness(3600);
    let respond = |body: &str| Fault::Respond {
        status: 200,
        headers: Vec::new(),
        body: body.to_owned(),
    };
    h.fake
        .fail_next(Route::RunnerRegistration, respond(r#"{"token":"x"}"#));
    h.fake
        .fail_next(Route::RunnerRegistration, respond(r#"{"url":"ftp://x/"}"#));
    h.fake.fail_next(
        Route::RunnerRegistration,
        respond(r#"{"url":"https://pipelines.example/"}"#),
    );
    h.fake
        .fail_next(Route::RunnerRegistration, respond("\u{feff}not json"));

    // when
    let mut fields = Vec::new();
    for round in 0..5 {
        if round == 4 {
            h.fake.fail_next(Route::RegistrationToken, respond("{}"));
        }
        if let ErrorKind::Decode { field, .. } =
            h.auth.admin_connection().await.expect_err("refused").kind
        {
            fields.push(field);
        }
    }

    // then
    assert_eq!(fields, ["url", "url", "token", "body", "token"]);
}

#[tokio::test(start_paused = true)]
async fn should_keep_credentials_out_of_debug_and_errors_when_the_chain_runs_and_fails() {
    // given: a refusal whose body carries a canary and a query-bearing service URL
    let h = harness(3600);
    let connection = h.auth.admin_connection().await.expect("connection");
    let installation = h.auth.installation_token().await.expect("token");
    let canary = "ghs_CANARY_TOKEN_IN_A_BODY";
    h.fake.fail_next(
        Route::InstallationToken,
        Fault::Respond {
            status: 401,
            headers: Vec::new(),
            body: format!(r#"{{"message":"bad credentials","token":"{canary}"}}"#),
        },
    );
    h.clock.advance(Millis::new(3_600_000));

    // when
    let error = h.auth.admin_connection().await.expect_err("refused");
    let printed = [
        format!("{connection:?}"),
        format!("{installation:?}"),
        format!("{:?}", h.auth),
        format!("{error:?}"),
        error.to_string(),
    ]
    .join("\n");

    // then
    let mut secrets = h.fake.issued_tokens();
    secrets.push(canary.to_owned());
    secrets.extend(
        keys()
            .private_pem
            .lines()
            .skip(1)
            .take(3)
            .map(str::to_owned),
    );
    assert!(secrets.len() >= 6);
    for secret in &secrets {
        assert!(!printed.contains(secret.as_str()), "leaked into: {printed}");
    }
    assert!(printed.contains("answered 401"));
}

#[tokio::test(start_paused = true)]
async fn should_count_attempts_by_bounded_labels_when_calls_are_made() {
    // given
    let h = harness(3600);
    h.fake.fail_next(
        Route::RegistrationToken,
        Fault::Respond {
            status: 502,
            headers: Vec::new(),
            body: String::new(),
        },
    );

    // when
    h.auth.admin_connection().await.expect("connection");

    // then
    let counts: Vec<(&str, &str, u64)> = h
        .auth
        .metrics()
        .snapshot()
        .iter()
        .map(|c| (c.op.as_str(), c.status_class.as_str(), c.count))
        .collect();
    assert_eq!(
        counts,
        [
            ("installation_token", "2xx", 1),
            ("registration_token", "2xx", 1),
            ("registration_token", "5xx", 1),
            ("admin_exchange", "2xx", 1),
        ]
    );
    let labels = [
        StatusClass::Success,
        StatusClass::Unexpected,
        StatusClass::RateLimited,
        StatusClass::ClientError,
        StatusClass::ServerError,
        StatusClass::Transport,
        StatusClass::Timeout,
    ]
    .map(StatusClass::as_str);
    assert_eq!(
        labels,
        [
            "2xx",
            "other",
            "rate_limited",
            "4xx",
            "5xx",
            "transport",
            "timeout"
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn should_report_granted_permissions_when_the_installation_is_probed() {
    // given
    let h = harness(3600);

    // when
    let granted = h.auth.granted_permissions().await.expect("permissions");

    // then
    let GrantedPermissions(map) = &granted;
    assert_eq!(
        map.get("organization_self_hosted_runners")
            .map(String::as_str),
        Some("write")
    );
    assert_eq!(map.get("metadata").map(String::as_str), Some("read"));
    assert_eq!(
        check_permissions(&granted, PUBLISHED_PERMISSIONS),
        PermissionCheck::ListUnverified
    );
}

#[tokio::test(start_paused = true)]
async fn should_refuse_the_service_url_when_a_token_could_leak_through_it() {
    // given: answers naming a service the admin token must never be sent to
    let h = harness(3600);
    let urls = [
        "http://pipelines.example/synthetic",
        "https://user:pw@pipelines.example/synthetic",
        "ftp://pipelines.example/synthetic",
        "https://pipelines.example/synthetic?token=1",
        "https://pipelines.example/synthetic#fragment",
    ];

    // when
    let mut refused = Vec::new();
    for url in urls {
        h.fake.fail_next(
            Route::RunnerRegistration,
            Fault::Respond {
                status: 200,
                headers: Vec::new(),
                body: format!(r#"{{"url":"{url}","token":"x"}}"#),
            },
        );
        refused.push(h.auth.admin_connection().await.expect_err("refused").kind);
    }

    // then
    assert!(
        refused
            .iter()
            .all(|k| matches!(k, ErrorKind::Decode { field: "url", .. })),
        "{refused:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn should_share_a_failed_refresh_when_callers_waited_on_it() {
    // given: the exchange is held until every caller waits, then refused once
    let h = harness(3600);
    let gate = h.fake.hold(Route::RunnerRegistration);
    h.fake.fail_next(
        Route::RunnerRegistration,
        Fault::Respond {
            status: 400,
            headers: Vec::new(),
            body: String::new(),
        },
    );

    // when
    let (a, b, c, d, ()) = tokio::join!(
        h.auth.admin_connection(),
        h.auth.admin_connection(),
        h.auth.admin_connection(),
        h.auth.admin_connection(),
        async {
            gate.arrived().await;
            gate.release();
        },
    );
    let after = h.auth.admin_connection().await;

    // then: one chain of three requests, its error shared; a later caller starts afresh
    let errors = [a, b, c, d].map(|r| r.expect_err("shared failure"));
    assert!(errors.iter().all(|e| e == &errors[0]));
    assert!(matches!(
        errors[0].kind,
        ErrorKind::Status { status: 400, .. }
    ));
    let counts = h.fake.counts();
    assert_eq!(
        (
            counts.installation_tokens,
            counts.registration_tokens,
            counts.admin_exchanges
        ),
        (2, 2, 1)
    );
    assert_eq!(counts.requests, 6);
    assert!(after.is_ok());
}

#[tokio::test(start_paused = true)]
async fn should_refuse_the_admin_token_when_it_expires_within_the_margin() {
    // given: admin tokens that live 30 s, inside the 60 s refresh margin
    let h = harness(30);

    // when
    let got = h.auth.admin_connection().await;

    // then
    let error = got.expect_err("too short");
    assert_eq!(
        error.kind,
        ErrorKind::AdminTokenTooShortLived {
            remaining: Millis::new(30_000)
        }
    );
    assert!(error.to_string().contains("refresh margin"));
}

#[tokio::test(start_paused = true)]
async fn should_print_the_service_url_without_its_query_when_debugging_a_connection() {
    // given
    let h = harness(3600);
    h.auth.admin_connection().await.expect("connection");
    let crafted = AdminConnection {
        service_url: url::Url::parse("https://pipelines.example/x?sig=SECRETQUERY").expect("url"),
        token: mayfly_domain::Secret::new("SECRETTOKEN".to_owned()),
        expires_at: START,
    };

    // when
    let printed = format!("{:?}\n{crafted:?}", h.auth);

    // then
    assert!(printed.contains("https://pipelines.example/synthetic-service"));
    assert!(printed.contains("https://pipelines.example/x"));
    assert!(!printed.contains("SECRETQUERY"));
    assert!(!printed.contains("SECRETTOKEN"));
    assert!(
        h.fake
            .issued_tokens()
            .iter()
            .all(|t| !printed.contains(t.as_str()))
    );
}
