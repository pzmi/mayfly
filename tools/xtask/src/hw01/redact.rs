//! Redaction of everything `hw01` writes into its report, before it is written.
//!
//! The report goes back to the repository's maintainers, so it carries no hostname, username,
//! home path, serial number, hardware UUID, token or keychain content: each known value of the
//! host is replaced by its role (`<hostname>`, `<user>`, …), the run's own directory by `$RUN`
//! (so paths are relative to the throwaway home), every MAC by a stable synthetic placeholder per
//! MAC (`02:00:00:00:00:NN`, locally administered, numbered by first sight), any UUID by
//! `<uuid>`, and anything shaped like a token by `<token>`. A value that still holds one of those
//! after redaction ([`Redactor::leak`]) is refused: its check fails and the evidence is dropped
//! ([`Redactor::value`]). Pinned by `should_find_no_planted_identity_when_the_report_is_written`
//! (`hw01::tests`) and this module's tests.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::Path;

use serde_json::Value;

/// What a host knows about itself that must never reach the report.
#[derive(Debug, Clone, Default)]
pub struct Identity {
    /// The host's names (`hostname`, the computer and local host names).
    pub hostnames: Vec<String>,
    /// The user's login name.
    pub username: Option<String>,
    /// The user's home directory.
    pub home: Option<String>,
    /// The hardware serial number.
    pub serial: Option<String>,
    /// The hardware UUID.
    pub hardware_uuid: Option<String>,
    /// The `--service-user` the launchd check runs as.
    pub service_user: Option<String>,
}

/// Replaces what [`Identity`] names, the run directory, MACs, UUIDs and tokens.
#[derive(Debug)]
pub struct Redactor {
    /// (value, placeholder, whole-word only), longest value first.
    exact: Vec<(String, &'static str, bool)>,
    /// The run directory's spellings, longest first.
    run_dirs: Vec<String>,
    macs: RefCell<BTreeMap<String, String>>,
}

/// Every placeholder this module writes.
const PLACEHOLDERS: &[&str] = &[
    "<home>",
    "<serial>",
    "<hardware-uuid>",
    "<user>",
    "<service-user>",
    "<hostname>",
    "<secret>",
    "<uuid>",
    "<token>",
    "$RUN",
];

const TOKEN_PREFIXES: &[&str] = &[
    "ghp_",
    "gho_",
    "ghs_",
    "ghu_",
    "ghr_",
    "github_pat_",
    "glpat-",
    "xoxb-",
    "xoxp-",
    "xoxa-",
    "AKIA",
    "-----BEGIN",
];

impl Redactor {
    /// A redactor for `identity`, the run directory `run_dir`, and `secrets` the run made itself
    /// (keychain passwords and item values).
    pub fn new(identity: &Identity, run_dir: &Path, secrets: &[String]) -> Self {
        let mut exact: Vec<(String, &'static str, bool)> = Vec::new();
        let mut push = |value: &Option<String>, placeholder, word| {
            if let Some(value) = value.as_ref().filter(|v| v.len() >= 2) {
                exact.push((value.clone(), placeholder, word));
            }
        };
        push(&identity.home, "<home>", false);
        push(&identity.serial, "<serial>", false);
        push(&identity.hardware_uuid, "<hardware-uuid>", false);
        push(&identity.username, "<user>", true);
        push(&identity.service_user, "<service-user>", true);
        for name in &identity.hostnames {
            push(&Some(name.clone()), "<hostname>", true);
            // `name.local` and `name.lan` reach logs as often as the bare name.
            if let Some((short, _)) = name.split_once('.') {
                push(&Some(short.to_owned()), "<hostname>", true);
            }
        }
        for secret in secrets {
            push(&Some(secret.clone()), "<secret>", false);
        }
        exact.sort_by_key(|(value, _, _)| std::cmp::Reverse(value.len()));
        let mut run_dirs = vec![run_dir.display().to_string()];
        if let Ok(canonical) = run_dir.canonicalize() {
            run_dirs.push(canonical.display().to_string());
        }
        // macOS spells its temp directory both /var/… and /private/var/….
        let spelled: Vec<String> = run_dirs
            .iter()
            .filter_map(|d| d.strip_prefix("/private").map(str::to_owned))
            .collect();
        run_dirs.extend(spelled);
        run_dirs.sort_by_key(|d| std::cmp::Reverse(d.len()));
        run_dirs.dedup();
        Self {
            exact,
            run_dirs,
            macs: RefCell::new(BTreeMap::new()),
        }
    }

    /// `text` with everything this redactor knows replaced.
    pub fn text(&self, text: &str) -> String {
        replace_tokens(&self.identity_text(text))
    }

    /// `text` with the run's paths, the host's names, MACs and UUIDs replaced; tokens left.
    fn identity_text(&self, text: &str) -> String {
        let mut out = text.to_owned();
        for dir in &self.run_dirs {
            out = out.replace(dir.as_str(), "$RUN");
        }
        for (value, placeholder, word) in &self.exact {
            out = if *word {
                replace_word(&out, value, placeholder)
            } else {
                out.replace(value.as_str(), placeholder)
            };
        }
        out = self.replace_macs(&out);
        replace_uuids(&out)
    }

    /// What `text` still leaks after redaction, if anything.
    pub fn leak(&self, text: &str) -> Option<&'static str> {
        // A placeholder may itself contain a short name ("<user>" for the user `user`).
        let mut text = text.to_owned();
        for placeholder in PLACEHOLDERS {
            text = text.replace(placeholder, " ");
        }
        let text = text.as_str();
        for (value, placeholder, word) in &self.exact {
            let found = if *word {
                find_word(text, value).is_some()
            } else {
                text.contains(value.as_str())
            };
            if found {
                return Some(placeholder);
            }
        }
        if self.run_dirs.iter().any(|d| text.contains(d.as_str())) {
            return Some("a run path");
        }
        let macs = self.macs.borrow();
        let placeholders: Vec<&String> = macs.values().collect();
        if mac_spans(text)
            .iter()
            .any(|(s, e)| !placeholders.iter().any(|p| p.as_str() == &text[*s..*e]))
        {
            return Some("a MAC");
        }
        if !uuid_spans(text).is_empty() {
            return Some("a UUID");
        }
        if !token_spans(text).is_empty() {
            return Some("a token");
        }
        None
    }

