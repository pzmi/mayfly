# 0009. Native async ports used through generics

Date: 2026-11-09

## Status

Accepted (owner, 2026-11-09)

## Context

Foundation section 6 needs ports that are cheap to call, testable with real objects and with
`Send` futures, in a codebase with no mocking frameworks. `async_trait` boxes every returned
future and forces `dyn` use everywhere; trait objects hide the concrete type that contract suites
and `ARCH-*` checks reason about. Since Rust 1.75 `async fn` in traits (and return-position
`impl Trait` in traits) cover the common case without either.

## Decision

Ports are native `async fn` / RPITIT traits used through generics. `async_trait` and `dyn` ports
appear only where object safety is proven necessary, with a note in that port's ADR. Returned
futures are `Send`. Every `async fn` on a port documents whether it is cancel-safe.

## Consequences

Composition is generic over its adapters, so wiring is checked by the compiler and there is no
vtable on hot paths. The cost is longer signatures and that a port whose implementation must be
chosen at run time needs the documented exception. Contract suites take a factory instead of a
`Box<dyn Port>` (verification section 5). Enforced in review and by foundation section 4.4;
introduced with the first port in KER-02 and ES-01.
