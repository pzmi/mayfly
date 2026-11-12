# Architecture decision records

This directory holds the decisions behind mayfly, one file per decision, in Michael Nygard's
lightweight format: `# NNNN. Title`, a date, then **Status**, **Context**, **Decision** and
**Consequences**, and optionally **Alternatives considered**. Keep a record short (about 30-50
lines); the design detail lives in the document the ADR points to. ADR 0001 records this
practice.

**An ADR is a decision record, not a rule.** It records a decision as it was made: the context
at the time, the options weighed and why one was chosen. When the context changes, the decision
can change, deliberately and on the record. Never cite an ADR as a reason not to do the better
thing; never drift from one silently either. Change it on purpose, say why, and change the
rules, fixtures and tests that pin it in the same commit.

**Creating.** `cargo xtask adr new "<title>"` copies [`template.md`](template.md) with the next
free number and adds the row here (`--number N` for a number the orchestrator reserved when it
dispatched the task). `cargo xtask adr index` checks the rows against the files and `--fix`
rewrites them. A change that adds or reverses an architectural decision (a crate or plane
boundary, a dependency of consequence, a protocol, a security, ownership or recovery policy, a
tool or CI topology, a rule exception) adds or changes an ADR in the same commit.

**Statuses.**

- `Proposed`: a planner default. Tasks follow it as written until the owner decides.
- `Accepted (owner, YYYY-MM-DD)`: the owner's decision. Only the owner moves a record here.
- `Rejected (owner, YYYY-MM-DD)`: considered and declined; kept so it is not proposed again
  without new context (the `docs` stage accepts it from FND-07 on).
- `Superseded by NNNN`: replaced as a whole. The new record says what it supersedes.

Any status may carry annotations after it: `amended YYYY-MM-DD`, `amended by NNNN`,
`partly superseded by NNNN (...)`.

**Changing a decision.**

- *Refine* (a bound, a name, a scope, a clarification that keeps the decision): append
  `## Amended YYYY-MM-DD (owner|planner): <what>` with the reason and what it replaces; mark the
  replaced passage inline with an italic pointer to the amendment. Never rewrite the original
  Context or Decision.
- *Refine by a later record*: the later ADR says what it amends; the earlier one's status gets
  `amended by NNNN` and a one-line note in its text.
- *Reverse* (or a change a reader of the old record must not miss): a new ADR that supersedes it,
  in whole or by section; the old record's status says so.
- An amendment of an Accepted record by anyone but the owner is a proposal: the record shows it
  as `amendment proposed YYYY-MM-DD` until the owner accepts it.

**Ids.** The ADR number is the decision's id. `doc/tasks.md`'s Decisions table lists every ADR
that binds tasks; its D1-D15 are historical aliases (D*n* = ADR *n*+1) and no new D-id is
minted. Numbers are reserved by the orchestrator at dispatch; a lane that finds its number taken
when it lands renumbers its record and every citation of it in the same commit.

