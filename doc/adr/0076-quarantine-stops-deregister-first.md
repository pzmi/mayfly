# 0076. Quarantine stops deregister first

Date: 2026-11-10

## Status

Accepted (owner, 2026-11-10). Amends ADR 0025 (decision 1: a quarantined VM is stopped unless a
job is live), marked there, and the quarantine stop KER-04F decided (`doc/domain-model.md`
§6.4).

## Context

I21 ("deregister before stop") keeps a VM from being stopped while a runner of the attempt's
registration may still be registered and idle in it: GitHub would keep that runner on its list,
offline, and could assign it a job that never runs. KER-04G made every retirement wait for P11's
deregistration, but left the automatic stop of a quarantined VM outside the rule by design: a
quarantine is a safety action (KER-04F), so its stop went ahead at once unless a job was live.
That stop has the same effect on GitHub's list as a retirement's. The owner was asked whether
the quarantine stop should deregister first or stay immediate, and answered (2026-11-10):
"deregister".

`formal/lifecycle.qnt`'s `i21DeregisterBeforeStop`, extended to quarantine stops, fails on the
model as it stood: a random simulation of `activationStep` (seed 7) reaches Listening, a
`Quarantine` command and an immediate `StopVm` within twelve steps, and the scenario
`quarantineStopWaitsForDeregistrationTest` fails on it.

## Decision

1. **Entering quarantine with a runner that may be registered requests its deregistration.**
   When the attempt enters quarantine with its VM running, its instance id recorded, no job live,
   a JIT issued, the runner not seen exiting, and no retirement that already passed P11 (it is
   not Retiring or Deleting), `AttemptQuarantined` is followed by `StopDeregistrationRequested`.
   Staying in quarantine (a second reason) requests nothing again. A quarantine while a
   retirement still waits for P11 (requested, not yet confirmed) requests it too: entering
   quarantine clears the `RetirementRequested`, and the `StopDeregistrationRequested` is the
   key P11 answers from then on (DEC-06F), so a busy answer to the retirement's own request
   does not withdraw the stop, and a removal still in flight for it serves the stop once it
   answers.
2. **The automatic stop waits for P11's answer.** P11 serves the request as it serves a
   retirement (`registration::retirement_step`, keyed by the `StopDeregistrationRequested`
   event): it deregisters the runner and answers with the existing commands.
   `ConfirmRetirement` (removed, or not found) records `StopDeregistrationConfirmed` and the stop
   follows; the attempt stays quarantined, and nothing retires it but an operator.
3. **Busy: the VM is not stopped.** `WithdrawRetirement` after a busy answer records
   `StopDeregistrationRefusedBusy`: a job runs on the runner, so the VM keeps running until the
   runner is seen exiting (`RunnerExited`); an exited runner takes no job, so the stop then goes
   ahead without another request. `RunnerExited` is the only way back from
   `StopDeregistrationRefusedBusy`: no timer and no second request. An exit the observers miss
   keeps the VM running, and its capacity held, until an operator resolves the case; that loss
   is accepted as ADR 0025 decision 3 accepts every quarantine's. Inside quarantine
   `RunnerExited` is accepted whatever the attempt's JIT (DEC-06F, model and aggregate alike).
4. **Unreachable GitHub: keep waiting.** While P11 has no answer nothing is recorded, the saga
   issues nothing, and the VM keeps running, across restarts too. The quarantine holds its
   capacity anyway (ADR 0025, I28), so waiting costs nothing more.
5. **Unchanged:** a quarantine with nothing that could be registered (no JIT issued, the runner
   exited, the VM stopped or vanished, ownership doubt) stops at once as before; a live job
   still holds the stop; an operator's resolution retires on the operator's word and is not
   gated (ADR 0025).

The three events are new types (schema version 1). An older build that meets one does not
refuse that stream alone: an unknown stored kind puts the whole daemon in safe mode at start-up
(`doc/event-store.md` §6.3: no new VM, no destructive directive) until it is rolled forward.
They skip the two-release prepare and activate of `doc/event-store.md` §6 only because nothing
is released yet: no tagged release exists, so no older build runs against a journal that holds
them; from the first release on, a new event type ships in a prepare release first.
`ConfirmRetirement` and `WithdrawRetirement` are accepted in one more phase, and no event, field
or command is renamed.

## Consequences

- A quarantined VM whose runner may be registered keeps running, and holding its memory, until
  GitHub answers; operators see it running in `mayfly quarantine inspect`. Nothing is deleted
  either way (I28).
- P11 (CP-05) reads one more request event; until CP-05 lands, nothing answers it, so such a
  quarantined VM is not stopped automatically (an operator's resolution still is).
- Pinned by `formal/lifecycle.qnt`'s `i21DeregisterBeforeStop` (its ghost now flags a quarantine
  `StopVm` issued before the answer) and the scenarios `quarantineStopWaitsForDeregistrationTest`,
  `quarantineStopBusyKeepsRunningTest`, `quarantineStopWithoutRegistrationNeedsNoDeregistrationTest`,
  `quarantineStopAfterRunnerExitedNeedsNoDeregistrationTest` and the positive control
  `quarantineStopBeforeDeregistrationIsFlaggedTest`; `formal/registration.qnt`'s
  `requestQuarantineStop` under `deregisterBeforeStop` (`quarantineStopDeregistersFirstTest`,
  `quarantineStopBusyKeepsRunningTest`, `quarantineStopWaitsForGitHubTest`,
  `quarantineDuringPendingRetirementTest`, `quarantineDuringPendingRetirementBusyTest`, and the
  negative control `quarantineStopAtOnceViolatesTest`); the regression traces
  `lifecycle-quarantineStopWaitsForDeregistration` and `lifecycle-quarantineStopBusyKeepsRunning`
  with the conformance driver's `dereg` field and its mutant
  (`should_catch_a_quarantine_stop_that_skips_the_deregistration_when_its_trace_is_replayed`);
  and `crates/mayfly-control-domain/tests/attempt/lifecycle.rs`:
  `should_stop_a_quarantined_vm_only_after_the_deregistration_when_a_runner_may_be_registered`,
  `should_keep_a_quarantined_vm_running_when_github_answers_busy_for_its_deregistration`,
  `should_keep_waiting_when_the_deregistration_of_a_quarantine_stop_is_not_answered`,
  `should_stop_a_quarantined_vm_at_once_when_no_runner_can_be_registered_in_it`,
  `should_request_the_deregistration_once_when_a_quarantined_attempt_is_quarantined_again`,
  `should_request_the_stop_deregistration_when_quarantined_during_a_pending_retirement`.
- The attempt's `matrix/attempt.txt`, `decisions/attempt.txt` and `intents/attempt.txt` gain
  the new rows and cases; the lifecycle regression traces gain the `dereg` field.

## Alternatives considered

- **Keep the quarantine stop immediate** (KER-04F): a stopped VM's runner stays on GitHub's list,
  offline, and can be assigned a job until the reaper removes it. Declined by the owner.
- **Stop after a timeout without an answer**: would stop with a runner still registered whenever
  GitHub is unreachable, which is exactly the case I21 guards; the quarantine already holds the
  capacity, so waiting costs nothing.
- **New commands for the quarantine answer**: P11 already turns its step into
  `ConfirmRetirement`/`WithdrawRetirement`; one policy and one command pair serve both requests.
