//! The client against `mayfly-fake-github` (spec §10): the auth chain and its refresh, the
//! retry policy, redaction and metrics in process under a paused Tokio clock, and the
//! `reqwest` transport over a loopback socket; the Scale Set client against the ports' contract
//! over the fake and over the recorded fixtures.

mod auth;
mod contract;
mod replay;
mod retry;
mod scaleset;
mod socket;
mod support;
