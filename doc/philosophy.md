# mayfly: philosophy

**Status:** draft for the owner, 2026-11-09. **Scope:** product behaviour, operator interface,
security, observability, dependencies and the way mayfly is built.

**How to use this document.** It says *why* mayfly is built the way it is and which lines it
does not cross. The plans say *how*: [`plan.md`](plan.md) (product and architecture),
[`domain-model.md`](domain-model.md) (language and boundaries),
[`foundation.md`](foundation.md) (engineering rules) and [`verification.md`](verification.md)
(evidence). Every new feature, directive, command, metric, log field, dependency or default
passes the decision test in §8 before it is designed. Where a plan and this document disagree,
raise it with the owner; do not quietly pick one. §9 maps each principle to what holds it in
place. A principle about something not built yet is a commitment about how it will be built,
not a claim about today; the README states only what is true now.

## 1. Purpose

mayfly runs each GitHub Actions job in a machine that has never run anything else, and then
makes that machine disappear. It does one job and gets out of the way.

Success **is**: every job ran in a clean VM cloned from a pinned image; nothing from one job
reached another; nothing mayfly did not create was touched; after any crash the host converged
to a known inventory; the operator was never surprised and could always see why.

Success is **not**: the highest jobs per hour at any cost, the shortest queue time bought with
reuse, or a dashboard that is green because nothing checked.

A day in which mayfly refused to act because it could not prove something, and said so, is a
good day.

## 2. Safety before throughput

2.1 **When in doubt, do not act.** A stalled queue is an inconvenience that recovers; a VM
that leaked state into the next job, a deleted VM that was not ours, or an overcommitted host is
damage. Admission, cleanup and recovery err towards refusing (plan §1.1 G1–G4).

2.2 **Never act on unproven ownership.** mayfly deletes only what it can prove it created: the
journal record, the exact owned name, the generation and the provider's instance identity,
matched against the current fence. Never a base image, never a foreign VM, never by name
prefix. An unknown VM that looks like ours is reported, never deleted (G4, plan §8.4).

2.3 **One job, one machine, then gone.** A VM that ran workflow code never re-enters a pool.
There is no "reuse when it looks clean" (G1).

2.4 **Bounded by construction.** Slots, guest-OS licence limits, memory headroom and disk reserve
are admission rules, not hopes (G3). Recovery never starts unbounded replacements (G2).

2.5 **Untrusted code is untrusted.** Workflow code is hostile by default. Fork pull requests are
off unless the operator turns them on for a pool. No host directory, socket, key or credential is
shared with a guest (plan §1.3, §13).

## 3. Honest state

3.1 **No fictional success.** State records what was observed, never what was attempted. An
effect whose outcome is unknown is `Unknown` until something outside mayfly confirms it; a
timeout is never success (G5).

3.2 **Status says what is known and how old it is.** `mayfly status` distinguishes observed,
unknown, stale and refused, and says why. Safe mode is announced, with its reason, not hidden.

3.3 **Evidence, not assurance.** The same rule binds how mayfly is built: a check that did not run
is not green, a missing tool fails, a run that did not reach its target is `INCONCLUSIVE`, and a
claim in any document names the test that pins it (verification §1, §11).

3.4 **Errors say what to do.** A refusal names the rule that refused, the evidence it lacked and
the operator's next step, never a stack of internal names.

## 4. The operator is in control

4.1 **mayfly proposes; the operator disposes.** Destructive recovery is a plan the operator
reviews and applies (`recover --dry-run`, `--apply <plan>`). Drain, pause and resume are the
operator's levers and always work.

4.2 **No surprises.** No self-update, no background download the operator did not configure, no
default that widens access [OWNER: confirm "no self-update, ever"]. Configuration is one file,
validated on load; a change is logged once at INFO.

4.3 **Reversible.** Releases roll back safely (adjacent releases are compatible both ways; an
older build that meets something it cannot understand enters safe mode rather than guessing).
Uninstall removes what mayfly created and nothing else [OWNER: confirm as a commitment for
OPS-01].

## 5. Privacy and least privilege

5.1 **No phoning home.** mayfly talks to the GitHub endpoints the operator configured and to the
local VM provider, and to nothing else. No telemetry, no usage statistics, no crash upload, no
update check [OWNER: confirm; this also rules out an opt-in telemetry later]. Metrics are
exposed locally and pulled by the operator's own tools.

5.2 **Secrets are transient.** A secret is never written by mayfly: not to the journal, the
outbox, logs, metrics, argv, a child's environment or diagnostics. It lives in memory, is
handed over once, and is gone (D13).