    /// Whether `text` holds a token or a secret the run made (a keychain password or item):
    /// evidence that does is never redacted and kept, it fails its check.
    pub fn has_secret(&self, text: &str) -> bool {
        !token_spans(&self.identity_text(text)).is_empty()
            || self.exact.iter().any(|(value, placeholder, _)| {
                *placeholder == "<secret>" && text.contains(value.as_str())
            })
    }

    /// `value` with every string (keys included) redacted: names, paths, MACs and UUIDs are
    /// replaced.
    ///
    /// # Errors
    /// A string holds a token or a secret ([`Self::has_secret`]), or still leaks after
    /// redaction: the caller drops the evidence and fails the check.
    pub fn value(&self, value: &Value) -> Result<Value, &'static str> {
        Ok(match value {
            Value::String(s) => {
                if self.has_secret(s) {
                    return Err("a token or a secret the run made");
                }
                let redacted = self.text(s);
                if let Some(leak) = self.leak(&redacted) {
                    return Err(leak);
                }
                Value::String(redacted)
            }
            Value::Array(items) => Value::Array(
                items
                    .iter()
                    .map(|v| self.value(v))
                    .collect::<Result<_, _>>()?,
            ),
            Value::Object(map) => {
                let mut out = serde_json::Map::new();
                for (k, v) in map {
                    if self.has_secret(k) {
                        return Err("a token or a secret the run made");
                    }
                    let key = self.text(k);
                    if let Some(leak) = self.leak(&key) {
                        return Err(leak);
                    }
                    out.insert(key, self.value(v)?);
                }
                Value::Object(out)
            }
            other => other.clone(),
        })
    }

    fn replace_macs(&self, text: &str) -> String {
        let spans = mac_spans(text);
        if spans.is_empty() {
            return text.to_owned();
        }
        let mut macs = self.macs.borrow_mut();
        let mut out = String::with_capacity(text.len());
        let mut at = 0;
        for (start, end) in spans {
            out.push_str(&text[at..start]);
            let real = text[start..end].to_ascii_lowercase();
            let next = macs.len() + 1;
            let synthetic = if macs.values().any(|p| p == &real) {
                // Already a placeholder this redactor handed out.
                real
            } else {
                macs.entry(real)
                    .or_insert_with(|| format!("02:00:00:00:{:02x}:{:02x}", next / 256, next % 256))
                    .clone()
            };
            out.push_str(&synthetic);
            at = end;
        }
        out.push_str(&text[at..]);
        out
    }
}

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
}

