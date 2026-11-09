//! The invocation log: `$TART_HOME/invocations.jsonl`, one JSON object per line, append-only.
//!
//! Every record has `v` (schema version, 1), `ts_ms` and `inv` (the invocation's id, unique per
//! `$TART_HOME`) plus an `event`:
//!
//! | `event`  | extra fields | written |
//! |:--|:--|:--|
//! | `invoke` | `pid`, `argv` (without argv[0]), `env` (every variable name; its value only for `TART_HOME` and `MAYFLY_FAKE_*`, `null` for the rest) | first, before parsing |
//! | `stdin`  | `len`, `sha256` | after an `exec -i` has read stdin; the content is never stored |
//! | `fault`  | `rule`, `action`, `phase` (`before_effect`, `after_effect`) | at the moment a fault takes effect |
//! | `effect` | `kind`, `vm` | when the command has changed state |
//! | `exit`   | `code` | last, when the process exits by itself |
//!
//! A hang or a crash writes `fault` and no `exit`; that absence is how the lab tells them from a
//! failure. `fault` with `action = "delay"` records a delay, `phase` `before_effect`.
//!
//! The log never holds an environment value outside the allowlist: a CI or operator environment can
//! carry tokens, and this file may be kept as test evidence. A test that must show a secret never
//! reached a child's environment checks the variable *names* here against the adapter's allowlist.
//!
//! Pinned by: `tests/cli/commands.rs::should_never_log_an_environment_value_when_the_variable_is_not_allowlisted`,
//! `tests/cli/commands.rs::should_record_what_the_guest_received_without_its_content_when_exec_reads_stdin`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The log's schema version.
pub(crate) const LOG_VERSION: u32 = 1;

/// One log line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Record {
    pub v: u32,
    pub ts_ms: u64,
    pub inv: u64,
    #[serde(flatten)]
    pub event: Event,
}

/// The event a record describes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub(crate) enum Event {
    Invoke {
        pid: u32,
        argv: Vec<String>,
        env: BTreeMap<String, Option<String>>,
    },
    Stdin {
        len: u64,
        sha256: String,
    },
    Fault {
        rule: String,
        action: String,
        phase: String,
    },
    Effect {
        kind: String,
        vm: String,
    },
    Exit {
        code: i32,
    },
}

impl Record {
    /// A record for invocation `inv` at `ts_ms`.
    pub fn new(inv: u64, ts_ms: u64, event: Event) -> Self {
        Self {
            v: LOG_VERSION,
            ts_ms,
            inv,
            event,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Event, Record};

    #[test]
    fn should_flatten_the_event_into_the_record_when_serialised() {
        // given
        let record = Record::new(3, 10, Event::Exit { code: 2 });

        // when
        let text = serde_json::to_string(&record).unwrap();

        // then
        assert_eq!(
            text,
            r#"{"v":1,"ts_ms":10,"inv":3,"event":"exit","code":2}"#
        );
        assert_eq!(serde_json::from_str::<Record>(&text).unwrap(), record);
    }
}
