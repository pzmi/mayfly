# 0003. Vendored fixture stubs

Date: 2026-11-09

## Status

Accepted

## Context

The fixture resolves third-party crates offline.

## Decision

Its `.cargo/config.toml` replaces crates.io with the stub directory `vendor`.

## Consequences

The fixture needs no network.
