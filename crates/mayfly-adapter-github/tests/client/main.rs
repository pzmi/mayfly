//! The client against `mayfly-fake-github` (spec §10): the auth chain and its refresh, the
//! retry policy, redaction and metrics in process under a paused Tokio clock, and the
//! `reqwest` transport over a loopback socket.

mod auth;
mod retry;
mod socket;
mod support;