pub(crate) fn find_word(text: &str, word: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut from = 0;
    while let Some(i) = text[from..].find(word) {
        let start = from + i;
        let end = start + word.len();
        let before = start == 0 || !is_word_byte(bytes[start - 1]);
        let after = end == bytes.len() || !is_word_byte(bytes[end]);
        if before && after {
            return Some(start);
        }
        from = start + word.len().max(1);
        if from >= text.len() {
            break;
        }
    }
    None
}

fn replace_word(text: &str, word: &str, placeholder: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = find_word(rest, word) {
        out.push_str(&rest[..start]);
        out.push_str(placeholder);
        rest = &rest[start + word.len()..];
    }
    out.push_str(rest);
    out
}

fn is_hex(b: u8) -> bool {
    b.is_ascii_hexdigit()
}

/// Spans of `xx:xx:xx:xx:xx:xx` (either case), not inside a longer hex run.
fn mac_spans(text: &str) -> Vec<(usize, usize)> {
    let bytes = text.as_bytes();
    let mut spans = Vec::new();
    let mut i = 0;
    while i + 17 <= bytes.len() {
        let window = &bytes[i..i + 17];
        let shaped = (0..17).all(|k| {
            if k % 3 == 2 {
                window[k] == b':'
            } else {
                is_hex(window[k])
            }
        });
        let bounded = (i == 0 || !(is_hex(bytes[i - 1]) || bytes[i - 1] == b':'))
            && (i + 17 == bytes.len() || !(is_hex(bytes[i + 17]) || bytes[i + 17] == b':'));
        if shaped && bounded {
            spans.push((i, i + 17));
            i += 17;
        } else {
            i += 1;
        }
    }
    spans
}

/// Spans of `8-4-4-4-12` hex UUIDs.
fn uuid_spans(text: &str) -> Vec<(usize, usize)> {
    const GROUPS: [usize; 5] = [8, 4, 4, 4, 12];
    let bytes = text.as_bytes();
    let mut spans = Vec::new();
    let mut i = 0;
    while i + 36 <= bytes.len() {
        let mut k = i;
        let mut ok = i == 0 || !is_hex(bytes[i - 1]);
        for (g, len) in GROUPS.iter().enumerate() {
            ok = ok && bytes[k..k + len].iter().all(|b| is_hex(*b));
            k += len;
            if g < 4 {
                ok = ok && bytes[k] == b'-';
                k += 1;
            }
        }
        ok = ok && (k == bytes.len() || !is_hex(bytes[k]));
        if ok {
            spans.push((i, k));
            i = k;
        } else {
            i += 1;
        }
    }
    spans
}

fn replace_spans(text: &str, spans: &[(usize, usize)], placeholder: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    for (start, end) in spans {
        out.push_str(&text[at..*start]);
        out.push_str(placeholder);
        at = *end;
    }
    out.push_str(&text[at..]);
    out
}

fn replace_uuids(text: &str) -> String {
    replace_spans(text, &uuid_spans(text), "<uuid>")
}

fn is_token_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'+' | b'/' | b'=' | b'.')
}

/// Spans that look like credentials: a known token prefix, a `Bearer` credential, a JWT, or a
/// long run that mixes upper case, lower case and digits (hex digests, all lower case, are not).
fn token_spans(text: &str) -> Vec<(usize, usize)> {
    let bytes = text.as_bytes();
    let mut spans = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if !is_token_byte(bytes[i]) || (i > 0 && is_token_byte(bytes[i - 1])) {
            i += 1;
            continue;
        }
        let mut end = i;
        while end < bytes.len() && is_token_byte(bytes[end]) {
            end += 1;
        }
        let run = &text[i..end];
        let mixed = run.len() >= 32
            && run.bytes().any(|b| b.is_ascii_uppercase())
            && run.bytes().any(|b| b.is_ascii_lowercase())
            && run.bytes().any(|b| b.is_ascii_digit());
        let prefixed = TOKEN_PREFIXES.iter().any(|p| run.starts_with(p)) && run.len() > 8;
        let jwt = run.starts_with("eyJ") && run.contains('.') && run.len() > 20;
        let bearer = text[..i].ends_with("Bearer ") || text[..i].ends_with("bearer ");
        if mixed || prefixed || jwt || bearer {
            spans.push((i, end));
        }
        i = end;
    }
    spans
}

