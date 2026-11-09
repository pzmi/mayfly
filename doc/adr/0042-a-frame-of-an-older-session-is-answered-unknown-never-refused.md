# 0042. A frame of an older session is answered Unknown, never refused

Date: 2026-11-09

## Status

Proposed. Amends plan §3.2 (the `stale_epoch` refusal reason).

## Context

Plan §3.2 lists `stale_epoch` among the reasons of `DirectiveRefused`, which is definite:
nothing happened. KER-02 modelled the link first (`formal/link.qnt`): at-least-once delivery,
duplicates, loss and reordering, and a data plane that reconnects or restarts into a new run
epoch. The relay stamps the session's epoch at send time and re-sends a directive until its
answer is definite; the data plane keeps the outcome table of the current session only, in
memory.

In that model a refusal of a stale frame is false. A `CloneVm` sent under epoch 1 takes effect
and its answer is lost; the data plane reconnects into epoch 2; a duplicated epoch-1 frame then
arrives. Answered `DirectiveRefused{stale_epoch}` stamped with epoch 1, it says that a
directive which took effect in epoch 1 did nothing there. Stamped with epoch 2, it says the
directive did nothing in epoch 2, and the relay's re-send under epoch 2 then executes it. Either
way the control plane's `Attempt` reads a definite non-application of an effect that happened
or will happen, and backs off a VM that exists. The data plane cannot answer otherwise: after a
restart it has no record of what an older session executed. The model's
`refusedMeansNothingHappened` invariant fails for this rule (the negative control recorded in
`doc/formal-models.md`).

## Decision

1. A directive frame of an epoch older than the data plane's session is **never executed** (I09)
   and is answered `OutcomeUnknown{reason: stale_epoch}`, stamped with the current session's
   epoch. It is not a `DirectiveRefused`: every refusal reason is definite for the epoch it is
   stamped with.
2. A frame of an epoch the data plane has not entered (before the welcome, or no session) and a
   frame of another major are discarded unanswered; the relay re-sends under the current
   session.
3. The data plane enters a session only on a `Welcome` strictly above its floor, so a session
   whose outcome table is gone is never re-entered and "each directive id executes at most once
   per epoch" holds across restarts.
4. Across epochs a directive may run again; that re-run is a re-observation by target name
   (plan §3.2, "idempotent by directive id and by target name"), which the data plane's executor
   provides (KER-08).

## Consequences

- `mayfly_protocol::RefusalReason` has no `stale_epoch`; `mayfly_protocol::UnknownReason` has
  `StaleEpoch`. `mayfly_protocol::check_epoch` classifies a frame (`Current`, `Stale`,
  `NotEntered`) and `mayfly_protocol::enters_session` is the strict floor rule.
- The control plane's `Attempt` aggregate treats a stale-epoch answer like any `OutcomeUnknown`:
  it leaves `Unknown` only on a later observation (I10), which the re-send under the new epoch
  provides. `mayfly_control_domain::attempt::RefusalReason::StaleEpoch` is no longer produced at
  the protocol boundary; the translation of CP-02 maps the protocol's `UnknownReason::StaleEpoch`
  to `Fact::OutcomeUnknown`.
- Pinned by `formal/link.qnt` (`staleFrameIsNeverRefusedTest`,
  `oldWelcomeAfterRestartIgnoredTest`, `refusedMeansNothingHappened`,
  `executesAtMostOncePerEpoch`), `crates/mayfly-protocol/src/handshake.rs`
  (`should_fence_frames_by_the_session_epoch_when_a_directive_arrives`,
  `should_enter_only_above_the_floor_when_a_welcome_arrives`), the golden fixture
  `observation/v1.unknown-stale-epoch.json`, and later the link contract suite (DP-02) and the
  data-plane fencing matrix (KER-08).

## Alternatives considered

- **Keep the refusal and let the control plane ignore it when stamped with an older epoch.** The
  attempt's epoch need not have moved when the answer arrives, and the epoch-2 stamping is false
  regardless.
- **Discard stale frames unanswered.** Safe, but the control plane then learns nothing until the
  directive's deadline; an `Unknown` answer is as safe and says why.
- **Keep the outcome table across sessions.** Lost on a restart, so the answer would still be
  false after one.
