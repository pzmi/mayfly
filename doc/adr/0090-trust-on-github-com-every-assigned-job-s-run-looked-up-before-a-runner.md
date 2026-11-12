# 0090. Trust on github.com: every assigned job's run looked up before a runner

Date: 2026-11-11

## Status

Accepted (owner, 2026-11-11 and 2026-11-12, GH-00R). Partly supersedes ADR 0027: its
classify-before-acquire mechanism now holds on GitHub Enterprise Server only; the trusted-only
tier stands. The owner decided on 2026-11-11 that every assigned job is classified before a
runner starts and that mayfly cancels nothing by default; on 2026-11-12 that public repositories
are supported and no GitHub plan is required, that the per-job check (with a workflow-run lookup)
is the trust boundary, that the hold while a refused job is assigned is the default with
cancellation as a per-pool option ("3a but as a configuration option"), and that the fork-PR
setting check is an option, default off. Amended 2026-11-12 (owner, GH-00S): the in-guest
job-started hook is accepted and is the hard boundary; the allowed events are accepted as the
default, configurable per pool; refused jobs hold nothing and go to a refusal sink (option A);
the permission list (ADR 0045) is accepted. The hold of point 3, its release window and the
operator release are withdrawn. See the two amendments below.

## Context

ADR 0027 enforced the trusted tier by classifying each `JobAvailable` offer and acquiring only the
trusted ones. On github.com that point no longer exists: a `scaleset` maintainer wrote that "the
job acquiring logic has been removed for a while from the API" (scaleset #75, 2026-04-13), and a
`JobAssigned` arrives with no offer and no acquire while the scale set advertises room (scaleset
#107, #133; `doc/github-scaleset.md` §6.1). GHES still offers jobs; that it is 3.18 and 3.19
comes from a user's report the reference client acted on (client-assumed). Once mayfly advertises
capacity, GitHub assigns any job its runner group admits, and any runner of the set may take any
assigned job (ADR 0027's context). A `JobAssigned` carries the repository, owner, event name and
labels (scaleset #107), but not whether a pull request comes from a fork: the repository is the
base's. Runner groups restricted to selected repositories need a paid plan ("Organization owners
using the GitHub Team plan can create additional organization-level runner groups"), and the
owner wants Free organisations and public repositories to work.

## Decision

1. **Start-up check (owner, 2026-11-12).** mayfly checks only that each pool's scale set exists
   (or can be created) and its session is reachable. A runner group is optional, any plan works,
   public repositories are allowed. The runner group is not the trust boundary.
2. **The per-job check is the boundary (owner, 2026-11-11 and 2026-11-12)** (*the lookup stays
   the gate for starting runners; the hard boundary is now the in-guest hook: amendment
   2026-11-12, GH-00S*). Every assigned job
   is classified **before any runner of its scale set starts**, and no runner starts while one is
   unclassified:
   - its repository must be one of the pool's `allowed_repositories` (config);
   - its workflow run is looked up, `GET /repos/{owner}/{repo}/actions/runs/{run_id}`
     ("Actions: Read"): the head repository against the base (a fork when they differ), the
     event and the actor;
   - **allowed events** (accepted as the default, configurable per pool: amendment GH-00S): `push`, `pull_request` whose head repository is the
     base repository, `workflow_dispatch`, `schedule`, `merge_group` and `release`. Each runs code
     that is in the repository or on one of its branches, put there or started by someone with
     write access; every other event is refused, among them a fork's `pull_request`,
     `pull_request_target`, `workflow_run`, `issue_comment`, `pull_request_review`,
     `pull_request_review_comment` and `repository_dispatch`. Adding an event is a new decision;
   - refused as well: anything unclassifiable (a field missing, empty or unknown) and any job
     whose run could not be looked up (an error, a timeout: the lookup's failure refuses). A
     per-repository allowance for forks may come later, by a new decision;
   - a refusal is journaled, `AssignedJobRefused{reason}`, shown to the operator: an event,
     never only a log line.
3. **Refused jobs (owner, 2026-11-12: "3a but as a configuration option").**
   - **Default** (*withdrawn: refused jobs hold nothing and go to a refusal sink, amendment
     GH-00S*): mayfly cancels nothing (no "Actions: Write"). No runner of the scale set starts
     while a refused job stays assigned, since any new runner could take it. GitHub's own release
     and requeue fails the job at its third release (scaleset README; the release time, publicly
     reported near 300 s, is [open: HW-02, spec §12 item 2]). The hold ends when a job fact shows
     the job gone, when the refusal lapses after the pool's `refusal_release_window` (at least
     three release times; 30 minutes by default until HW-02 measures the release), or when the
     operator runs `mayfly pool release-refusal <pool> <job>`, which is refused while GitHub still
     holds the job assigned, so it lifts a hold and asserts no outcome.
   - **Option `cancel_refused_runs`** (per pool, default false, needs "Actions: Write"): mayfly
     asks GitHub to cancel the refused run at once (`POST
     /repos/{owner}/{repo}/actions/runs/{run_id}/cancel`) and holds only until the cancellation
     is observed (the job leaves the assignment). A cancel that fails or whose answer is lost
     changes nothing at GitHub; the hold then falls back to the release window (*now: GitHub's
     release, which blocks nothing; amendment GH-00S*).
   - The refused job holds its slot of `maxCapacity` while it stays assigned.
4. **Fork-PR setting check, an option (owner, 2026-11-12).** `check_fork_pr_setting` (per
   organisation, default off) reads `GET /orgs/{org}/actions/permissions/fork-pr-workflows-private-repos`
   ("Administration: Read") at start-up and refuses to start when workflows from fork pull
   requests run in private repositories. The run lookup already refuses a fork's run; this check
   is defence in depth for private repositories.
5. **GHES keeps ADR 0027.** Where the acquire path exists, offers are classified and declined
   before acquire, as before.

## Consequences

- Permissions (ADR 0045, its proposed amendment): base "Self-hosted runners: Read and write"
  (organisation; it also covers the runner-group read of the public-repository report) and
  "Actions: Read" (repository, on the allowed repositories); optional "Actions: Write"
  (`cancel_refused_runs`) and "Administration: Read" (organisation, the fork-setting check).
- The model (`formal/scaleset.qnt`) expresses both flows and the gate: `initDirect` and
  `initAcquire` (and `initDirectWith(true)` for the cancel option), `ghAssigns` capped by the
  advertised `maxCapacity`, `pending` assignments and `lookupRun` (a failed lookup refuses),
  `startVm`, `cancelRun` (effect and answer each may be lost), releases bounded at three,
  `assignmentExpires` (A7: the window outlasts GitHub's releases), `operatorReleasesRefusal`;
  invariants `vmOnlyForAJobClassifiedTrusted`, `refusedJobNeverGetsAVm`,
  `assignedJobsClassified`, `cancelOnlyRefused`; tests `reassignedJobIsClassifiedAfreshTest`, `failedLookupRefusesTest`,
  `cancelledRefusedRunReleasesTheHoldTest`, `failedCancelFallsBackToTheWindowTest` and the
  progress scenarios; mutants `RUN_ASSIGNED`, `IGNORE_REFUSALS`, `IGNORE_PENDING` and
  `CANCEL_ANY`.
- KER-06G (the aggregate, demand and the reconciler's gate), GH-08 (config, the start-up checks
  and the public-repository report), GH-10 (the run lookup and the cancel), GH-09 (the operator
  release and the window) implement it; GH-01's fake serves the run, cancel and runner-group
  routes.
- **The limit**, stated rather than hidden: a runner started for a trusted job may take an
  untrusted job GitHub assigned after the last message mayfly recorded, whether before or after
  the runner started (`runnerMayTakeAJobAssignedBeforeItStartedTest`,
  `runnerMayTakeAJobAssignedAfterItStartedTest`; not invariants): mayfly cannot know of an
  assignment it has not received. Nothing in the scale-set protocol prevents it; the per-job
  check stops every runner it can, the cancel option shortens what is left, the proposed
  job-started hook (amendment below) would stop the job inside the guest, and with public
  repositories allowed this race is the residual risk SEC-01 records.
- **Public repositories need the runner group to admit them.** GitHub routes a public
  repository's jobs to a runner group only when the group allows public repositories, which the
  default group does not by default ("By default, only private repositories can access runners
  in a runner group"). GH-08 reads the scale set's group (`allows_public_repositories`, with the
  "Self-hosted runners" permission mayfly holds) and reports, at start-up and in `doctor`, an
  allowed repository that is public while the group does not admit public repositories. It
  reports; it does not refuse, since the runner group is not the boundary.
- A refused job delays the scale set's new runners until it is gone, the window lapses or the
  operator releases it; a failed lookup does the same for a trusted job. The operator sees each
  refusal and its reason.

## Alternatives considered

- Restrict the runner group to selected private repositories (this record's first draft):
  needs a paid plan and excludes public repositories.
- Advertise 0 and accept only offers: github.com no longer offers.
- Classify from the message alone: it cannot show a fork.
- Always cancel refused runs: needs "Actions: Write" on every allowed repository; kept as the
  option.
- Hold no runner after a refusal and accept the race: a runner started beside a refused job may
  take it (mutant `IGNORE_REFUSALS`).

## Amended 2026-11-12 (planner, GH-00R): an in-guest job-started hook (proposed)

Status: **Accepted by the owner on 2026-11-12**, as the hard boundary (amendment GH-00S below).

**Context.** The limit above stays: a runner started for a trusted job may take an untrusted job
assigned after the last message mayfly recorded. GitHub's runner runs a pre-job script named by
`ACTIONS_RUNNER_HOOK_JOB_STARTED` "when a job has been assigned to a runner, but before the job
starts running"; it has the default variables and the event payload at `GITHUB_EVENT_PATH`, and
"if there is any other exit code [than 0], the job will not run and will be marked as failed"
(Running scripts before or after a job, read 2026-11-12; doc-stated).

**Proposal.** Each image bakes a fixed job-started hook beside the runner (ADR 0030), and the
launcher sets `ACTIONS_RUNNER_HOOK_JOB_STARTED` to it when it starts the runner (ADR 0082). The
hook reads `GITHUB_EVENT_NAME`, `GITHUB_REPOSITORY` and the payload at `GITHUB_EVENT_PATH`
(`pull_request.head.repo.full_name` against `repository.full_name`) and exits non-zero, failing
closed (anything missing or unreadable fails), unless the event is one point 2 allows, the
repository is one the image's pool allows, and a pull request's head is the base. It is defence
in depth inside the guest, before any job step; the per-job check stays the boundary. The
allowed repositories reach the hook as data the launcher passes, never interpolated into a
command.

**Consequences.** A card (IMG-02) for the hook and the launcher's variable; HW-02 checks when the
hook runs relative to a job container's setup and that a failing hook fails the job without
running a step (spec §12 item 11).

## Amended 2026-11-12 (owner, GH-00S): the hook is the boundary; refused jobs go to a sink

**Context.** The owner decided the proposed parts on 2026-11-12. The research behind it, read
from `actions/runner` at `8cfa104` (runner 2.338.0): the broker, not the runner, picks which
assigned job a scale-set runner gets, so only a check on the job a runner actually received binds
a refusal to a runner; and before the job-started hook runs, the worker downloads and unpacks
every action the job references (`src/Runner.Worker/JobExtension.cs`, `PrepareActionsAsync`
before the hook), so attacker-chosen archives are parsed, though no job code runs. Container
setup comes after the hook.

**Decision.**

1. **The in-guest job-started hook is required and is the hard trust boundary** (owner). Every
   image bakes it (ADR 0030); the launcher sets `ACTIONS_RUNNER_HOOK_JOB_STARTED` (ADR 0082) and
   hands it the pool's allowed repositories and events as data. It classifies the job the runner
   received by the same rules as the lookup (`GITHUB_EVENT_NAME`, `GITHUB_REPOSITORY`, the head
   repository in the payload at `GITHUB_EVENT_PATH`): a trusted job runs; any other fails before
   any step with `::error title=Refused by mayfly::<reason>` and a non-zero exit. It fails closed:
   anything missing or unreadable refuses. Because actions are unpacked before it runs, a runner
   always runs in a disposable VM, never as a host process. IMG-02 is accepted and required.
2. **The run lookup stays the gate for starting runners**: a runner starts only for a job the
   lookup classified trusted. Nothing else holds the pool: a refused or pending job holds no
   runner, and an untrusted job that reaches a runner fails in seconds in its hook.
3. **Allowed events: the accepted default, configurable per pool** (`allowed_events`, default
   `push`, `pull_request` from the base repository, `workflow_dispatch`, `schedule`,
   `merge_group`, `release`). Per pool, not per repository: any runner of the pool can receive
   any job of any of the pool's repositories, so its VMs' trust is the pool's; a per-repository
   list would suggest a separation the runners do not have. A pool that needs a different list
   for one repository gets its own pool and scale set.
4. **Every refusal** (by the lookup, or reported by a hook through the launcher) is journaled as
   `AssignedJobRefused{reason}` and logged at WARN as the fixed event `job_refused` with the
   fields `repository`, `event`, `reason` and `run_id`, nothing more.
5. **Refused jobs: option A** (owner).
   - For a job the lookup refused, mayfly starts a **refusal sink**: GitHub's official runner in
     a small disposable **Linux** VM (no macOS slot) whose hook fails every job it gets; the VM is
     deleted after it, or at its deadline. GitHub picks the job a runner gets, so a sink starts
     only while mayfly knows of no trusted or pending assignment, and while it is up the scale
     set's `maxCapacity` is pinned at the assigned count, so GitHub assigns nothing new it could
     take (the cap: spec §5). One sink per refusal, one at a time per pool.
   - `cancel_refused_runs` (per pool, default false, "Actions: Write") cancels the refused run
     instead of a sink.
   - When neither can act (no capacity for a sink, a failed cancel), GitHub's own release fails
     the job at its third release; nothing waits on it.
   - **Withdrawn**: the hold of point 3, `refusal_release_window`, and `mayfly pool
     release-refusal` (GH-09). With the hook in every runner nothing needs a hold; a refusal is
     bookkeeping that a job fact, a new assignment or a re-created scale set clears.
6. **Permissions (ADR 0045): accepted** as proposed.

**Consequences.**

- The model (`formal/scaleset.qnt`) replaces the hold's invariant with
  `noStepOfAnUntrustedJobRuns` (the hook as the guard), and adds `sinkRunsNoStep` and
  `sinkOnlyForARefusedJob`; the mutants `HOOK_PASS` (a hook that lets everything through) and
  `SINK_RUNS` (a sink that runs job steps) violate them. `vmOnlyForAJobClassifiedTrusted`,
  `assignedJobsClassified` and `cancelOnlyRefused` stay.
- **The sink's limit**, not an invariant: a trusted job GitHub assigns while the sink is coming
  up, before the pinned poll, may reach the sink and fail; its message says to re-run
  (`sinkMayFailATrustedJobItCouldNotKnowOfTest`). HW-02 measures whether the pin holds with a
  backlog behind it, and the sink's latency.
- **Open for HW-02** (spec §12): when the hook runs relative to a job container's setup; that a
  Linux runner JIT-registered in a scale set whose jobs ask for macOS labels receives its jobs
  (labels belong to the scale set); what a fork PR's author sees (the annotation, the Checks
  tab); the time from assignment to the sink's failure.
- Cards: IMG-02 (required), GH-08, GH-10, KER-06G; GH-09 is withdrawn; IMG-03 builds the sink
  image and its directive.

**Alternatives considered.**

- **A fake runner in mayfly** that takes the refused job and completes it as failed (rejected):
  the broker, not the runner, picks which job a runner gets, so it could take a trusted job it
  cannot hand back; it would have to claim an official runner version it is not, to pass the
  service's minimum-version enforcement; the runner's internal protocol is undocumented and
  changes by server-side flags (about 65 commits to the protocol files of `actions/runner`
  between 2022 and 2026); and acquiring a job hands its secrets and token to the daemon, a
  control-plane process meant to hold no job data.
- **A throwaway macOS VM** in place of the Linux sink: uses one of the host's two macOS slots
  for seconds of work; kept only for where no Linux sink can run.
- **Keep the hold**: safe, but a refused job idles the pool for up to three releases.
