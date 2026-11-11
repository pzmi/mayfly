# 0064. GitHub client crypto and TLS: aws-lc-rs and rustls, no JWT crate

Date: 2026-11-10

## Status

Accepted (owner, 2026-11-10). Amends the `mayfly-adapter-github` and `mayfly-fake-github` rows
of foundation.md §2.4 (marked there) and adds `[graph] targets` to `deny.toml`. A first draft the
same day chose `ring` and `reqwest` 0.12; it was replaced before anything landed.

## Context

GH-02 needs TLS to GitHub and RS256 App JWTs (spec §1). Foundation.md §2.4 allowed `reqwest`
(`rustls-tls`) and "a JWT crate (RS256)". What the options were on 2026-11-10 (sources: the
`reqwest` repository's CHANGELOG and Cargo.toml, crates.io, the `rustls` README, the
`rustls-platform-verifier` and `rustls-pki-types` documentation, the `aws-lc-rs` book, `ring`
discussion 2414, the RustSec advisory database, GitHub's "Generating a JSON Web Token for a
GitHub App" documentation):

- **HTTP.** `reqwest` 0.12 is frozen (last release 0.12.28, 2025-12-22); 0.13 is current. Its
  `rustls-no-provider` feature takes a preconfigured `rustls::ClientConfig`
  (`tls_backend_preconfigured`), so no process-global `CryptoProvider::install_default` is
  needed, and it always brings `rustls-platform-verifier`.
- **Trust.** `rustls-native-certs` only loads root certificates and makes no trust decision:
  on macOS it ignores Keychain and MDM trust settings, name constraints and revocation.
  `rustls-platform-verifier` 0.7 asks the operating system (Security.framework on macOS, the
  system store with webpki on Linux) and takes extra roots (`new_with_extra_roots`) for a GHES
  private CA. Its only dependency outside `deny.toml`'s licences, `webpki-root-certs`
  (CDLA-Permissive-2.0), is a `wasm32`-only dependency, which `cargo deny` counted only because
  no target list was set.
- **Crypto provider.** The `rustls` team recommends `aws-lc-rs`, and `rustls` 0.24 requires an
  explicit provider. `aws-lc-sys` 0.39 and later dropped the OpenSSL licence: 0.47.0 is
  `ISC AND (Apache-2.0 OR ISC) AND Apache-2.0 AND MIT AND BSD-3-Clause AND (Apache-2.0 OR ISC
  OR MIT) AND (Apache-2.0 OR ISC OR MIT-0)`, every term within `deny.toml` (the `MIT-0` term
  only as an alternative). `ring` is maintained again but has not released since 2025-03 and
  does not zeroize. `aws-lc-rs` signs RSA PKCS#1 v1.5 and parses PKCS#1 and PKCS#8 keys, and
  `aws-lc` frees key memory with a cleansing `OPENSSL_free` (read in `crypto/mem.c`).
- **JWT.** `jsonwebtoken` 10+ needs `aws_lc_rs` or `rust_crypto` (`rsa`, `p256`, `p384`,
  `ed25519-dalek`, `hmac` for one algorithm); a JWT is three base64url parts, a few lines by
  hand. `rustls-pemfile` is unmaintained; `rustls-pki-types` 1.15 parses PEM and implements
  `Zeroize` for private keys.
- **Advisories.** Minimums: `rustls` 0.23.46, `rustls-webpki` 0.103.16, `aws-lc-sys` 0.39,
  `h2` 0.4.16 and `webpki-roots` 1.0.10 where present; `rsa` before 0.10.0 has
  RUSTSEC-2023-0071.

## Decision

1. **HTTP:** `reqwest` 0.13.5 with default features off and only `rustls-no-provider`, given its
   `rustls` client through `tls_backend_preconfigured`. Redirects off, no proxy read from the
   environment (config says where GitHub is), a connect timeout, a bounded body. HTTP/2 is left
   off: `http2` adds seven crates (`h2`, `indexmap` and theirs), and HTTP/1.1 serves the auth
   chain and a long poll.
2. **TLS:** `rustls` 0.23.46 (`aws_lc_rs`, `std`, `tls12`) with `aws-lc-rs`'s provider and
   `rustls-platform-verifier` 0.7.1; `ReqwestTransport::with_extra_roots` takes a GHES private
   CA, read from a config path by RT-01. No `rustls-native-certs`, no bundled roots.
3. **One provider:** `aws-lc-rs` 1.19 (`aws-lc-sys` 0.47) for TLS and for the RS256 signature.
   `ring` is no longer in the graph. The swap back is one line in each of `transport.rs` and
   `jwt.rs` if `aws-lc-sys` ever fails to build.
4. **RS256 by hand:** the adapter builds the App JWT (`iat` set back 60 s, `exp = iat + 600`,
   `iss` = the App's client id, as spec §1 and GitHub's documentation recommend) and signs it
   with `RSA_PKCS1_SHA256`, from a PEM parsed by `rustls-pki-types` whose DER is zeroized after
   parsing. It reads the admin token's `exp` unverified (spec §1).
5. **The fake** checks RS256 with `aws-lc-rs`'s `UnparsedPublicKey` through its own JWT and PEM
   code, and generates PKCS#1 test keys at run time with `rsa` 0.10.0, a different
   implementation from the provider; no private key is committed (`.gitleaks.toml`).
6. **`deny.toml`** checks the four shipped targets (`[graph] targets`); the licence list is
   unchanged.
7. **The cross-check stage** lints `aarch64-apple-darwin` on Linux without the macOS SDK, which
   `aws-lc-sys`'s C needs: as for `ring` before, `true` stands in for the target's C compiler
   and archiver (measured: the build script then succeeds, and fails without it), so every Rust
   line is linted and the `macos` CI job compiles the C natively.

## Consequences

- No OpenSSL, `native-tls` or `ring`; the adapter's normal tree is 99 crates (100 with `ring`
  and `reqwest` 0.12), a clean debug build of the adapter took 30 s before and 44 s after
  (`aws-lc-sys` compiles a larger C library; measured with two jobs).
- A larger C surface (`aws-lc`) whose advisories `cargo deny` follows; the platform verifier
  may do synchronous work and network fetches in the handshake, and does no revocation on Linux.
- An HTTP proxy is not supported until a task adds it as a config field.
- The hand-written JWT owns its claims and clock logic, pinned by tests.
- Pinned by `architecture.toml` (`[external.normal]` rows for both crates),
  `crates/mayfly-adapter-github/tests/client/auth.rs::should_mint_an_admin_connection_when_the_chain_succeeds`
  (the fake checks signature and claims),
  `crates/mayfly-adapter-github/src/jwt.rs::tests::should_sign_three_parts_when_the_key_is_pkcs8`
  and
  `tools/xtask/src/check.rs::should_skip_only_the_c_builds_of_sqlite_and_aws_lc_when_cross_check_runs`.

## Alternatives considered

- `reqwest` 0.12 with `rustls-native-certs`: frozen upstream, and no OS trust decisions.
- `ring` as the provider: works, smaller build, but no release since 2025-03, no zeroize, and
  not the `rustls` team's recommendation; kept as the measured fallback.
- `jsonwebtoken`: a second crypto stack or five more crates for one algorithm.
- `rsa` in production: a second RSA implementation in the daemon; it stays in the fake only.
