//! The daemon's distinct exit codes (ADR 0031).
//!
//! A service manager restarts an exited daemon; a refused second instance or an invalid config
//! must not be restarted forever, so each has a code of its own and OPS-01's unit file
//! can tell them apart. The values are the BSD `sysexits.h` ones where one fits. They are
//! never renamed or renumbered (compatibility rule); OPS-05 adds the watchdog's.
//! Pinned by `tests/state_dir.rs::should_use_distinct_exit_codes_when_each_refusal_is_mapped`.

/// The configuration is unreadable or invalid (`EX_CONFIG`).
pub const CONFIG_INVALID: u8 = 78;

/// Another instance holds the state-dir lock (`EX_UNAVAILABLE`).
pub const LOCK_REFUSED: u8 = 69;

/// The state dir cannot be used: not a directory, not owned by the service user, wider than
/// `0700`, or an I/O error (`EX_CANTCREAT`).
pub const STATE_DIR_UNUSABLE: u8 = 73;
