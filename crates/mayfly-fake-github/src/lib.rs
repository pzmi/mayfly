//! `mayfly-fake-github` (test plane): Test-only: a Scale Set and REST fake server, an
//! independent wire implementation.
//!
//! Built from `doc/github-scaleset.md` (ADR 0048), never from `mayfly-adapter-github`'s types
//! (D11, foundation.md §2.6): it parses requests and checks credentials with its own code, so a
//! production bug and this oracle cannot agree by construction. The RS256 check itself is
//! `aws-lc-rs`'s `UnparsedPublicKey` (a third-party primitive, not mayfly code); test keys are
//! generated at run time by the independent `rsa` crate, as PKCS#1 as GitHub hands them out.
//!
//! GH-02 brings the auth chain of spec §1:
//!
//! - `POST /app/installations/{id}/access_tokens`: an App JWT, verified (RS256 with the App's
//!   public key, issuer, issued-at not in the future, expiry in the future and at most ten
//!   minutes after issue and from now) → 201 installation token, its expiry and the
//!   installation's permissions.
//! - `POST /orgs/{org}/actions/runners/registration-token` (or `/repos/{owner}/{repo}/...`): a
//!   live installation token → 201 registration token and expiry.
//! - `POST /actions/runner-registration`: `RemoteAuth <registration token>` and the body
//!   `{"url": <config URL>, "runner_event": "register"}` → 200 Actions service URL and admin
//!   token, a JWT whose `exp` is [`FakeConfig::admin_token_lifetime_secs`] away, short to
//!   exercise refresh (spec §10).
//!
//! Faults ([`Fault`]) replace the next answers of one route; a [`Gate`] holds a route's requests
//! until the test releases them. [`FakeGithub::handle`] answers in process, so a client test
//! runs under a paused Tokio clock with no socket; [`FakeServer`] serves the same fake on a
//! loopback port. GH-01 extends both with the Scale Set routes.

mod keys;
mod server;
mod state;
mod timefmt;
mod token;

pub use keys::{AppKeyPair, generate_app_key};
pub use server::FakeServer;
pub use state::{
    Counts, FakeConfig, FakeGithub, FakeRequest, FakeResponse, Fault, Gate, JwtClaims, Route, Scope,
};
