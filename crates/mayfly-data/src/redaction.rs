//! What a guest's exported runner log may keep (plan §8.3: diagnostics bounded and redacted).
//!
//! Each line has its control characters (but tab) dropped first, so an escape sequence cannot
//! split a secret past the patterns, then is masked whole as [`REDACTED_LINE`] when it names the
//! JIT variable, holds a value the operator configured as secret, a GitHub token (`ghs_`,
//! `ghr_`, `ghp_`, `gho_`, `ghu_`, `github_pat_`), a `Bearer` credential, or a run of at least
//! [`BLOB_RUN`] base64 characters (a JIT config is base64). One function serves the Tart
//! transport and the in-memory guests, so the fake masks exactly what the real one does.
//! Pinned by the tests below and the `GuestTransport` contract's GT-10.

/// What masks a redacted line.
pub const REDACTED_LINE: &str = "[redacted]";

/// The variable the launcher hands the JIT config to the runner in (`doc/github-scaleset.md`,
/// guest side).
pub const JIT_VARIABLE: &str = "ACTIONS_RUNNER_INPUT_JITCONFIG";

/// The shortest run of base64 characters masked as a possible secret.
pub const BLOB_RUN: usize = 32;

const TOKEN_PREFIXES: &[&str] = &["ghs_", "ghr_", "ghp_", "gho_", "ghu_", "github_pat_"];

fn base64ish(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '=' | '-' | '_')
}

fn longest_blob(line: &str) -> usize {
    line.split(|c: char| !base64ish(c))
        .map(str::len)
        .max()
        .unwrap_or(0)
}

fn secret(line: &str, masked: &[String]) -> bool {
    line.contains(JIT_VARIABLE)
        || masked
            .iter()
            .any(|m| !m.is_empty() && line.contains(m.as_str()))
        || TOKEN_PREFIXES.iter().any(|p| line.contains(p))
        || line.to_ascii_lowercase().contains("bearer ")
        || longest_blob(line) >= BLOB_RUN
}

/// `log` as it may be kept (see the module).
#[must_use]
pub fn redact_log(log: &[u8], masked: &[String]) -> String {
    String::from_utf8_lossy(log)
        .lines()
        .map(|line| {
            let clean: String = line
                .chars()
                .filter(|c| *c == '\t' || !c.is_control())
                .collect();
            if secret(&clean, masked) {
                REDACTED_LINE.to_owned()
            } else {
                clean
            }
        })
        .fold(String::new(), |mut out, line| {
            out.push_str(&line);
            out.push('\n');
            out
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_mask_each_kind_of_secret_line_when_redacting_a_log() {
        // given
        let log = "start\n\
            ACTIONS_RUNNER_INPUT_JITCONFIG=abc\n\
            token ghs_x1\n\
            pat github_pat_11\n\
            Authorization: bearer abc\n\
            bWF5Zmx5LWNhbmFyeS1qaXQtY29uZmlnLTdmM2E5\n\
            configured s3cret here\n\
            short base64 aGVsbG8=\n";
        let masked = vec!["s3cret".to_owned(), String::new()];

        // when
        let kept = redact_log(log.as_bytes(), &masked);

        // then
        assert_eq!(
            kept,
            "start\n[redacted]\n[redacted]\n[redacted]\n[redacted]\n[redacted]\n[redacted]\nshort base64 aGVsbG8=\n"
        );
    }

    #[test]
    fn should_match_after_dropping_control_characters_when_an_escape_splits_a_secret() {
        // given
        let log = b"ACTIONS_RUNNER_INPUT\x1b_JITCONFIG=x\n\x1b[31mred\tok\ngh\x07s_token\n";

        // when
        let kept = redact_log(log, &[]);

        // then
        assert_eq!(kept, "[redacted]\n[31mred\tok\n[redacted]\n");
    }
}
