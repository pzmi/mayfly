//! Diagnostics of the architecture checker, rendered as `ARCH-<rule>: <message> (at <path>)`.
//!
//! The path is relative to the checked workspace root, never absolute.

use std::fmt;

pub use super::rules::Rule;

/// One violation.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Diagnostic {
    /// The violated rule.
    pub rule: Rule,
    /// What is wrong, naming the crates involved.
    pub message: String,
    /// The offending file, relative to the workspace root.
    pub at: String,
}

impl Diagnostic {
    /// Builds a diagnostic.
    pub fn new(rule: Rule, message: impl Into<String>, at: impl Into<String>) -> Self {
        Self {
            rule,
            message: message.into(),
            at: at.into(),
        }
    }
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {} (at {})", self.rule.id(), self.message, self.at)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_render_rule_message_and_path_when_displayed() {
        // given
        let diagnostic = Diagnostic::new(
            Rule::UnknownCrate,
            "crate `extra` is in no policy category",
            "crates/extra/Cargo.toml",
        );

        // when
        let line = diagnostic.to_string();

        // then
        assert_eq!(
            line,
            "ARCH-unknown-crate: crate `extra` is in no policy category (at crates/extra/Cargo.toml)"
        );
    }
}
