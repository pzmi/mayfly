# 0045. The GitHub App permission list is a published commitment

Date: 2026-11-09

## Status

Accepted (owner, 2026-11-09). Decides philosophy P4.

## Context

mayfly runs jobs on the operator's machines with a GitHub App's authority. The narrower the
App's permissions, the less a compromised key or a mayfly bug can do (philosophy §5.3).

## Decision

1. The **exact minimal permission list**, with the reason for each permission, is published in
   the documentation as a commitment.
2. **Widening it needs the documentation and an ADR first**, then the code.
3. `mayfly doctor` **warns when the installed App holds more** than the list needs.
4. The list itself is **to be verified**: GH-00 and HW-02 confirm it against a real App in a
   disposable organisation, and fill it in. Until then the documentation says "unverified" and
   no permission is guessed (plan §7.1).

## Consequences

- GH-02 adds the permission check that doctor uses; the documented list is pinned by a test
  against the fake-github fixture, and by HW-02 on a real App.
- An organisation-wide scale set needs some organisation-level permission; the list says which
  and why, rather than defaulting to a broad one.

## Alternatives considered

- Documenting permissions loosely: lets them widen by drift, which this record forbids.
