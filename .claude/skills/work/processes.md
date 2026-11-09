# Processes

The size decides how many stations run. The unit is a task in `doc/tasks.md`; a task's size is read from its text, not guessed from the diff.

## Classification

Priority: user override (`--small`/`--medium`/`--large`), then auto, then MEDIUM.

Auto:

- **SMALL**: docs, ADR files, config, CI, mechanical, or one module plus its test; `haiku` tasks.
- **MEDIUM**: one crate, up to about five `Accept:` clauses; the usual `sonnet` task.
- **LARGE**: the CP↔DP protocol or link, fencing and epochs, the `Attempt` lifecycle, capacity and admission, recovery and safe mode, VM ownership or any delete path, the journal, `AggregateStore`, outbox/inbox, the scale-set listener, secrets, the Tart or guest adapters (processes), a Quint model or conformance driver, a contract suite, the fault lab, `architecture.toml` or the architecture checker, anything marked ⛳, `opus` tasks, or production code in more than one crate.

## Sequences

**SMALL**, no review station:

```
intake -> plan -> implement -> test -> polish -> qa -> ship -> babysit
```

**MEDIUM and LARGE**, with the review loop:

```
intake -> plan -> implement -> test -> polish -> review -> qa -> ship -> babysit
              ^                                  |
              +------ structural changes --------+
```

`qa` runs the binaries only when the task changed something a person, operator or agent can observe (`mayflyd`/`mayfly` flags, output, exit codes, the socket API, config validation, `mayfly-fake-tart`/`mayfly-fake-github` behaviour, the end-to-end loop); otherwise it records `skipped: no runnable surface` with the reason and passes.

## Gates

| From | To | Kind | Condition |
|:--|:--|:--|:--|
| intake | plan | SCRIPT | Definition of Ready met |
| plan | implement | SCRIPT | checklist written; LARGE with an open design question also asks the owner one short question |
| implement | test | SCRIPT | diff non-empty |
| test | polish | SCRIPT | the gate green |
| polish | review | SCRIPT | MEDIUM or LARGE |
| polish | qa | SCRIPT | SMALL |
| review | qa | AGENT | no finding needs a structural change |
| review | implement | AGENT | structural change needed and `loopCount < 2` |
| review | qa | SCRIPT | `loopCount >= 2`: force on; remainder goes in the ship report |
| qa | ship | SCRIPT | no regression |
| ship | babysit | SCRIPT | pushed |
| ship | end | SCRIPT | not the push owner: report "commit ready" |
| babysit | end | SCRIPT | CI green, or escalated after 3 fixes |

## Limits

| Loop | Max | On limit |
|:--|:--|:--|
| review to implement | 2 | remaining findings reported |
| CI fix attempts | 3 | escalate |
