//! `mayfly-fake-github` (test plane): a Scale Set and REST fake server, an independent wire
//! implementation of `doc/github-scaleset.md`.
//!
//! Built from the spec (ADR 0048), never from upstream's tests, test server or fixtures, and
//! never from `mayfly-adapter-github`'s types (D11, foundation.md §2.6): it parses requests and
//! checks credentials with its own code, so a production bug and this oracle cannot agree by
//! construction. The RS256 check itself is `aws-lc-rs`'s `UnparsedPublicKey` (a third-party
//! primitive, not mayfly code); test keys are generated at run time by the independent `rsa`
//! crate, as PKCS#1 as GitHub hands them out.
//!
//! What it serves ([`Route`]):
//!
//! - **The auth chain** (spec §1): an App JWT, verified (RS256, issuer, issued-at, at most ten
//!   minutes) → installation token → registration token (an auth step only, ADR 0028) →
//!   runner-registration exchange → the Actions service URL and an admin token, an RS256 JWT
//!   signed with [`FakeConfig::admin_token_key_pem`], whose `exp` is
//!   [`FakeConfig::admin_token_lifetime_secs`] away, short to exercise refresh.
//! - **The Actions service** (spec §2–§7), on a live admin token: runner group and scale-set
//!   lookup, scale-set create and update, one live session per scale set (409
//!   `RunnerScaleSetSessionConflictException` on a second), refresh (400
//!   `RunnerScaleSetSessionExpiredException` once expired), delete; the long poll on the queue
//!   token with the 202 hold, redelivery until the ack, at most
//!   [`Knobs::max_jobs_per_message`] job messages per message and the statistics; both flows of
//!   spec §6.1 ([`Flow`]): github.com's direct `JobAssigned` (`runnerRequestId` 0, nothing
//!   assigned beyond the last poll's `X-ScaleSetMaxCapacity`, so 0 assigns nothing) and GHES's
//!   offers with `acquirejobs` and `acquirablejobs`; release and requeue of an assigned job no
//!   runner takes, failed at the third release; JIT configs (409 `AgentExistsException` for a
//!   taken name); runner get, list and remove (204 also for an absent runner, a busy one refused
//!   with `JobStillRunningException`); dormant deletion of a scale set.
//! - **The REST trust check** of ADR 0090 (spec §6.2), on the installation token and its
//!   permissions: the workflow-run lookup (a fork's run has a `head_repository` other than its
//!   `repository`), the cancel (202, or 409 for a run that is over), the fork-PR setting and the
//!   runner-group read.
//! - **The control API** (`/_fake/...`, and the same as methods of [`FakeGithub`]): queue,
//!   start, complete, release and cancel jobs; expire a session or its token; delete a dormant
//!   scale set; restart a numbering; queue faults; change knobs; dump the state as JSON.
//!
//! Every behaviour the spec leaves client-assumed or open is a field of [`Knobs`] named after
//! its HW-02 item. Faults ([`Fault`]) replace or bend the next answers of one route (a status
//! with `Retry-After`, a rate-limited 403, a 200 with an empty body, a stall, a response dropped
//! after its effect, a delay); a [`Gate`] holds a route's requests until the test releases them.
//! [`FakeGithub::handle`] answers in process, so a client test runs under a paused Tokio clock
//! with no socket; [`FakeServer`] serves the same fake on a loopback port, and the
//! `mayfly-fake-github` binary serves it as a process.
//!
//! [`contract`] holds the wire contract suite every implementation of the spec must pass: this
//! fake, and (owner-gated, HW-02) a live disposable organisation.

pub mod contract;
mod control;
mod keys;
mod knobs;
mod rest;
mod routes;
mod server;
mod service;
mod state;
mod timefmt;
mod token;
mod wait;
mod wire;
mod world;

pub use control::ControlError;
pub use keys::{AppKeyPair, generate_app_key};
pub use knobs::{
    AbsentRemoval, Flow, ForkPrSetting, Knobs, PartialAcquire, RunnerGroup, StatisticsAt,
};
pub use routes::Route;
pub use server::FakeServer;
pub use state::{
    Counts, FakeConfig, FakeGithub, FakeRequest, FakeResponse, Fault, Gate, JwtClaims, Scope,
};
pub use world::{
    Job, JobSpec, JobState, Label, Message, Run, Runner, ScaleSet, Session, SessionState,
    Statistics, World, ZERO_TIME,
};
