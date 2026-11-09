# 0036. No telemetry and a local diagnostics bundle

Date: 2026-11-09

## Status

Accepted (owner, 2026-11-09)

## Context

`doc/philosophy.md` §5.1 asked the owner whether "no phoning home" also rules out opt-in
telemetry. Without any telemetry, a maintainer still needs evidence to fix a bug report.

## Decision

1. mayfly sends nothing anywhere the operator did not configure: no telemetry, no usage
   statistics, no crash upload, no update check, ever, and not as an opt-in either.
2. Evidence is gathered locally: `mayfly diagnostics bundle [--out <path>]` writes a redacted
   archive (version and build, config with secrets as references only, `doctor --json`,
   `status --json`, recent structured logs, event kinds and counts, projection summaries,
   provider and OS versions) and lists what it contains. It holds no secret, token, JIT config,
   repository content or job log. The operator reviews it and sends it by hand; nothing is sent
   in the background.

## Consequences

OPS-07 builds the bundle with a canary-secret test over its contents; an architecture rule that
only the GitHub adapter opens outbound connections pins "nothing else talks to the network"
(planned with SEC-01). The plan's `mayfly diagnostics export --redact` becomes `diagnostics
bundle` before any release, so no released name changes.

## Alternatives considered

- Opt-in telemetry: a default away from silence, and a channel that can widen later.
- Crash upload to a service: the same channel with less review.