| ADR | Title | Status |
|---|---|---|
| [0001](0001-record-architecture-decisions.md) | Record architecture decisions | Accepted. Amended 2026-11-09 |
| [0002](0002-control-plane-and-data-plane-in-a-multi-crate-workspace.md) | Control plane and data plane in a multi-crate workspace | Accepted |
| [0003](0003-platforms-macos-arm64-product-linux-buildable.md) | Platforms: macOS arm64 product, Linux-buildable | Accepted |
| [0004](0004-port-xtask-from-site-preview.md) | Port xtask from site-preview | Accepted |
| [0005](0005-integrate-on-main-one-signed-commit-per-task.md) | Integrate on main, one signed commit per task | Accepted |
| [0006](0006-pinned-stable-toolchain-edition-2024-dual-licence.md) | Pinned stable toolchain, edition 2024, dual licence | Accepted |
| [0007](0007-event-sourcing-and-cqrs-in-the-control-plane-only.md) | Event sourcing and CQRS in the control plane only | Accepted. Amended by 0024, 0026, 0037 |
| [0008](0008-sqlite-through-rusqlite-as-the-first-journal-adapter.md) | SQLite through rusqlite as the first journal adapter | Accepted. Amended by 0021 |
| [0009](0009-native-async-ports-used-through-generics.md) | Native async ports used through generics | Accepted |
| [0010](0010-serde-json-link-protocol-with-an-explicit-version.md) | Serde JSON link protocol with an explicit version | Accepted |
| [0011](0011-model-first-with-quint-conformance.md) | Model first with Quint conformance | Accepted |
| [0012](0012-independent-fault-lab-and-fakes.md) | Independent fault lab and fakes | Accepted |
| [0013](0013-tart-through-its-cli-by-argv-guests-by-tart-exec-or-ssh.md) | Tart through its CLI by argv; guests by tart exec or ssh | Accepted |
| [0014](0014-secrets-are-never-persisted-by-mayfly.md) | Secrets are never persisted by mayfly | Accepted. Amended by 0033, 0034 |
| [0015](0015-owner-gated-real-world-lanes.md) | Owner-gated real-world lanes | Accepted. Amended 2026-11-11 |
| [0016](0016-agent-sandbox-and-fail-closed-tools.md) | Agent sandbox and fail-closed tools | Accepted. Amended by 0035 |
| [0017](0017-serde-derive-in-the-shared-kernel.md) | Serde derive in the shared kernel | Accepted |
| [0018](0018-event-codec-and-aggregate-registry.md) | Event codec and aggregate registry | Accepted. Amended 2026-11-09 |
| [0019](0019-plane-separation-over-dev-closures-and-policy-driven-planes.md) | Plane separation over dev closures and policy-driven planes | Accepted |
| [0020](0020-control-plane-time-and-the-journal-s-recorded-at.md) | Control-plane time and the journal's recorded_at | Accepted. Amended 2026-11-10 |
| [0021](0021-a-storage-neutral-stream-scoped-event-store.md) | A storage-neutral, stream-scoped event store | Accepted. Amended by 0034 |
| [0022](0022-strategic-ddd-with-a-data-oriented-implementation.md) | Strategic DDD with a data-oriented implementation | Accepted |
| [0023](0023-fake-tool-binaries-may-wait-in-real-time.md) | Fake tool binaries may wait in real time | Accepted |
| [0024](0024-the-data-plane-owns-host-resource-accounting.md) | The data plane owns host resource accounting | Accepted. Amended by 0052 |
| [0025](0025-quarantine-keeps-evidence-and-unknown-resolves-only-by-observation.md) | Quarantine keeps evidence and Unknown resolves only by observation | Accepted. Amended by 0076 |
| [0026](0026-provider-instance-identity-in-the-ownership-proof-and-an-external-epoch-floor.md) | Provider instance identity in the ownership proof and an external epoch floor | Accepted. Amended by 0053 |
| [0027](0027-trusted-only-trust-tier-in-v0-x.md) | Trusted-only trust tier in v0.x | Accepted. Amended by 0090 |
| [0028](0028-scale-sets-only-with-prepared-unregistered-vms.md) | Scale sets only with prepared unregistered VMs | Accepted. Amended by 0048 |
| [0029](0029-registration-reaper-and-the-wire-behaviour-of-pause-drain-and-safe-mode.md) | Registration reaper and the wire behaviour of pause, drain and safe mode | Accepted |
| [0030](0030-runner-baked-into-the-image-with-refresh-tooling.md) | Runner baked into the image with refresh tooling | Accepted |
| [0031](0031-service-modes-and-the-internal-watchdog.md) | Service modes and the internal watchdog | Accepted |
| [0032](0032-support-softnet-with-the-daemon-kept-non-root.md) | Support Softnet with the daemon kept non-root | Accepted |
| [0033](0033-secret-providers-selected-by-uri.md) | Secret providers selected by URI | Accepted |
| [0034](0034-event-store-choices-for-directives-receipts-epochs-snapshots-and-rebuild.md) | Event store choices for directives, receipts, epochs, snapshots and rebuild | Accepted |
| [0035](0035-postgresql-contract-tests-in-docker-and-knossos-as-the-store-lab-checker.md) | PostgreSQL contract tests in Docker and Knossos as the store-lab checker | Accepted |
| [0036](0036-no-telemetry-and-a-local-diagnostics-bundle.md) | No telemetry and a local diagnostics bundle | Accepted |
| [0037](0037-a-per-vm-shim-owns-each-tart-run-process.md) | A per-VM shim owns each tart run process | Accepted |
| [0038](0038-jit-delivery-through-tart-exec-with-a-checked-guest-identity.md) | JIT delivery through tart exec with a checked guest identity | Accepted |
| [0039](0039-clones-only-from-local-service-owned-image-bases.md) | Clones only from local service-owned image bases | Accepted |
| [0040](0040-one-scale-set-per-pool-at-organisation-scope.md) | One scale set per pool at organisation scope | Accepted. Amended 2026-11-11 |
| [0041](0041-aggregatestore-one-write-path-intent-digests-and-refusals-bound-to-every-read.md) | AggregateStore: one write path, intent digests and refusals bound to every read | Accepted |
| [0042](0042-a-frame-of-an-older-session-is-answered-unknown-never-refused.md) | A frame of an older session is answered Unknown, never refused | Accepted |
| [0043](0043-upgrades-jump-forward-any-distance-rollback-is-one-release-deeper-rollback-restores-a-backup.md) | Upgrades jump forward any distance; rollback is one release, deeper rollback restores a backup | Accepted |
| [0044](0044-homebrew-distributes-and-updates-mayfly-mayfly-uninstall-purge-removes-what-it-created.md) | Homebrew distributes and updates mayfly; `mayfly uninstall --purge` removes what it created | Accepted |
| [0045](0045-the-github-app-permission-list-is-a-published-commitment.md) | The GitHub App permission list is a published commitment | Accepted |
| [0046](0046-config-schema-v1-reads-raw-toml-secret-references-live-in-the-kernel-exit-codes.md) | Config schema v1 reads raw TOML, secret references live in the kernel, exit codes | Accepted |
| [0048](0048-our-own-scale-set-client-informed-by-reading-actions-scaleset.md) | Our own Scale Set client, informed by reading actions/scaleset | Accepted. Amended 2026-11-11 |
| [0052](0052-no-defaults-for-unknown-facts-the-data-plane-fails-safe.md) | No defaults for unknown facts: the data plane fails safe | Accepted. Amended 2026-11-12 |
| [0053](0053-destructive-data-plane-directives-are-idempotent-by-outcome.md) | Destructive data-plane directives are idempotent by outcome | Accepted. Amended by 0066 |
| [0058](0058-the-secrets-adapter-runs-its-own-commands-the-keychain-through-the-security-cli.md) | The secrets adapter runs its own commands; the Keychain through the security CLI | Accepted |
| [0064](0064-github-client-crypto-and-tls-aws-lc-rs-and-rustls-no-jwt-crate.md) | GitHub client crypto and TLS: aws-lc-rs and rustls, no JWT crate | Accepted |
| [0066](0066-a-vanished-vm-is-quarantined-as-a-desync.md) | A vanished VM is quarantined as a desync | Accepted. Amended 2026-11-11 |
| [0069](0069-the-capacity-carve-keeps-the-headroom-and-what-quarantines-owe.md) | The capacity carve keeps the headroom and what quarantines owe | Accepted. Amended 2026-11-11 |
| [0076](0076-quarantine-stops-deregister-first.md) | Quarantine stops deregister first | Accepted |
| [0082](0082-jit-configs-on-the-data-plane-and-the-guest-launcher-contract.md) | JIT configs on the data plane and the guest launcher contract | Accepted |
| [0087](0087-service-owned-image-bases-a-durable-record-a-booted-base-and-pull-verification.md) | Service-owned image bases: a durable record, a booted base and pull verification | Accepted. Amended 2026-11-12 |
| [0090](0090-trust-on-github-com-every-assigned-job-s-run-looked-up-before-a-runner.md) | Trust on github.com: every assigned job's run looked up before a runner | Accepted. Amended 2026-11-12 |
| [0091](0091-the-daemon-s-composition-one-run-per-session-its-configuration-and-identity.md) | The daemon's composition: one run per session, its configuration and identity | Proposed |
