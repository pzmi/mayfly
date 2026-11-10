//! The child-environment probe (ADR 0058): every child in the product gets a cleared
//! environment plus an explicit allowlist, so a contract of any port that runs a child (the
//! secrets contract's EX-04 today; a data-plane process runner later) checks it the same way.
//!
//! The test picks a variable of its own environment that a clean child must not see
//! ([`inherited_variable`]), runs [`probe_script`] as the child's `/bin/sh` body, and expects
//! [`NOT_INHERITED`] on its stdout. Pinned by `mayfly-adapter-secrets/tests/contract.rs`
//! (`ShellExecProvider`, which inherits the environment, fails EX-04).

/// What [`probe_script`] prints when the variable did not reach the child.
pub const NOT_INHERITED: &str = "not-inherited";

/// Variables a shell may set by itself when they are absent from its environment, so their
/// presence in a child proves nothing.
const SHELL_SET: &[&str] = &[
    "PATH",
    "PWD",
    "OLDPWD",
    "SHLVL",
    "_",
    "IFS",
    "PS1",
    "PS2",
    "PS4",
    "OPTIND",
    "PPID",
    "HOME",
    "ENV",
    "LINENO",
    "MAIL",
    "MAILCHECK",
];

/// A variable of the calling process's environment that a clean child must not see, or `None`
/// when there is none (the caller then fails its case: no case is skipped).
#[must_use]
pub fn inherited_variable() -> Option<String> {
    std::env::vars_os()
        .filter_map(|(name, _)| name.into_string().ok())
        .filter(|name| {
            let mut chars = name.chars();
            chars
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
                && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
        })
        .find(|name| !SHELL_SET.contains(&name.as_str()))
}

/// A `/bin/sh` body printing `name`'s value in the child, or [`NOT_INHERITED`] when it is unset.
/// `name` comes from [`inherited_variable`], a plain identifier.
#[must_use]
pub fn probe_script(name: &str) -> String {
    format!("printf '%s' \"${{{name}-{NOT_INHERITED}}}\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_pick_a_plain_name_the_shell_does_not_set_when_the_environment_has_one() {
        // given: a test process always has variables of its own (the runner's, cargo's)

        // when
        let name = inherited_variable();

        // then
        let name = name.expect("a variable");
        assert!(!SHELL_SET.contains(&name.as_str()));
        assert!(name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'));
    }

    #[test]
    fn should_print_the_marker_when_the_probe_names_an_unset_variable() {
        // given
        let name = "SOME_NAME";

        // when
        let body = probe_script(name);

        // then
        assert_eq!(body, "printf '%s' \"${SOME_NAME-not-inherited}\"");
    }
}