5.3 **Least privilege.** The daemon runs as a non-root user; its state is `0700`/`0600`; the
GitHub App asks for the narrowest permissions that work, and the documentation lists them and
why [OWNER: confirm the exact permission list is a documented commitment].

5.4 **Telemetry about the system, never about people or code.** Logs and metrics describe the
daemon's health with bounded labels; they never carry repository content, job logs, user
names or tokens. Diagnostics export is redacted by default.

## 6. Boring to operate

6.1 **Few moving parts.** One daemon, one local journal file, one config file, one socket, the
OS's service manager. A new moving part (a database server, a network listener, a second host)
needs an ADR that says why the boring option failed.

6.2 **Quiet when healthy.** A healthy daemon logs (almost) nothing (fnd §5). Every ERROR is
something the operator must act on, and has a runbook entry.

6.3 **A good citizen of what it depends on.** mayfly respects GitHub's rate limits and
`Retry-After`, polls no faster than demand needs, and never retries in a tight loop. It
respects the VM provider's licence (it executes the binary, never incorporates its source) and
the guest-OS licence limits.

6.4 **The host belongs to the operator.** Disk and memory reserves are honoured before mayfly's
own wishes; mayfly cleans up what it made, and never fills the disk to run one more job.

## 7. Open, inspectable, replaceable

7.1 **Inspectable.** Every decision is an event with its inputs (G7). The journal is readable
with plain SQL and documented; the reasons behind the architecture are in ADRs; the source is
public under `MIT OR Apache-2.0`.

7.2 **No lock-in beyond ports.** Tart and GitHub are adapters behind ports; the domain knows
neither (G8). The config and image-manifest formats are documented and versioned. If another
tool fits someone better, the documentation says so [OWNER: confirm we name alternatives, for
example for Linux-only fleets on Kubernetes].

7.3 **Simple over clever.** Plain data, pure functions, tables and enums (ADR 0022). Correctness
beats cleverness; a shortcut that makes today's task faster and tomorrow's proof harder is
treated with suspicion. Architecture that protects correctness and testability is not ceremony.

## 8. The decision test

Before designing a feature, directive, command, default, metric, log field or dependency, ask:

| Question | If the answer is wrong |
|---|---|
| Can it cause an effect on a VM, image, runner or host without positive proof of ownership and a current fence? | Do not build it that way. |
| Can it report success, or a state, that nothing observed? | Make it report `Unknown` or refuse. |
| If it fails halfway, or its reply is lost, does the system converge to a known state without a human? | Add the intent, the observation and the reconciliation path first. |
| Does it let one job see anything of another job? | Do not ship it. |
| Does it send anything anywhere the operator did not configure? | Do not ship it. |
| Does it put a secret, an id or upstream text somewhere that persists or is exported? | Use a handle, a bounded label or nothing. |
| Does it widen a default (access, trust tier, privilege, network)? | Make it opt-in, documented and visible in `status`. |
| Can the operator see it, stop it and undo it? | Make sure they can. |
| Would an older or newer release sharing the same journal still be correct? | Prepare first, activate later. |
| Does it add a moving part an operator must understand at 03:00? | Prefer the boring option, or write the ADR that says why not. |
| Would we be comfortable explaining the mechanism in the public README? | Do not ship it. |

## 9. Where each principle is held

| Principle | Held by |
|---|---|
| §2.1–2.4 safety, ownership, single use, bounds | plan §1.1 G1–G4; invariants I01–I05, I17, I29; fault lab F01–F31; `OwnedVmName` |
| §2.5 untrusted code | plan §1.3 trust tier; SEC-01 tests |
| §3.1 no fictional success | G5; I10; lifecycle model; CP-02 lost-reply tests |
| §3.3 evidence | verification §1, §11; the four verdicts; `quick` is not `check` |
| §4.1 operator approves recovery | plan §10, §11 (`recover --dry-run/--apply`); I11 |
| §4.3 reversible releases | event-store.md §6; ES-11 compat harness |
| §5.1 no phoning home | [OWNER] a test or architecture rule that only the GitHub and provider adapters open network or process handles (fnd §3 spawn/IO rules) |
| §5.2 secrets | D13 (ADR 0014); I15; RT-04 redaction tests; secret scan |
| §5.4 bounded telemetry | fnd §5.4; RT-04 metric goldens |
| §6.2 quiet when healthy | fnd §5.2 |
| §6.3 good citizen | GH-02 rate-limit handling |
| §7.2 ports | G8; ADR 0002; `architecture.toml` plane rules |
| §7.3 data-oriented | ADR 0022; review skill |
| §8 decision test | the `review` skill's checklist and the `plan` station |

Section numbers stay stable when sections are reordered.
