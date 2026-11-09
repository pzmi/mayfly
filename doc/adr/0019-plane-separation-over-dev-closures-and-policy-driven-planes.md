# 0019. Plane separation over dev closures and policy-driven planes

Date: 2026-11-09

## Status

Proposed

## Context

D1 (ADR 0002) makes the control plane / data plane split structural, and fnd §2.2 rule 3 says
a dev edge never breaks it ("a DP crate's tests do not use the SQLite adapter"). The checker
ported in FND-02 did not enforce that: a dev edge was legal whenever it targeted a test-only
crate, and the name rules ran over production code only, so `mayfly-data` could dev-depend on
`mayfly-conformance`, which sees both planes and may re-export `mayfly_adapter_sqlite`, and its
tests could drive the journal (FND-04 checkpoint finding M1). The plane membership itself was
spread over hand-written tables in the checker's code (`PURE`, `PROCESS_ALLOWED`,
`INDEPENDENT`, the isolation table), while fnd §3.1 describes `[planes]` and `[purity]`
tables in `architecture.toml` (ARCH-01 item 2, finding M3).

`mayfly-testkit` was listed with both planes' crates in its allowlist, and KER-03 gave it the
golden and determinism harnesses over `mayfly-control-domain`'s `Aggregate`; ES-01 adds the
in-memory journal and its contract suite there, DP-01 a fake VM provider. One kit serving both
planes is exactly the bypass above.

Alternatives considered:

- *Name rules only* (run layer isolation over test code and follow re-exports): catches a test
  that names the other plane, but not one that drives it through a test crate's own API, which
  names nothing plane-specific.
- *Allow dev edges to any test crate and trust review*: the finding shows review missed it.
- *One plane-neutral testkit*: impossible while the event-sourcing harnesses are generic over
  the control domain's `Aggregate`.

## Decision

1. Plane membership and the role selectors are data in `architecture.toml`: `[planes]`
   (`control`, `data`, `shared`, `both`, and `[planes.vocabulary]`) and `[purity]` (`pure`,
   `process_authority`, `independent`). Every production crate is in exactly one plane list.
   A repository test pins the lists to fnd §2.1 and §2.3 so an edit cannot switch a check off
   silently; the checker keeps in code only the selectors fnd does not tabulate.
2. `ARCH-plane-separation` holds at two levels. Over the policy: the `[dependencies]`
   allowlist closure of a control, data or shared crate contains no crate of a forbidden plane
   (the other plane; for a shared crate, either plane). Over the resolved graph: from a control
   or data crate, its own normal, build **and dev** edges as the first hop, then normal and
   build edges, reach no crate of the other plane. A dev edge to a test crate is therefore
   legal only when that crate's production closure stays within the dependent's plane and the
   shared crates. A shared crate's tests may use a plane's kit: they cross into no plane's code
   under test.
3. Test support is split per plane. `mayfly-testkit` is the control plane's kit (plane
   `control`; allowlist `mayfly-domain`, `mayfly-protocol`, `mayfly-control-domain`,
   `mayfly-control`). Data-plane fakes and suites (the fake provider, the `VmProvider` contract
   suite) go to a data-plane kit created by the first task that needs one (DP-01), with plane
   `data`. Plane-neutral helpers that both planes' tests need (FND-05's poll helper, DP-02's
   link contract suite) go to a shared kit whose closure is the shared crates, created by the
   first of those tasks; both plane kits may depend on it. `mayfly-conformance` stays `both`,
   so no plane crate may dev-depend on it.
4. `ARCH-layer-isolation` runs over test code too (test code may still name test-only
   crates), `ARCH-plane-vocabulary` forbids each plane's own type names in the other plane and
   in shared crates, and references to a workspace crate are followed through its `use`
   re-exports, so neither a re-export nor a renaming hides a name.

## Consequences

- A data-plane crate cannot use `mayfly-testkit`; DP-01 creates the data-plane kit and its
  policy entry, FND-05 and DP-02 the shared kit (and FND-05 moves the poll helper selector of
  `ARCH-test-sleep` with it). fnd §2.1, §2.2 and §2.5 say so.
- Adding a crate means choosing its plane in the same commit, or `ARCH-plane-separation`
  fails.
- The plane rules can only grow stricter by data: widening a plane list or an allowlist that
  crosses planes fails the policy-level check before any code exists.
- Enforced by `ARCH-plane-separation` (fixtures `data-dev-depends-on-conformance`,
  `policy-lets-data-reach-control`), `ARCH-plane-vocabulary` (`control-names-vm-provider`,
  `data-names-journal-via-shared`) and `ARCH-layer-isolation`
  (`data-test-names-sqlite-adapter`); introduced by ARCH-01.

## Amendment (2026-11-09): shared crates' tests use only shared crates

Decision 2 said "a shared crate's tests may use a plane's kit: they cross into no plane's code
under test". The second adversarial review of the FND-04 checkpoint showed the gap that leaves:
a kernel or protocol test that dev-depends on `mayfly-testkit` or `mayfly-conformance` builds
and drives a plane's crates (the control plane's aggregates through the testkit's harnesses, or
both planes through conformance), so the shared crates' test suites would start depending on
one plane's behaviour, and a plane-neutral helper would drift into a plane's kit because the
kernel could reach it there. Since FND-05 created `mayfly-testkit-shared`, the shared crates
have a kit of their own, so the allowance is no longer needed.

Decision 2 is amended: **a shared crate dev-depends only on shared crates** (the shared test
kit and the other shared crates); never on a plane's kit, on `mayfly-conformance` (`both`) or on
a crate in no plane (the fakes). A plane-neutral helper a shared crate's tests need goes to
`mayfly-testkit-shared`. Control and data crates keep the rule of decision 2: a dev edge is
legal when its target's production closure stays within the dependent's plane and the shared
crates.

Enforced by `ARCH-plane-separation` over the resolved graph (fixture
`shared-dev-depends-on-testkit`); fnd §2.2 rules 2 and 3 say so. The positive fixture's kernel
now dev-depends on the shared kit; the repository had no such edge.

## Amendment (2026-11-09): what the checker defends against

The third adversarial review of the FND-04 checkpoint bypassed the checker without touching a
rule: a lint level passed to a macro as a metavariable (`#[$l(clippy::disallowed_methods)]`
expanded to an `allow` nobody wrote), Cargo configuration and environment that switch Clippy off
(`rustc-workspace-wrapper`, `[env] CLIPPY_ARGS`, `-Zcrate-attr`, an alias `clippy = "check"`),
nightly features, gaps in the pure crates' `clippy.toml`, and CI steps whose failure a `|| true`
or `set +e` would swallow. Each pass so far has found new ways around name-based rules, so the
decision is to state what the checker is for rather than imply it is complete.

Decided: **the checker defends against honest mistakes, agent shortcuts and accidental drift,
and closes every bypass an adversarial review finds with a fixture or test that reproduces it
and fails for a named rule. It is not a sandbox against a determined insider; human and agent
review remain the backstop**, and a finding there becomes a fixture. What steers the build from
outside the source is refused, fail closed: Cargo configuration anywhere in the repository
(`ARCH-lint-config`) and the environment of `cargo xtask` (its preflight; only
`RUSTFLAGS=-D warnings`, which CI's native macOS job sets, is accepted). Nightly constructs are
refused outright (`ARCH-nightly-features`), as the toolchain is a pinned stable (ADR 0006).
Process authority over values (a returned `Command`) stays out of scope for the name-based
rules. The model, the refused variables and the list of what each of the three passes closed
are in fnd §3.6.
