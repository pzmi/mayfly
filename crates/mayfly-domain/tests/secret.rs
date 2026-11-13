//! `Secret<T>` never prints its value (ADR 0014, plan §7.1: "a test proves it"), and
//! implements no `Serialize`, which the compiler checks below.

use mayfly_domain::Secret;

/// Resolves without ambiguity for a type that does not implement `Serialize`: only the
/// `()` impl applies. Were `Secret<T>` to implement `Serialize` (a derive, a hand-written
/// impl or a blanket impl), the `u8` impl would apply too, `<Secret<_> as NotSerialize<_>>`
/// would be ambiguous, and this test target would not compile. The syntactic rule
/// `ARCH-secret-serialize` keeps every type that holds a secret from serialising it; this
/// keeps the wrapper itself from ever becoming serialisable, without a dependency.
trait NotSerialize<Marker> {
    fn check() {}
}

impl<T: ?Sized> NotSerialize<()> for T {}

impl<T: ?Sized + serde::Serialize> NotSerialize<u8> for T {}

const _: fn() = || {
    let _ = <Secret<String> as NotSerialize<_>>::check;
    let _ = <Secret<Vec<u8>> as NotSerialize<_>>::check;
};

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