fn replace_tokens(text: &str) -> String {
    replace_spans(text, &token_spans(text), "<token>")
}

/// A token-shaped value for tests, built at run time so no literal in the repository matches a
/// secret-scanner rule (the repository's policy: fake secrets are made when the test runs).
#[cfg(test)]
pub(crate) fn planted_token() -> String {
    let prefix: String = ['g', 'h', 'p', '_'].iter().collect();
    format!("{prefix}{}{}", "0123456789abcdef", "ABCDEF".repeat(3))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity() -> Identity {
        Identity {
            hostnames: vec!["owners-mac.local".to_owned()],
            username: Some("pat".to_owned()),
            home: Some("/Users/pat".to_owned()),
            serial: Some("C02XK1ABJG5H".to_owned()),
            hardware_uuid: Some("A1B2C3D4-0000-1111-2222-333344445555".to_owned()),
            service_user: None,
        }
    }

    #[test]
    fn should_replace_each_mac_by_a_stable_placeholder_when_text_is_redacted() {
        // given
        let redactor = Redactor::new(&identity(), Path::new("/tmp/run"), &[]);

        // when
        let first = redactor.text("vm a=8E:22:11:00:AA:01 b=8e:22:11:00:aa:02");
        let again = redactor.text("a again: 8e:22:11:00:aa:01");

        // then
        assert_eq!(first, "vm a=02:00:00:00:00:01 b=02:00:00:00:00:02");
        assert_eq!(again, "a again: 02:00:00:00:00:01");
        assert_eq!(redactor.leak(&first), None);
        assert_eq!(redactor.leak("8e:22:11:00:aa:03"), Some("a MAC"));
    }

    #[test]
    fn should_keep_digests_and_replace_tokens_when_text_is_redacted() {
        // given
        let redactor = Redactor::new(&identity(), Path::new("/tmp/run"), &[]);
        let digest = "sha256:97166c84ea5f102be33d2959f70e83861a36bd749f1715ab669d93e6c9f31476";

        // when
        let text = redactor.text(&format!(
            "{digest} Authorization: Bearer abc.def {} /tmp/run/home/vms",
            planted_token()
        ));

        // then
        assert!(text.contains(digest), "{text}");
        assert!(text.contains("Bearer <token>"), "{text}");
        assert!(!text.contains("ghp_"), "{text}");
        assert!(text.contains("$RUN/home/vms"), "{text}");
    }

    #[test]
    fn should_replace_names_only_as_whole_words_when_they_are_short() {
        // given
        let redactor = Redactor::new(&identity(), Path::new("/tmp/run"), &[]);

        // when
        let text = redactor.text("user pat ran on owners-mac, path /Users/pat/x, compat stays");

        // then
        assert_eq!(
            text,
            "user <user> ran on <hostname>, path <home>/x, compat stays"
        );
    }

    #[test]
    fn should_replace_identity_and_refuse_secrets_when_a_value_is_redacted() {
        // given
        let redactor = Redactor::new(&identity(), Path::new("/tmp/run"), &["s3cr3t-item".into()]);
        let named = serde_json::json!({
            "C02XK1ABJG5H": ["A1B2C3D4-0000-1111-2222-333344445555", "vm 0a:1b:2c:3d:4e:5f"],
        });
        let secret = serde_json::json!(["read: s3cr3t-item"]);

        // when
        let redacted = redactor.value(&named).expect("redactable");
        let refused = redactor.value(&secret);

        // then
        let text = redacted.to_string();
        assert!(
            text.contains("<serial>") && text.contains("<hardware-uuid>"),
            "{text}"
        );
        assert!(text.contains("02:00:00:00:00:01"), "{text}");
        assert!(refused.is_err());
    }
}
