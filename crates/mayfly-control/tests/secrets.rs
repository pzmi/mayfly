//! The `SecretProvider` port's shared rules: the byte rule every provider follows, refusals
//! that never echo the source, and the `doctor` check that never keeps a value (ADR 0033).

use mayfly_control::secrets::{
    MAX_SECRET_BYTES, Refusal, SecretError, check_references, secret_from_bytes,
};
use mayfly_domain::SecretRef;
use mayfly_testkit::block_on;
use mayfly_testkit::secrets::MemorySecretProvider;

fn reference(name: &str) -> SecretRef {
    SecretRef::Env {
        name: name.to_owned(),
    }
}

fn refusal(got: Result<mayfly_domain::Secret<String>, SecretError>) -> Option<Refusal> {
    got.err().map(|err| err.refusal)
}

#[test]
fn should_drop_one_trailing_line_break_when_bytes_become_a_secret() {
    // given
    let cases = [
        (&b"value\n"[..], "value"),
        (b"value\r\n", "value"),
        (b"value\n\n", "value\n"),
        (b"value\r", "value\r"),
        (b"a\nb", "a\nb"),
    ];

    // when
    let got: Vec<String> = cases
        .iter()
        .map(|(bytes, _)| {
            secret_from_bytes(&reference("A"), bytes.to_vec())
                .map(|s| s.expose().clone())
                .unwrap_or_default()
        })
        .collect();

    // then
    let want: Vec<String> = cases.iter().map(|(_, w)| (*w).to_owned()).collect();
    assert_eq!(got, want);
}

#[test]
fn should_refuse_when_bytes_are_empty_too_large_or_not_text() {
    // given
    let too_large = vec![b'x'; MAX_SECRET_BYTES + 1];

    // when
    let got = [
        refusal(secret_from_bytes(&reference("A"), Vec::new())),
        refusal(secret_from_bytes(&reference("A"), b"\r\n".to_vec())),
        refusal(secret_from_bytes(&reference("A"), too_large)),
        refusal(secret_from_bytes(&reference("A"), vec![0xff, 0xfe])),
        refusal(secret_from_bytes(
            &reference("A"),
            vec![b'x'; MAX_SECRET_BYTES],
        )),
    ];

    // then
    assert_eq!(
        got,
        [
            Some(Refusal::Empty),
            Some(Refusal::Empty),
            Some(Refusal::TooLarge {
                limit: MAX_SECRET_BYTES
            }),
            Some(Refusal::NotText),
            None,
        ]
    );
}

#[test]
fn should_not_echo_the_value_when_a_source_is_refused() {
    // given
    let mut bytes = b"canary-not-text-".to_vec();
    bytes.push(0xff);

    // when
    let err = secret_from_bytes(&reference("APP_KEY"), bytes).err();

    // then
    let shown = format!(
        "{err:?} {}",
        err.as_ref().map(ToString::to_string).unwrap_or_default()
    );
    assert!(!shown.contains("canary"), "{shown}");
    assert!(shown.contains("env:APP_KEY"), "{shown}");
}

#[test]
fn should_report_each_reference_without_its_value_when_doctor_checks_them() {
    // given
    let provider = MemorySecretProvider::new();
    provider.set(&reference("PRESENT"), b"synthetic-doctor-value\n");
    let references = [
        ("github.app_key".to_owned(), reference("PRESENT")),
        ("registry.token".to_owned(), reference("ABSENT")),
    ];

    // when
    let checks = block_on(check_references(&provider, &references));

    // then
    let outcomes: Vec<(&str, Option<Refusal>)> = checks
        .iter()
        .map(|c| (c.name.as_str(), c.outcome.clone().err().map(|e| e.refusal)))
        .collect();
    assert_eq!(
        outcomes,
        [
            ("github.app_key", None),
            ("registry.token", Some(Refusal::NotFound))
        ]
    );
    assert!(!format!("{checks:?}").contains("synthetic-doctor-value"));
}
