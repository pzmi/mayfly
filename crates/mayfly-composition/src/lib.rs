//! `mayfly-composition` (shared plane): config, wiring, runtime, supervision tree, startup
//! sequence and roles.
//!
//! Today: the configuration schema and its validation ([`config`], plan §11) and the state-dir
//! lock ([`state_dir`], plan §13). Wiring arrives with RT-02.

pub mod config;
pub mod exit_code;
pub mod state_dir;
