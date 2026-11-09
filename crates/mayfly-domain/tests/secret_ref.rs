//! Secret references through the public API (ADR 0033): the four schemes, their refusals, and
//! that a refusal never carries the refused text.

use std::path::PathBuf;

use mayfly_domain::{SecretRef, SecretRefError};

#[test]
fn should_parse_each_scheme_when_the_reference_is_well_formed() {
    // given
    let cases = [
        (
            "keychain:mayfly/app-key",
            SecretRef::Keychain {
                service: "mayfly".to_owned(),
                account: "app-key".to_owned(),
            },
        ),
        (
            "file:/etc/mayfly/app.pem",
            SecretRef::File {
                path: PathBuf::from("/etc/mayfly/app.pem"),
            },
        ),
        (
            "env:MAYFLY_APP_KEY",
            SecretRef::Env {
                name: "MAYFLY_APP_KEY".to_owned(),
            },
        ),
        (
            "exec:/usr/local/bin/print-key",
            SecretRef::Exec {
                path: PathBuf::from("/usr/local/bin/print-key"),
            },
        ),
    ];
    for (text, expected) in cases {
        // when
        let parsed: SecretRef = text.parse().unwrap();
        // then
        assert_eq!(parsed, expected);
        assert_eq!(parsed.to_string(), text);
    }
}

#[test]
fn should_name_the_scheme_when_a_reference_is_parsed() {
    // given
    let refs = ["keychain:a/b", "file:/a", "env:A", "exec:/a"];
    // when
    let schemes: Vec<&str> = refs
        .iter()
        .map(|text| text.parse::<SecretRef>().unwrap().scheme())
        .collect();
    // then
    assert_eq!(schemes, ["keychain", "file", "env", "exec"]);
}

#[test]
fn should_refuse_with_the_scheme_error_when_the_reference_is_malformed() {
    // given
    let cases = [
        ("", SecretRefError::UnknownScheme),
        ("just-a-value", SecretRefError::UnknownScheme),
        ("vault:secret/app", SecretRefError::UnknownScheme),
        ("FILE:/etc/key", SecretRefError::UnknownScheme),
        ("keychain:onlyservice", SecretRefError::KeychainShape),
        ("keychain:/account", SecretRefError::KeychainShape),
        ("keychain:service/", SecretRefError::KeychainShape),
        ("keychain:a/b/c", SecretRefError::KeychainShape),
        ("keychain:a/b\nc", SecretRefError::KeychainShape),
        ("file:relative/key.pem", SecretRefError::FilePath),
        ("file:", SecretRefError::FilePath),
        ("file:/etc/../key", SecretRefError::FilePath),
        ("file:/etc/key\0", SecretRefError::FilePath),
        ("env:", SecretRefError::EnvName),
        ("env:1ABC", SecretRefError::EnvName),
        ("env:HAS SPACE", SecretRefError::EnvName),
        ("exec:bin/print", SecretRefError::ExecPath),
        ("exec:/usr/bin/op read x", SecretRefError::ExecPath),
        ("exec:/usr/../bin/x", SecretRefError::ExecPath),
    ];
    for (text, expected) in cases {
        // when
        let result = text.parse::<SecretRef>();
        // then
        assert_eq!(result, Err(expected), "{text:?}");
    }
}

#[test]
fn should_refuse_an_env_name_when_it_is_longer_than_255_bytes() {
    // given
    let long = format!("env:{}", "A".repeat(256));
    let longest = format!("env:{}", "A".repeat(255));
    // when
    let (refused, accepted) = (long.parse::<SecretRef>(), longest.parse::<SecretRef>());
    // then
    assert_eq!(refused, Err(SecretRefError::EnvName));
    assert!(accepted.is_ok());
}

#[test]
fn should_not_echo_the_input_when_a_reference_is_refused() {
    // given
    let pasted = [
        "ghp_SYNTHETICTOKEN0123456789",
        "-----BEGIN PRIVATE KEY-----",
        "file:SYNTHETICSECRET",
        "env:SYNTHETIC SECRET",
        "exec:SYNTHETICSECRET arg",
        "keychain:SYNTHETICSECRET",
    ];
    for text in pasted {
        // when
        let error = text.parse::<SecretRef>().unwrap_err();
        // then
        let shown = format!("{error} {error:?}");
        assert!(!shown.contains("SYNTHETIC"), "{shown}");
        assert!(!shown.contains("BEGIN"), "{shown}");
    }
}
