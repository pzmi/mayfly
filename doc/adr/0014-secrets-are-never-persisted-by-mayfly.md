# 0014. Secrets are never persisted by mayfly

Date: 2026-11-09

## Status

Proposed

## Context

Just-in-time runner configs and GitHub tokens are credentials: a leaked JIT config lets anyone
register a runner. Plan sections 3.2 and 13 say they must never be persisted, logged or placed
in argv or child environment. A journal that kept them would also make every backup a credential
store, against goals G7 and the public-repository guardrail (plan section 1.3).

## Decision

Secrets are never persisted by mayfly. `Secret<T>` has redacting `Debug` and `Display` and no
`Serialize`. JIT configs live in memory and go to the guest launcher over stdin. The outbox stores
a `SecretHandle` (ADR 0010), never the secret; after a crash the attempt is retired and a new one
minted, and a possibly leaked config is never reused. A redaction test (`ARCH-secret-serialize`)
and the secret scan guard it.

## Consequences

A crash costs one attempt, which the recovery model already handles (plan section 10). Nothing
secret can appear in the journal, so replay, backups and the fault lab read it freely. Every new
type that holds a secret must use `Secret<T>`, and the secret scan (`gitleaks`, FND-04) covers
the repository. Implemented by KER-01/KER-02 (the type and handle) and enforced by the
architecture and redaction tests.
