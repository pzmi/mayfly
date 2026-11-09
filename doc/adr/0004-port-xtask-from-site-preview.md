# 0004. Port xtask from site-preview

Date: 2026-11-09

## Status

Proposed

## Context

Mayfly needs the same engineering tooling its owner's `pzmi/site-preview` repository already
has: an architecture checker with negative fixtures, a catalogue of checks run as one gate,
`doctor` with pinned tool versions, `adr`, `land` and an action-pin check. Foundation section 9
makes every repeatable job an `xtask` subcommand. Rewriting that from scratch would repeat a
solved problem; copying it verbatim would carry site-preview specifics (benchmarks, release
mechanics) that mayfly does not have yet. The licence is compatible: same owner, MIT OR
Apache-2.0.

## Decision

`tools/xtask` is ported from `pzmi/site-preview` and adapted, not rewritten: the architecture
checker with negative fixtures, the check catalogue, `doctor` with `tools/versions.toml`, `adr`,
`land` and the action-pin check. Site-preview-specific stages (benchmarks, docs and release
specifics) are dropped until a mayfly task needs them.

## Consequences

The gate (`cargo xtask check`) exists from FND-02 and later tasks add stages to the catalogue.
The port is not final: ADR 0019 changed the checker so that plane membership and role selectors
are data in `architecture.toml` and test support is split per plane, which the site-preview
checker did not have. Divergence from site-preview is expected and is recorded by later ADRs,
not by editing this one. Implemented by FND-02.
