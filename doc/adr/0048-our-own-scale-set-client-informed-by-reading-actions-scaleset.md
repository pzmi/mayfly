# 0048. Our own Scale Set client, informed by reading actions/scaleset

Date: 2026-11-10

## Status

Accepted (owner, 2026-11-10). Amends ADR 0028 (the registration token as an auth-chain step
only), marked there. Amended 2026-11-11 (owner, PLAN-01 and GH-00R): evidence strengths, and
"until GH-00's capture settles it" becomes "until it is server-observed".

## Context

GitHub documents the Runner Scale Set protocol mainly through its own MIT-licensed Go client,
`github.com/actions/scaleset`. Its code and tests are the best available evidence of the wire
protocol, but a line-by-line Rust port would carry Go idioms (context values, closures as
policy, mutable shared clients, inline blocking work in the handler) and would make mayfly a
derivative of substantial portions of it, with a licence notice in every ported module. Plan §7
and the GH-00..GH-04 cards also assumed the fake would take its wire shapes "from the Go
source". The client sits on two of mayfly's hardest rules: ack only after the inbox record
commits, and a pure domain with no blocking calls.

The owner decided, verbatim: "We inspect the code, we don't port it. We wrote good idiomatic
rust implementation. We only take inspiration from the GitHub code. If mit attribution is
required in this case let's add it".

## Decision

1. **mayfly implements its own idiomatic Rust Scale Set client** (`mayfly-adapter-github`):
   plain data types, free functions, explicit retry policy values, time and randomness through
   ports. It is informed by reading `actions/scaleset`; **no code or tests are copied or
   translated**.
2. **The protocol is ours to specify.** `doc/github-scaleset.md` states the wire protocol and the
   observed behaviour in mayfly's words. Each fact cites the upstream file and function at a
   pinned commit where it was verified, or is marked unverified until GH-00's live capture
   settles it (*since 2026-11-11: each fact carries its evidence strength; see the
   amendment*). mayfly re-derives those facts and pins them with its own recorded, redacted
   fixtures (GH-00, GH-03).
3. **The fake is built from the spec.** `mayfly-fake-github` (GH-01) implements
   `doc/github-scaleset.md` and the GH-00 captures, never upstream's tests, test server or
   fixtures (ADR 0012).
4. **Attribution.** The spec acknowledges the reference implementation and its pinned commit.
   Since no expression is copied, the MIT notice is not required. **Rule:** if any fragment,
   test vector or recorded payload from an upstream repository is ever copied, it ships with
   that repository's licence notice (for `actions/scaleset`, GitHub's MIT notice) in a
   `THIRD-PARTY-NOTICES` file at the repository root and is listed there by path, source and
   commit, in the same commit that copies it. Apache-2.0 sources (ARC) are cited for behaviour
   only.
5. **Registration token, refined.** The auth chain needs a runner registration token as the
   input of the Actions-service admin exchange (`doc/github-scaleset.md` §1). mayfly requests it
   for that purpose only and never registers a runner with it; ADR 0028's "no registration-token
   client" means no classic registration path. This refinement amends ADR 0028 (accepted by
   the owner, 2026-11-10).

## Consequences

- GH-01 (fake from the spec, contract suite against fake and, owner-gated, live), GH-02 (App
  auth and HTTP client), GH-03 (client with a recorded-fixture contract) and GH-04 cite
  `doc/github-scaleset.md`; plan §7.4 follows. The fake gains the registration-token and
  runner-registration routes.
- Every [unverified: GH-00] item in the spec stays a named assumption, a fake knob and a test
  name until the owner's capture replaces it.
- A review checks a GitHub-facing change for copied upstream expression; a copy without a
  `THIRD-PARTY-NOTICES` entry is a blocking finding.
- Updating the pinned commit is a deliberate spec change: re-read, re-cite, and re-record the
  facts that moved.

## Alternatives considered

- Port `actions/scaleset` module by module, with its MIT notice in each file: fastest to a first
  client, but it imports the reference's structure and idioms that fight mayfly's rules.
- Wrap the Go client as a sidecar binary: a second runtime and a process boundary for a
  credential-bearing component.
- Ignore the reference and work only from captures: slower, and loses the reference's evidence on
  retries, refresh and error classes.

## Amended 2026-11-11 (owner, PLAN-01 and GH-00R): public evidence first, with its strength

**Context.** The owner decided on 2026-11-11 to settle the spec from public evidence before any
live session, and to shrink GH-00 to the live remainder recorded in HW-02's session. Public
issues hold real responses of the service; a reference client's code holds only what it sends
and tolerates.

**Decision.** Each fact in `doc/github-scaleset.md` carries its evidence strength and a citation:
**server-observed** (a public source shows the service's own response), **client-assumed** (what
a reference client sends, expects or tolerates) or **doc-stated** (GitHub's documentation, or a
GitHub maintainer's written statement); **[open: HW-02]** marks what no public source settles.
Where this record says "until GH-00's capture settles it", read "**until it is
server-observed**": a client-assumed or open fact stays a named assumption, a fake knob and a
test name. The reading rule is unchanged: inspect, never port; a payload quoted in an issue is
evidence, never a fixture (HW-02 records its own). Sources are pinned (repositories by commit,
issues by number and the date of the comment, documentation by page and the date read).

**Consequences.** GH-00R applied it (spec §1–§13, `formal/scaleset.qnt`'s A1–A6); GH-00's
capture raises an item to server-observed by committing its fixture.
