# Architecture decision records

This directory holds the decisions behind mayfly, one file per decision, in Michael Nygard's lightweight ADR format: `# NNNN. Title`, a date, then **Status**, **Context**, **Decision** and **Consequences**. To add a record, run `cargo xtask adr new "<title>"` (or `--number N` for a number the orchestrator reserved): it copies [`template.md`](template.md) with the next free number and adds the row here; commit both together. `cargo xtask adr index` checks the rows against the files and `cargo xtask adr index --fix` rewrites them. A decision is never edited away: a reversal is a new ADR, and the old record's status becomes `Superseded by NNNN`. **Accepted** records are owner decisions. **Proposed** records are planner defaults that the owner may override. `doc/tasks.md` cites decision D*n* as ADR *n*+1.

| ADR | Title | Status |
|---|---|---|
| [0001](0001-record-architecture-decisions.md) | Record architecture decisions | Proposed |
| [0002](0002-control-plane-and-data-plane-in-a-multi-crate-workspace.md) | Control plane and data plane in a multi-crate workspace | Accepted |
| [0003](0003-platforms-macos-arm64-product-linux-buildable.md) | Platforms: macOS arm64 product, Linux-buildable | Proposed |
| [0004](0004-port-xtask-from-site-preview.md) | Port xtask from site-preview | Proposed |
| [0005](0005-integrate-on-main-one-signed-commit-per-task.md) | Integrate on main, one signed commit per task | Accepted |
| [0006](0006-pinned-stable-toolchain-edition-2024-dual-licence.md) | Pinned stable toolchain, edition 2024, dual licence | Proposed |
| [0007](0007-event-sourcing-and-cqrs-in-the-control-plane-only.md) | Event sourcing and CQRS in the control plane only | Proposed |
| [0008](0008-sqlite-through-rusqlite-as-the-first-journal-adapter.md) | SQLite through rusqlite as the first journal adapter | Proposed. Amended by 0021 |
| [0009](0009-native-async-ports-used-through-generics.md) | Native async ports used through generics | Proposed |
| [0010](0010-serde-json-link-protocol-with-an-explicit-version.md) | Serde JSON link protocol with an explicit version | Proposed |
| [0011](0011-model-first-with-quint-conformance.md) | Model first with Quint conformance | Proposed |
| [0012](0012-independent-fault-lab-and-fakes.md) | Independent fault lab and fakes | Proposed |
| [0013](0013-tart-through-its-cli-by-argv-guests-by-tart-exec-or-ssh.md) | Tart through its CLI by argv; guests by tart exec or ssh | Proposed |
| [0014](0014-secrets-are-never-persisted-by-mayfly.md) | Secrets are never persisted by mayfly | Proposed |
| [0015](0015-owner-gated-real-world-lanes.md) | Owner-gated real-world lanes | Proposed |
| [0016](0016-agent-sandbox-and-fail-closed-tools.md) | Agent sandbox and fail-closed tools | Proposed |
| [0017](0017-serde-derive-in-the-shared-kernel.md) | Serde derive in the shared kernel | Proposed |
| [0018](0018-event-codec-and-aggregate-registry.md) | Event codec and aggregate registry | Proposed |
| [0019](0019-plane-separation-over-dev-closures-and-policy-driven-planes.md) | Plane separation over dev closures and policy-driven planes | Proposed |
| [0020](0020-control-plane-time-and-the-journal-s-recorded-at.md) | Control-plane time and the journal's recorded_at | Proposed |
| [0021](0021-a-storage-neutral-stream-scoped-event-store.md) | A storage-neutral, stream-scoped event store | Proposed |
| [0022](0022-strategic-ddd-with-a-data-oriented-implementation.md) | Strategic DDD with a data-oriented implementation | Accepted |
| [0023](0023-fake-tool-binaries-may-wait-in-real-time.md) | Fake tool binaries may wait in real time | Proposed |
