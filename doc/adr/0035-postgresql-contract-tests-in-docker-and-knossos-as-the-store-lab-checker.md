# 0035. PostgreSQL contract tests in Docker and Knossos as the store-lab checker

Date: 2026-11-09

## Status

Accepted (owner, 2026-11-09)

## Context

The PostgreSQL adapter (ES-09) exists to keep the store design honest, so its contract suites
must run wherever the SQLite ones do. `doc/event-store.md` recommended pinned PostgreSQL binaries
(O4) and a home-grown known-order checker for the store lab (O5). The agent sandbox can run
Docker with images from `mirror.gcr.io`, and the JVM is already pinned for Apalache. ADR 0016
said the sandbox has no Docker. (Owner decisions E4, E5.)

## Decision

1. **PostgreSQL runs in Docker**, from `mirror.gcr.io/library/postgres` pinned by tag and
   `@sha256:` digest in `tools/versions.toml`. The `contract-postgres` stage starts a throwaway
   container labelled with its run id, bound to `127.0.0.1` on a free port, runs every store
   contract in the PostgreSQL modes and removes the container. It runs locally, in the agent
   sandbox and in CI (a `postgres` job running the same stage). A missing Docker CLI or a
   daemon that is not running **fails closed** with a message saying how to start it.
2. **The store lab checks histories with Knossos**, an existing linearizability checker, pinned
   with its SHA-256 and run on the pinned JVM: each stream is an independent key whose model is
   a compare-and-set register over its revision. What Knossos cannot state (event-id
   uniqueness, delivery coverage, fences) is checked as plain assertions over the quiescent
   audit, not by a home-grown linearizability checker. Elle's list-append checker is the named
   fallback if Knossos does not scale to the workloads; switching needs an amendment.

## Consequences

Amends ADR 0016 (Docker is allowed for the PostgreSQL stage and the store lab). `doctor` checks
Docker, the image digest and Knossos; ES-09 and ES-10 implement it; CLAUDE.md's sandbox section
says so.

## Alternatives considered

- Pinned PostgreSQL binaries in a temporary cluster (O4): no Docker, but a second way to get
  PostgreSQL than CI's.
- A CI-only service container: the local gate would never see PostgreSQL.
- A home-grown known-order checker (O5) or Porcupine (drops indeterminate operations).
