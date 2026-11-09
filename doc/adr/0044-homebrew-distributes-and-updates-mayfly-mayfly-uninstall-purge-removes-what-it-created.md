# 0044. Homebrew distributes and updates mayfly; `mayfly uninstall --purge` removes what it created

Date: 2026-11-09

## Status

Accepted (owner, 2026-11-09). Decides philosophy P1 and P3.

## Context

A fleet daemon that updates itself is a surprise, and one that leaves VMs, image bases and runner
registrations behind after the package is removed is a leak. The philosophy (§4.2, §4.3) asks for
no surprises and for reversibility; ADR 0036 already forbids an update check.

## Decision

1. **Distribution and update go through Homebrew.** Releases are published for it (OPS-03).
   mayfly never updates itself: no self-update, no update check, no background download.
2. **`brew uninstall` removes the program**, nothing else.
3. **`mayfly uninstall --purge` removes exactly what mayfly created**: its journal and state, its
   VMs and image bases (each only with the ownership proof of plan §8.4: journal record, exact
   `OwnedVmName` and generation; never a base it did not create, never a foreign VM, never by
   name prefix), and its runner registrations. Homebrew's caveat points to it.
4. **It is a destructive operation on a real host**: run only by the owner or on the owner's
   explicit request, never by an agent on its own judgement (CLAUDE.md). It plans first and
   applies only a reviewed plan, like `recover` (philosophy §4.1).

## Consequences

- OPS-01 and OPS-03 cite this record; a new card OPS-08 builds `uninstall --purge`.
- A VM whose ownership cannot be proven is reported and left, so a purge can leave residue; that
  is the price of never deleting a foreign VM.
- The README may claim the commitment only once a test pins it (CLAUDE.md claims rule).

## Alternatives considered

- A self-updating daemon: a surprise and a supply-chain risk.
- A package-manager uninstall hook that purges: surprising data loss on a routine `brew upgrade`.
