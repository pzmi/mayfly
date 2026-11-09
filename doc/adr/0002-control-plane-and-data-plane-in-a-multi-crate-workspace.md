# 0002. Control plane and data plane in a multi-crate workspace

Date: 2026-11-09

## Status

Accepted

## Context

Mayfly runs ephemeral macOS VMs for GitHub Actions. Plan section 3 splits the product into a
control plane (decides, journals, reconciles) and a data plane (executes against the VM
provider), and states why: the split makes the later multi-host edition a transport change, not a
rewrite, and gives each plane a narrow, testable contract. Both run in one process in v0.x.
Plan section 4 lays out the workspace crates; foundation sections 2 and 3 give the dependency law and its enforcement.
The risk is that a split kept by convention erodes: one convenient import and the planes meet.

Alternative considered: a single crate with modules. Cheaper at first, but nothing would stop the
domain from naming SQLite or Tart, which goals G4, G5 and G8 depend on it not doing.

## Decision

The code is a multi-crate Cargo workspace laid out as in plan section 4, with the control plane /
data plane split of plan section 3. Pure crates (`mayfly-domain`, `mayfly-protocol`,
`*-domain`) hold rules with no I/O; application crates hold use cases and port traits; adapters
bind one technology; only composition sees both planes. Control and data meet only through
`mayfly-protocol` messages and composition. The split is enforced by `architecture.toml` edges
checked by `cargo xtask architecture` (with negative fixtures), not by convention.

## Consequences

Adding a crate means a workspace member, an `architecture.toml` entry and its plane in the same
commit, and an architecture failure is fixed by fixing the structure. The plane rules were later
tightened over dev edges and test support by ADR 0019 (a per-plane test kit, plane membership as
data); ADR 0007 puts event sourcing on the control side of the line. Implemented by FND-01 and
FND-02 and refined by ARCH-01.
