//! `Secret<T>` never prints its value (ADR 0014, plan §7.1: "a test proves it").

use mayfly_domain::Secret;

#[derive(Debug)]
struct Holder {
    name: &'static str,
    key: Secret<String>,
}

#[test]
fn should_print_redacted_when_a_secret_is_formatted_with_debug_or_display() {
    // given
    let secret = Secret::new(String::from("synthetic-value-7f3a"));

    // when
    let shown = [
        format!("{secret:?}"),
        format!("{secret:#?}"),
        format!("{secret}"),
    ];

    // then
    assert_eq!(
        shown,
        ["Secret(<redacted>)", "Secret(<redacted>)", "<redacted>"]
    );
}

#[test]
fn should_print_redacted_when_a_type_holding_a_secret_derives_debug() {
    // given
    let holder = Holder {
        name: "app-key",
        key: Secret::new(String::from("synthetic-value-7f3a")),
    };

    // when
    let shown = format!("{holder:?} {holder:#?}");

    // then
    assert!(!shown.contains("synthetic-value-7f3a"), "{shown}");
    assert!(shown.contains(holder.name), "{shown}");
    assert_eq!(holder.key.expose().len(), 20);
}

#[test]
fn should_hand_the_value_back_when_it_is_exposed() {
    // given
    let secret = Secret::new(vec![1_u8, 2, 3]);

    // when
    let value = secret.expose();

    // then
    assert_eq!(value, &[1, 2, 3]);
}
