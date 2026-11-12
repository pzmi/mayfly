# The Runner Scale Set protocol, as mayfly uses it

This is mayfly's own specification of the GitHub Actions Runner Scale Set protocol: the
credentials, endpoints, message flow and failure behaviour that `mayfly-adapter-github` (GH-02,
GH-03), `mayfly-fake-github` (GH-01) and the listener use case (GH-04) are built from. It is
written in our words from reading reference implementations, public issue reports and GitHub's
documentation; nothing here is copied code (ADR 0048).

**Sources.** GitHub publishes the protocol only through its implementations. We read, and did
not port:

| Short name | Repository | Pinned commit | Licence |
|:--|:--|:--|:--|
| `scaleset` | `github.com/actions/scaleset` (GitHub's Go client and listener) | `716908e` | MIT |
| `runner` | `github.com/actions/runner` (the runner that consumes a JIT config) | `310a370` | MIT |
| `ARC` | `github.com/actions/actions-runner-controller`, which at this commit uses `scaleset` as its client | `89f34c4` | Apache-2.0 (behaviour cited, never code) |
| `ARC@c6e4c94` | the same repository before its own client was replaced by `scaleset`: its client (`github/actions/client.go`) and the design records GitHub's staff wrote in 2022 (`docs/adrs/`) | `c6e4c94` | Apache-2.0 (behaviour cited, never code) |
| issues | public issues of `actions/scaleset`, `actions/actions-runner-controller` and `actions/runner`, cited by number and the date of the comment cited; read on 2026-11-11 | — | — |
| docs | GitHub's documentation, cited by page title; read on 2026-11-11 | — | — |

**Evidence strengths.** Every fact carries one (GH-00R):

- **server-observed**: a public source shows the service's own response: a recorded payload, a
  status, an error body or a log line from a real run. Cited as an issue
  (`[server-observed: ARC #4285, 2025-10-24]`). One report is one deployment at one time; where
  only one exists, the text says so.
- **client-assumed**: what a reference client sends, expects or tolerates: evidence of what the
  service accepts from it, not of what it does. A bare `[scaleset: file, function]`,
  `[runner: …]`, `[ARC: …]` or `[ARC@c6e4c94: …]` citation is client-assumed.
- **doc-stated**: GitHub's documentation says so (`[docs: page]`), or a GitHub maintainer stated
  it in writing (`[maintainer-stated: …]`, an issue comment or one of ARC's design records).
- **[open: HW-02]**: no public source settles it. Section 12 lists what HW-02's session captures
  (GH-00); until then the fake implements the stated assumption as a named knob and the contract
  tests name it (ADR 0048).

A client-assumed or open fact stays a named assumption wherever it is used: here,
`formal/scaleset.qnt`'s A1–A6, GH-01's knob and the test that names it, until it is
server-observed (ADR 0048, amended 2026-11-11).

The protocol is in public preview (plan §7.4). Everything here is a fact about a third party's
service at a point in time; recorded fixtures (GH-00, GH-03) are the contract, this document is
their index.

## 1. Credentials and the auth chain

Four credentials, all `Secret<T>` in memory only, never journaled, logged or put in a child's
environment (ADR 0014, plan §7.1, dm §9.1). The GitHub App private key comes from a
`SecretProvider` reference (ADR 0033).

```
App private key ──sign──▶ App JWT ──▶ installation token ──▶ registration token
   ──runner-registration──▶ admin token + Actions service URL ──create session──▶ message-queue token
```

1. **App JWT.** RS256, signed locally with the App's key. Issued-at backdated about 60 s for
   clock skew; expiry at most 10 minutes after issue; issuer is the App's client id (the app id is
   also accepted). A fresh JWT is minted for every installation-token request
   [scaleset: jwt_provider.go, newGitHubAppJWT] [docs].
2. **Installation token.** `POST {api}/app/installations/{installation_id}/access_tokens`,
   `Authorization: Bearer <JWT>`, no body; answers 201 with `token` and `expires_at`
   [scaleset: client.go, fetchAccessToken]. Lifetime one hour [docs]. Its format is not fixed
   length; mayfly never validates a token by shape [docs]. The reference client uses it once,
   as a step of the admin-token refresh, and does not cache it [scaleset: client.go,
   getRunnerRegistrationToken]; mayfly also reads the `permissions` its answer lists for the App
   permission probe (ADR 0045, GH-02) [docs], and uses it for the workflow-run lookup of each
   assigned job (section 6.2).
3. **Registration token.** `POST {api}/orgs/{org}/actions/runners/registration-token` (repository
   scope: `/repos/{owner}/{repo}/...`), `Authorization: Bearer <installation token>`, empty body;
   201 with `token` and `expires_at` [scaleset: client.go, getRunnerRegistrationToken,
   createRegistrationTokenPath]. Lifetime one hour [docs]. **mayfly uses it for one thing only:
   as the input of step 4.** It never registers a runner with it (no classic or `--ephemeral`
   mode, ADR 0028; ADR 0048 records this refinement).
4. **Admin token and Actions service URL.** `POST {api}/actions/runner-registration`,
   `Authorization: RemoteAuth <registration token>`, JSON body `{"url": "<config URL>",
   "runner_event": "register"}`, where the config URL is the organisation's (or repository's)
   `https://github.com/...` URL; a 2xx answers `url` (the Actions service base) and `token` (a
   JWT) [scaleset: client.go, getActionsServiceAdminConnection]. Both fields are required. The
   token's expiry is read from its `exp` claim without verifying the signature; a token without
   `exp` is refused [scaleset: client.go, actionsServiceAdminTokenExpiresAt]. mayfly also refuses
   a token that expires within the 60 s refresh margin, and a service URL that is not `https`
   (plain `http` only to loopback, for the fake) or carries a query, a fragment or user info: the
   admin token is sent to it (GH-02). **Hosts:** on github.com the queue URL was
   `https://broker.actions.githubusercontent.com/scalesets/message` in 2026
   [server-observed: scaleset #131, 2026-09-24], and the service URL
   `https://pipelines.actions.githubusercontent.com/<tenant>/` in GitHub's 2022 design record
   [ARC@c6e4c94: docs/adrs/2022-12-27-pick-the-right-runner-to-scale-down.md];
   GitHub lists `*.actions.githubusercontent.com` among the domains a runner needs for essential
   operations [docs: Self-hosted runners reference, Communication]. So when the API base is
   `https://api.github.com`, mayfly requires the service URL's and the queue URL's host to end in
   `.actions.githubusercontent.com` (doc-stated suffix; the exact host has changed once, so it is
   never pinned; GH-02H). On GHES no host is required beyond the https rule.
   This one exchange also retries 401 and 403, because a freshly minted registration token is not
   always usable at once [scaleset: client.go, getActionsServiceAdminConnectionRequest].
   Lifetime: no source states it; clients read `exp` and refresh 60 s early
   (client-assumed) [open: HW-02, item 5: decode `exp − iat`].
5. **Message-queue token.** Returned with the session (section 3) as `messageQueueAccessToken`,
   with the queue's URL `messageQueueUrl`. It authorises poll, ack **and acquire**
   [scaleset: session_client.go, getMessage, deleteMessage, acquireJobs]. **Lifetime about one
   hour**: a listener idle for exactly 60 minutes found it expired, and listeners refresh it every
   60 to 65 minutes; expiry shows as 401, logged by the reference listener as "message queue
   token is expired" [server-observed: ARC #3204, 2023-12-28 and 2025-04-18] (section 3.3).

**Refresh.** The admin token is refreshed lazily, before a request, when it expires within 60 s;
refresh reruns steps 1–4, serialised so that concurrent callers refresh once
[scaleset: client.go, updateTokenIfNeeded, actionsServiceAdminTokenSnapshot]. mayfly keeps the
same 60 s margin and single-flight refresh; its clock comes from the time port (ADR 0020), never
read inline. The App key is asked of its provider again on each refresh, so rotation needs no
restart (ADR 0033).

**API base and scope.** For `github.com` the REST base is `https://api.github.com`; for GitHub
Enterprise Server it is `https://<host>/api/v3` [scaleset: config.go, gitHubAPIURL]. mayfly
supports organisation scope, and repository scope for tests (ADR 0040). **Enterprise scope is
unsupported:** GitHub App authentication does not work for runners at the enterprise level
[docs: Authenticating ARC to the GitHub API], so it would need a classic token (ADR 0040,
amended 2026-11-11). Several organisations, one scale set each with the same App, are the
planned next step (GH-07). GHES is a config field, never an environment read.

## 2. Requests to the Actions service

Every call to the Actions service (`{svc}` below, the URL of step 4) carries
`Authorization: Bearer <admin token>`, `Content-Type: application/json` and the query parameter
`api-version=6.0-preview`, added when absent [scaleset: client.go,
newActionsServiceRequestWithQuery, requestURL]. Exceptions: the message-queue calls use the queue
URL and token (section 4), and acquire uses `{svc}` with the queue token.

**User-Agent.** The reference client sends a JSON object (system, version, commit, scale-set id,
`kind`) [scaleset: common_client.go, client.go, userAgentString]. mayfly sends its own values
(`system: "mayfly"`, version, commit). Whether the service reads it: no source says, and nothing
in mayfly depends on it (not on the HW-02 list).

**Endpoints mayfly uses.** `{id}` is the scale set's id.

| Operation | Request | Success | Verified in `scaleset` |
|:--|:--|:--|:--|
| Runner group by name | `GET {svc}/_apis/runtime/runnergroups/?groupName=<name>` | 200 `{count, value: [group]}`; exactly one expected | client.go, GetRunnerGroupByName |
| Scale set by name | `GET {svc}/_apis/runtime/runnerscalesets?runnerGroupId=<g>&name=<n>` | 200 list; none is "absent", more than one is an error | client.go, GetRunnerScaleSet |
| Scale set by id | `GET {svc}/_apis/runtime/runnerscalesets/{id}` | 200 | client.go, GetRunnerScaleSetByID |
| Create scale set | `POST {svc}/_apis/runtime/runnerscalesets`, body a scale set | **200** | client.go, CreateRunnerScaleSet |
| Update scale set | `PATCH {svc}/_apis/runtime/runnerscalesets/{id}` | 200 | client.go, UpdateRunnerScaleSet |
| Create session | `POST {svc}/_apis/runtime/runnerscalesets/{id}/sessions`, body `{"ownerName": "<owner>"}` | 200 session | session_client.go, createMessageSession |
| Refresh session | `PATCH {svc}/_apis/runtime/runnerscalesets/{id}/sessions/{sessionId}`, no body | 200 session | session_client.go, refreshMessageSession |
| Delete session | `DELETE {svc}/_apis/runtime/runnerscalesets/{id}/sessions/{sessionId}` | 204 | session_client.go, deleteMessageSession |
| Poll | `GET <messageQueueUrl>[?lastMessageId=<n>]` | 200 message, **202 none** | session_client.go, getMessage |
| Ack | `DELETE <messageQueueUrl path>/{messageId}` | 204 | session_client.go, deleteMessage |
| Acquire jobs (GHES only, section 6.1) | `POST {svc}/_apis/runtime/runnerscalesets/{id}/acquirejobs`, body a JSON array of request ids, queue token | 200 `{count, value: [ids]}` | session_client.go, acquireJobs |
| Acquirable jobs (GHES only, section 6.1) | `GET {svc}/_apis/runtime/runnerscalesets/{id}/acquirablejobs` | 200 `{count, value: [job]}`, **204 none** | ARC@c6e4c94: github/actions/client.go, GetAcquirableJobs |
| JIT config | `POST {svc}/_apis/runtime/runnerscalesets/{id}/generatejitconfig`, body `{"name", "workFolder"}` | 200 `{runner, encodedJITConfig}` | client.go, GenerateJitRunnerConfig |
| Runner by id | `GET {svc}/_apis/distributedtask/pools/0/agents/{runnerId}` | 200 runner | client.go, GetRunner |
| Runner by name | `GET {svc}/_apis/distributedtask/pools/0/agents?agentName=<name>` | 200 list; none is "absent" | client.go, GetRunnerByName |
| Remove runner | `DELETE {svc}/_apis/distributedtask/pools/0/agents/{runnerId}` | **204** (also for an absent runner, section 7) | client.go, RemoveRunner |

And on the REST API, with the installation token, for the trust check of section 6.2 (ADR 0090)
[docs: REST API endpoints for workflow runs; Permissions required for GitHub Apps, read
2026-11-12]:

| Operation | Request | Success | Permission |
|:--|:--|:--|:--|
| Workflow run of an assigned job | `GET {api}/repos/{owner}/{repo}/actions/runs/{run_id}` | 200 run: `event`, `actor`, `triggering_actor`, `repository`, `head_repository` | repository "Actions: Read" |
| Cancel a refused run (`cancel_refused_runs` only) | `POST {api}/repos/{owner}/{repo}/actions/runs/{run_id}/cancel` | 202; 409 when the run cannot be cancelled | repository "Actions: Write" |
| Fork-PR setting (`check_fork_pr_setting` only) | `GET {api}/orgs/{org}/actions/permissions/fork-pr-workflows-private-repos` | 200 `run_workflows_from_fork_pull_requests`, `send_write_tokens_to_workflows`, `send_secrets_and_variables`, `require_approval_for_fork_pr_workflows` | organisation "Administration: Read" |

mayfly never calls the delete-scale-set endpoint (ADR 0040), and the fake need not implement it.

**Shapes.** Field names are camelCase as listed. **Ids are 64-bit integers** in mayfly's types
(`ScaleSetId`, `RunnerId`, `RunnerRequestId`, `MessageId` are `u64`): the reference client
declares them as Go `int` and `int64`, 64 bits on its platforms [scaleset: types.go], and
message ids near `100000001` are server-observed (section 4.1).

- *Scale set*: `id`, `name`, `runnerGroupId`, `runnerGroupName`, `labels` (each `{type, name}`;
  the reference client defaults to one label equal to the name with type `System`),
  `RunnerSetting` (capital R, carrying `disableUpdate`), `createdOn`, `runnerJitConfigUrl`,
  `statistics` [scaleset: types.go]. A zero `createdOn` (`0001-01-01T00:00:00Z`) must decode.
- *Session*: `sessionId` (a UUID), `ownerName`, `runnerScaleSet`, `messageQueueUrl`,
  `messageQueueAccessToken`, `statistics` [scaleset: types.go].
- *Statistics*: `totalAvailableJobs`, `totalAcquiredJobs`, `totalAssignedJobs`,
  `totalRunningJobs`, `totalRegisteredRunners`, `totalBusyRunners`, `totalIdleRunners`, all
  integers [scaleset: types.go]; all seven present in a github.com envelope
  [server-observed: scaleset #107, 2026-09-29].
- *Runner reference*: `id`, `name`, `runnerScaleSetId` [scaleset: types.go].
- *Error body*: `{"typeName": "...", "message": "..."}`; a `text/plain` error body also occurs;
  a body may start with a UTF-8 byte-order mark, which is trimmed before decoding; the response
  headers `ActivityId` and `X-GitHub-Request-Id` identify the request for support
  [scaleset: errors.go, responseErrorCause; common_client.go, bufferResponseBody].

**Error classes** that matter to mayfly are told apart **by `typeName` only, never by status**:
the reference client maps them by name whatever the status [scaleset: errors.go,
responseErrorCause, wrapResponseErrorType], and the service has answered the same class with
different statuses (a busy runner: 400 in GitHub's 2022 design record, 409 in the reference
client's tests; section 7).

| `typeName` contains | Status seen | Meaning for mayfly | Strength |
|:--|:--|:--|:--|
| `AgentNotFoundException` | 404 | the runner is gone: removal is already done | client-assumed [scaleset: errors.go] |
| `AgentExistsException` | 409 | JIT name taken: look the runner up by name (section 6.3) | client-assumed [scaleset: errors.go] |
| `JobStillRunningException` | 400 or 409 | removal refused, the runner is busy (section 7) | maintainer-stated (400) [ARC@c6e4c94: docs/adrs/2022-12-27-pick-the-right-runner-to-scale-down.md]; client-assumed (409) |
| `RunnerScaleSetSessionConflictException` | 409 | a session exists for the scale set (section 3.1) | server-observed [ARC #4285, 2025-10-24; ARC #3351, 2024-03-14] |
| `RunnerScaleSetSessionExpiredException` | 400 | the session is gone: create a new one, never refresh again (section 3.1) | server-observed [ARC #4285, 2025-10-24] |
| `ConfigureTooManyRunnersException` | 429 | JIT minting throttled, for tens of minutes: back off (section 8) | server-observed, through the maintainer's commit message of scaleset `6a58b52` (#134) |
| any other, 400, 401, 404 or 409 | — | definite refusal of this request | client-assumed |

Error text that mayfly logs or returns never includes a URL's query string or user info, nor a
response body that may hold a token (the reference client redacts likewise
[scaleset: errors.go, redactedURL]).

## 3. The session

### 3.1 Lifecycle

One scale set has at most one live session, held by one listener (plan §7.2). mayfly's session
states are plan §7.2's `Unconfigured → Ensured → Opening → Active ⇄ Backoff | Degraded → Closing
→ Closed`.

- **Ensure.** Look the scale set up by name in its runner group, create it when absent, and
  journal its id. **GitHub deletes a scale set that no session has connected to for 7 days**
  [maintainer-stated: ARC@c6e4c94: docs/adrs/2022-10-27-runnerscaleset-lifetime.md, its
  `DormantRunnerScaleSetCleanupJob`]; the jobs assigned to it and not to a runner go back to
  GitHub's queue (same record). So a pool idle, paused for good or with its daemon stopped for a
  week finds its journaled id gone. That a session create or a scale-set read then answers 404
  is client-assumed [open: HW-02, item 9]; such an answer for the journaled id sends the stream
  back to `Unconfigured`, and `ensure` creates the scale set again and journals the new id
  (`RecordScaleSetRecreated`). The new set's message numbering starts afresh (cursor and floor
  reset) and the old set's reservations, classifications and refusals are dropped on that
  observation (`formal/scaleset.qnt`, `recreateScaleSet`, `dormantScaleSetIsRecreatedTest`;
  KER-06G). This is not deletion by mayfly (ADR 0040): mayfly re-creates its own set.
- **Create.** Answers the session id, the queue URL and token, the scale set, and the current
  statistics [scaleset: session_client.go, createMessageSession]. The reference listener refuses
  to start without a session id and statistics [scaleset: listener/listener.go, Run]. mayfly
  records the opening and its statistics as the session's initial demand (plan §7.2 step 5); it
  does not disguise them as a message with a made-up id, as the reference listener does with
  id −1.
- **Owner name.** A free string; mayfly sends the host identity and the pool, never a secret.
- **A second session** answers **409** with `typeName`
  `GitHub.Actions.Runtime.WebApi.RunnerScaleSetSessionConflictException` and a message "The
  actions runner scaleset <name> already has an active session." [server-observed: ARC #4285,
  2025-10-24; ARC #3351, 2024-03-14, "… for owner <owner>"]. A scale-set name is unique
  within its runner group, so two deployments using one name in one group conflict the same way
  [maintainer-stated: ARC #3351, 2024-03-15]. The reference
  client has no handling for it: any non-200 is an error and its listener exits
  [scaleset: session_client.go, doSessionRequest; listener/listener.go, Run]; ARC's earlier
  listener retried a conflict every 30 s, up to 10 times [ARC@c6e4c94:
  cmd/ghalistener/listener/listener.go] (client-assumed). How long an abandoned session blocks a
  new create is [open: HW-02, item 9]. mayfly's rule (A3): 409 on create means "a session
  exists"; mayfly deletes the session id it journaled (if any) and retries with backoff starting
  near 30 s, and never steals a session it has no record of; five 409s in a row are escalated to
  the operator (`SessionConflictEscalated`), and the retries go on.
- **Refresh.** On 401 from poll, ack or acquire, the session is refreshed with a PATCH and the
  call retried once; a refresh is skipped when another caller already replaced the expired
  session, so concurrent expiries refresh once [scaleset: session_client.go, GetMessage,
  refreshMessageSession]. **A refresh can answer `RunnerScaleSetSessionExpiredException`** ("The
  session identifier <uuid> is not valid.") [server-observed: ARC #4285, 2025-10-24]: the session
  is gone, so mayfly records the loss (`RecordSessionLoss{expired}`) and creates a new session,
  never refreshing again (the model's `sessionEnds` then `sessionLoss`; GH-03). How long a
  session lives is [open: HW-02, item 5]. Whether a refresh returns a new `messageQueueUrl` is
  [open: HW-02, item 5]; mayfly uses whatever the refresh returns, and the new URL passes the
  same host rule (section 1).
- **Delete.** On orderly shutdown, drain completion or re-open: DELETE answers 204
  [scaleset: session_client.go, deleteMessageSession]; a delete of an expired session answers
  the expired exception [server-observed: ARC #4285], which mayfly reads as "already gone". Pause,
  drain and safe mode keep the session open (ADR 0029).

### 3.2 Persistence

mayfly journals the session id (never its token) when the session opens and its deletion when
it closes, so a daemon restarted after a crash knows which session to delete before creating a
new one. The reference client keeps the session only in memory and deletes it on close
[scaleset: session_client.go, Close].

### 3.3 Token expiry

A 401 on a queue call is the expiry signal [scaleset: session_client.go, getMessage,
deleteMessage, acquireJobs] [server-observed: ARC #3204]. After one refresh and one retry, a
second 401 is a session failure: the session goes to `Backoff` and is re-opened, and a re-open
recomputes demand from its initial statistics (plan §7.2).

### 3.4 Statistics that outlive a session

After a GitHub incident, `totalAssignedJobs` stayed 15 above the running jobs for about a day,
**across listener and session re-creates**, and cleared only when the scale set was deleted and
created again [server-observed: ARC #4397, 2026-03-06; one report, a second similar one in the
same issue]. mayfly's `desired = min(deliverable, assigned)` bounds what such a count asks for,
and a runner starts only for a job classified trusted (section 6.2), so a stale count starts no
runner; it is reported instead: an `assigned − running` gap that stays wide is an operator alarm
(GH-04 note). mayfly never deletes the scale set to clear it (ADR 0040).

## 4. Messages

### 4.1 Long poll

`GET <messageQueueUrl>` with `Authorization: Bearer <queue token>`, `Accept: application/json;
api-version=6.0-preview` (here the version rides in `Accept`, not the query) and
`X-ScaleSetMaxCapacity: <n>` [scaleset: session_client.go, getMessage].

- **200** carries one message; **202** means the hold expired with nothing to deliver
  [scaleset: session_client.go, getMessage]. Whether a 202 carries a body or statistics is
  [open: HW-02, item 6]; mayfly ignores any.
- **A 200 with an empty body** occurs ("StatusCode 200 … EOF") [server-observed: ARC #4285,
  2025-10-23]. mayfly treats a 200 whose body is empty or does not decode as a transient poll
  failure: retried, nothing recorded, nothing acked; never protocol drift (GH-03).
- **The hold is about 50 s**: one empty hold measured 50.1 s, and polls in logs come about every
  50 s [server-observed: scaleset #122, 2026-07-25]. The reference client bounds each attempt
  at 2 minutes
  and retries a stalled attempt with a fresh window [scaleset: http_client.go,
  MessagePollTimeout; README.md]. mayfly bounds each poll attempt the same way, and polls again at
  once after handling a response.
- **A held poll does not wake for every event**: `JobStarted` arrived on the next poll, up to a
  hold later (p90 48.5 s over 2,069 jobs) [server-observed: scaleset #122, 2026-10]. So
  nothing in mayfly waits on a timely `JobStarted`: a runner's start is confirmed through the
  runner endpoints (ADR 0038).
- **Dead connections**: a listener stalled for about 25 minutes because every retry reused one
  dead HTTP/2 connection [server-observed: scaleset #131, 2026-09-24]; the reference client
  now pings silent
  HTTP/2 connections (send a ping after 30 s of silence, give up after 10 s)
  [scaleset: http_client.go, DefaultTransport]. mayfly's transport does the same or drops the
  connection after a timed-out attempt (GH-02H).
- `lastMessageId` is sent only when greater than zero [scaleset: session_client.go, getMessage].
  Passing the last processed id avoids reprocessing; omitting it returns the first available
  message [scaleset: README.md]. Whether it also acknowledges earlier messages implicitly is
  [open: HW-02, item 4]; mayfly always acks explicitly.
- **Message ids.** In 2026 logs ids are large, dense and increasing within a session: about
  `100000001` and up, `100000046 → 100000047`, `100001345`; they continue across a queue-token
  refresh in one session [server-observed: scaleset #122, #131; ARC #3204, 2025-04-18]. Logs from
  2023 and 2024 show small ids (180, 557, 13462) [server-observed: ARC #3204]. The reference
  listener's comment that the service "numbers real messages from 0" [scaleset:
  listener/listener.go, near `lastMessageID`] is contradicted by those values. So the numbering
  has changed at least once, and **nothing shows that ids keep increasing across sessions** of
  one scale set: A1's cross-session monotonicity is client-assumed only. mayfly therefore
  recovers from a restart instead of only degrading (section 9, A1;
  `formal/scaleset.qnt`'s `a1RestartIsRecoveredTest`): a session's first message whose id is
  strictly below the stream's cursor at the open starts a new numbering, recorded for the
  operator as `MessageIdsRestarted`, applied as new, and the cursor restarts from it. Within a
  session only the cursor can be redelivered (oldest first), so a message is a
  `DuplicateMessage` only when its inbox receipt exists **and** its id equals the cursor; any
  other id at or below the cursor, receipt or not, is a reused id: it applies nothing and
  degrades the session (`message_ids_regressed`; `restartWithinASessionDegradesTest`;
  KER-06G, GH-03). A reused id equal to the cursor reads as the redelivery, the one case no id
  can tell apart. Whether
  ids continue across sessions, a daemon restart and a scale set re-created is
  [open: HW-02, item 4]. mayfly never uses id 0 as a sentinel.

### 4.2 Envelope

`{"messageId": <int>, "messageType": "RunnerScaleSetJobMessages", "body": "<string>",
"statistics": {...}}`. The `body` is a **string** that itself holds a JSON array of job messages;
an empty body has none. The reference client rejects any other envelope `messageType`
[scaleset: client.go, parseRunnerScaleSetMessageResponse]. mayfly decodes an unknown envelope
type into a recorded, acked, counted protocol-drift fact instead (plan §7.2 step 6; dm §9.1).

### 4.3 Job messages

Each element carries its own `messageType` and common fields: `runnerRequestId`,
`repositoryName`, `ownerName`, `jobId`, `jobWorkflowRef`, `jobDisplayName`, `workflowRunId`,
`eventName`, `requestLabels`, `queueTime`, `scaleSetAssignTime`, `runnerAssignTime`,
`finishTime` [scaleset: types.go]. The variants:

| `messageType` | Extra fields | mayfly reads it as |
|:--|:--|:--|
| `JobAvailable` | `acquireJobUrl` | GHES: an offer to classify and maybe acquire (section 6.1) |
| `JobAssigned` | — | a job assigned to the scale set: on github.com classified before any runner starts (section 6.2); never counted for demand |
| `JobStarted` | `runnerId`, `runnerName` | a fact tying a job to a runner |
| `JobCompleted` | `result`, `runnerId`, `runnerName` | a fact; a cancel arrives as `result: "canceled"` |

On github.com a `JobAssigned` carried an explicitly present **`runnerRequestId: 0`**, and the
workflow run id, workflow ref, repository, owner, event name and requested label of the job
[server-observed: scaleset #107, 2026-06-02 and 2026-09-29]. So `runnerRequestId` is **not a
key** on github.com: mayfly's inbox, job facts and classifications never key or deduplicate on
it; a direct assignment is identified by `(jobId, workflowRunId)` and the message it arrived in
(KER-06G). `repositoryName` is the repository's name without its owner; the full name is
`ownerName/repositoryName` [ARC: cmd/ghalistener/scaler/scaler.go] (client-assumed).

The reference client skips unknown inner types silently and splits a batch into four lists by
type, losing the order between them [scaleset: client.go, parseRunnerScaleSetMessageResponse;
types.go]. mayfly keeps the batch in arrival order and records unknown types as protocol drift.
All strings are untrusted input: length-bounded and never interpolated (dm §9.1). Field presence
for all four types, null against absent, zero times, the full set of `result` and `eventName`
values, and whether a fork's pull request can be told from the message are
[open: HW-02, items 1 and 8]; until then fixtures are synthetic and say so, and a field the
classifier needs that is missing or unknown makes the job unclassifiable (A5).

A job can be assigned, not taken by a runner in time, completed as `canceled` and re-queued,
up to three times, each as new messages for the same workflow job [scaleset: README.md, Job
Reassignment]; after the third the job fails. The release came about 300 s after the
assignment in public reports [open: HW-02, item 2: measure it].

### 4.4 Ack by delete

`DELETE` of the message id under the queue URL acknowledges it; an unacknowledged message is
redelivered on the next poll [scaleset: session_client.go, deleteMessage; README.md]. The
reference listener acks after its in-memory handler returns, with no cancellation
[scaleset: listener/listener.go, Run]. A 404 on ack is reported as an error by the reference
client; whether it means "already acked" is [open: HW-02, item 4], and mayfly treats it as done
(the inbox record already exists).

## 5. Demand: statistics and capacity

- **Demand** is `statistics.totalAssignedJobs`, which counts jobs waiting for a runner and jobs
  running (`totalAssignedJobs >= totalRunningJobs`); never count job messages, since a response
  holds at most 50 and a backlog is truncated [scaleset: README.md]. mayfly's demand is
  `desired = min(deliverable, assigned)` (plan §7.2, ADR 0028). On github.com a refused job
  (section 6.2) is counted in `totalAssignedJobs` while GitHub holds it assigned.
- **Capacity** is the `X-ScaleSetMaxCapacity` header on every poll [scaleset: client.go,
  HeaderScaleSetMaxCapacity; session_client.go, getMessage]. It is the scale set's **total**
  capacity, not the free slots: the reference listener sends the same configured value on every
  poll [scaleset: listener/listener.go, Run]. Zero is valid [scaleset:
  listener/listener.go, Config.Validate].
- **The header caps assignment, and so `totalAssignedJobs`.** The service does not assign a job
  to a scale set that advertises 0 [maintainer-stated: ARC #3446, 2025-07-01, "the back-end
  would not assign jobs to the scale set anymore if the capacity is 0"; and 2024-04-18, it
  "will not offer any jobs to the listener that it won't be able to handle"]. On github.com,
  scale sets advertising 0 kept `totalAssignedJobs` at 0 and received no `JobAssigned` or
  `JobAvailable` for matching queued jobs over 100 s with healthy sessions; advertising 1 with no
  runner, a `JobAssigned` arrived and `totalAssignedJobs` became 1, with no acquire call
  [server-observed: scaleset #133, 2026-09-30]. So a pool carved 0 never sees the job that
  waits for it: ADR 0069's `demand` policy starves it (U1 fails), as
  `formal/scheduling.qnt`'s `demandStarvesWhenDemandIsInvisibleTest` models. `capacity_policy`
  stays `exact` by default and `demand` is an explicit, **experimental** opt-in (ADR 0069,
  amended 2026-11-11). What it does between 0 and the backlog, whether lowering the header
  un-assigns a job, whether `totalAvailableJobs` counts a job held back by 0, and the plain-text
  400 "capacity" error are [open: HW-02, item 3].
- **Stranding.** A job queued while every matching scale set advertises 0 (all paused, draining
  or in safe mode) is assigned to none; and a scale set with no live session can keep jobs
  assigned to it until GitHub releases them (section 4.3). Both are consequences of the cap,
  documented for operators (OPS-04).
- **Changing capacity.** The value may change between polls of one session: the reference
  listener exposes `SetMaxRunners`, documented as safe to call while it runs, which stores the
  value atomically and is read on the next poll [scaleset: listener/listener.go,
  Listener.SetMaxRunners, Run]. mayfly does the same with no blocking: each poll sends the
  scale set's current **reported max** (≤ its deliverable carve-out, I24), which pause, drain and
  safe mode change without closing the session (ADR 0029).

## 6. Assignment, trust and JIT

### 6.1 Two flows

**github.com assigns jobs directly.** A `scaleset` maintainer wrote that "the job acquiring
logic has been removed for a while from the API" [maintainer-stated: scaleset #75, 2026-04-13];
on github.com a `JobAssigned` arrives with no `JobAvailable` and no acquire call, and
`totalAssignedJobs` counts it [server-observed: scaleset #107, 2026-09-29; scaleset #133,
2026-09-30]. The reference client's acquire step was restored for GitHub Enterprise Server,
whose 3.18 and 3.19 still offer and need it (scaleset #90, `22bae12`; the version range is a
user's report in scaleset #75, so it is client-assumed). Whether github.com still ever sends
`JobAvailable` is [open: HW-02, item 2]. So mayfly has two flows, one per service, and models
both (`formal/scaleset.qnt`, `initDirect` and `initAcquire`):

- **github.com (direct).** GitHub assigns up to the advertised `maxCapacity`; each `JobAssigned`
  is classified before any runner starts (section 6.2). There is nothing to acquire and nothing
  to decline: a job mayfly will not run is **refused**.
- **GHES (acquire).** The listener's handler must acquire every `JobAvailable` it wants, or the
  job stays unassigned [scaleset: listener/listener.go, Scaler]. The body is a JSON array of
  `runnerRequestId`s; the answer's `value` lists the ids acquired [scaleset: session_client.go,
  acquireJobs]. Each offer is classified **before** it is acquired, and an offer the pool's tier
  does not admit is declined, never acquired (ADR 0027, kept for this flow by ADR 0090). An
  acquire right after a session refresh once answered 400 [server-observed: ARC #3204,
  2024-02-11]; mayfly reads an acquire 400 as a definite refusal of that batch. The reference
  example calls re-acquiring an acquired job a no-op [scaleset: examples/dockerscaleset/scaler.go,
  Scale] (client-assumed). Whether a partial answer omits ids no longer available or fails the
  request, and whether a repeat is a no-op on the service, are [open: HW-02, item 2] (A4).
  mayfly journals the acquire plan before the call (I23, I32) and the returned set as an
  observed fact; an id not returned is not acquired. **On a session (re)open**, a GHES listener
  lists `acquirablejobs` and plans them like offers, since an offer made while no session lived
  is not offered again to the next one: ARC's earlier listener did so whenever the opening's
  statistics showed available or assigned jobs [ARC@c6e4c94:
  cmd/githubrunnerscalesetlistener/autoScalerMessageListener.go], and GHES users list them for
  the same reason [scaleset #75] (client-assumed; GH-03, GH-04).

### 6.2 Trust on github.com (ADR 0090)

Runners of one scale set are interchangeable: GitHub gives an assigned job to any idle runner of
the set (ADR 0027). With no offer to decline, the owner decided (2026-11-11 and 2026-11-12):

1. **Start-up.** mayfly checks only that each pool's scale set exists (or creates it) and that
   its session is reachable. A runner group is optional, any GitHub plan works (Free included),
   and public repositories are allowed: the runner group is not the trust boundary. Runner groups
   restricted to selected repositories need a paid plan ("Organization owners using the GitHub
   Team plan can create additional organization-level runner groups") [docs: Managing access to
   self-hosted runners using groups], which is why the boundary is the per-job check. **Public
   repositories still need the group to admit them**: "By default, only private repositories can
   access runners in a runner group" [same page], so GitHub routes a public repository's jobs to
   the scale set only when its group allows public repositories. mayfly reads the group with
   `GET {api}/orgs/{org}/actions/runner-groups/{group_id}` ("Self-hosted runners", which it holds)
   and reports, at start-up and in `doctor`, each allowed repository that is public while
   `allows_public_repositories` is false; it reports and does not refuse (GH-08).
2. **Per job, before any runner starts: the boundary.** Every `JobAssigned` is classified, and
   while one is unclassified no runner of the scale set starts:
   - its repository (`ownerName/repositoryName`) must be one of the pool's `allowed_repositories`
     (config);
   - its workflow run is looked up, `GET /repos/{owner}/{repo}/actions/runs/{run_id}` with the
     installation token ("Actions: Read", section 2): `head_repository` against `repository` (a
     fork's pull request when they differ), `event`, `actor`;
   - **allowed events** (proposed list, ADR 0090): `push`, `pull_request` whose head repository
     is the base, `workflow_dispatch`, `schedule`, `merge_group`, `release`: each runs code in the
     repository or on one of its branches, put there or started by someone with write access;
   - **refused**: every other event (a fork's `pull_request`, `pull_request_target`,
     `workflow_run`, `issue_comment`, `pull_request_review`, `pull_request_review_comment`,
     `repository_dispatch`, ...), anything unclassifiable (a field missing, empty or unknown, A5),
     and any job whose run could not be looked up (an error, a timeout, an absent run: a failed
     lookup refuses, so nothing runs unclassified). A per-repository allowance for forks may come
     later, by a new decision;
   - a new assignment of a job (after a release) is classified afresh: its earlier refusal or
     classification does not carry over (`reassignedJobIsClassifiedAfreshTest`);
   - a refusal is journaled, `AssignedJobRefused{reason}`, and shown to the operator: an event,
     never only a log line.
   That `workflowRunId` in a `JobAssigned` names the run the REST API knows, and that the lookup
   works with "Actions: Read" alone, are [open: HW-02, item 8].
3. **Refused jobs** ("3a but as a configuration option"). The refused job holds its slot of
   `maxCapacity` while GitHub keeps it assigned, and no runner of the scale set starts meanwhile,
   since any new runner could take it.
   - **Default:** mayfly cancels nothing. GitHub's own release and requeue (section 4.3) fails the
     job at its third release. The hold ends when the job is observed gone: a `JobStarted` or
     `JobCompleted` naming it, or the scale set re-created after a dormant deletion (section 3.1) (whether a github.com `JobCompleted` names it is [open: HW-02, item 1]), when
     the refusal lapses after the pool's `refusal_release_window` (at least three release
     times; 30 minutes by default until HW-02 item 2 measures the release; A7), or when the
     operator runs `mayfly pool release-refusal <pool> <job>`, refused while GitHub still holds
     the job assigned, so it lifts a hold and asserts no outcome (GH-09).
   - **`cancel_refused_runs`** (per pool, default false, "Actions: Write"): mayfly asks GitHub to
     cancel the refused run at once (section 2) and holds only until the cancellation is observed
     (the job leaves the assignment). A cancel that fails (409, an error) or whose answer is lost
     changes nothing at GitHub, and the hold falls back to the release window (GH-10).
4. **`check_fork_pr_setting`** (per organisation, default off, "Administration: Read"): at
   start-up mayfly reads the organisation's fork-PR setting for private repositories (section 2)
   and refuses to start when `run_workflows_from_fork_pull_requests` is true. The run lookup
   already refuses a fork's run; this is defence in depth.

Pinned by `formal/scaleset.qnt`'s `vmOnlyForAJobClassifiedTrusted`, `refusedJobNeverGetsAVm`,
`assignedJobsClassified`, `cancelOnlyRefused`, `assignedUntrustedJobIsRefusedTest`,
`failedLookupRefusesTest`, `cancelledRefusedRunReleasesTheHoldTest`,
`failedCancelFallsBackToTheWindowTest`, `refusedJobFailsThenTrustedJobRunsTest`,
`refusalExpiresWhenTheReleaseNamesNoJobTest` and `operatorReleasesARefusalTest`; the Rust side
is KER-06G (aggregate and gate), GH-08 (config and start-up), GH-09 (operator release, window),
GH-10 (lookup and cancel).

**What the per-job check cannot do.** A runner started for a trusted job may take an untrusted
job GitHub assigned after the last message mayfly recorded, whether before or after the runner
started (`runnerMayTakeAJobAssignedBeforeItStartedTest`,
`runnerMayTakeAJobAssignedAfterItStartedTest`; not invariants): mayfly cannot know of an
assignment it has not received, and nothing in the scale-set protocol ties a job to a runner.
The per-job check stops every runner it can, `cancel_refused_runs` shortens what remains, and
with public repositories allowed this race is the residual risk SEC-01 records. ADR 0090's
proposed amendment adds a job-started hook in the guest that fails such a job before its first
step (`ACTIONS_RUNNER_HOOK_JOB_STARTED`: "If there is any other exit code, the job will not run
and will be marked as failed" [docs: Running scripts before or after a job]); IMG-02.

### 6.3 JIT config

`generatejitconfig` takes a runner name (mayfly: the VM name, issued once per registration, I19)
and a work folder, and answers the runner reference (its id is journaled) and
`encodedJITConfig`, a secret [scaleset: client.go, GenerateJitRunnerConfig; types.go]. A taken
name answers 409 `AgentExistsException` [scaleset: errors.go, responseErrorCause]; ARC then
looks the runner up by name and removes it only when it belongs to its own scale set
[ARC: controllers/actions.github.com/ephemeralrunner_controller.go]. mayfly instead removes only
a runner whose id it journaled (ADR 0029) and otherwise reports the collision. Under load the
service answered `generatejitconfig` with 429 `ConfigureTooManyRunnersException` for about 40
minutes (section 8): a mint directive survives a long throttle without being re-issued.

**Guest side.** The JIT config travels from the control plane to the data plane as a
`SecretHandle`, and into the guest only on the stdin of `tart exec -i <exact VM> <fixed
launcher>` (ADR 0038, plan §8.3). Inside the guest the runner consumes it in one of two ways:
the `--jitconfig` argument of `run`, or the environment variable
`ACTIONS_RUNNER_INPUT_JITCONFIG`, which the runner reads, masks as a secret and removes from its
own environment [runner: src/Runner.Listener/CommandSettings.cs, constructor and GetJitConfig;
src/Runner.Common/Constants.cs, Args.Secrets]. The reference example uses the environment
variable [scaleset: examples/dockerscaleset/scaler.go]. mayfly's launcher reads stdin and starts
the runner with the environment variable, never the argument (an argument is visible in the
guest's process list). The value is base64 of a JSON object mapping file names to base64
content; the runner writes those files (its registration and credentials) into its root
directory [runner: src/Runner.Listener/Runner.cs, ExecuteCommand]. That write happens inside the
disposable guest and dies with the VM; mayfly itself never writes the config to disk (ADR 0014).

**Confirmation and lifetime.** After delivery the control plane confirms through the runner
endpoints that the issued runner id came online (ADR 0038). "If JIT runners never run a job,
they will automatically be removed", and an ephemeral runner not connected for more than a day
is removed [docs: Removing self-hosted runners]; ARC states that the service drops a
registration once the runner stops reporting in [ARC: controllers/actions.github.com/
runner_unregistration.go], and an ephemeral runner which ran a job is deregistered by the
service when it finishes [docs: Self-hosted runners reference]. When exactly an unconnected JIT
registration goes is [open: HW-02, item 10]; mayfly's reaper never waits for it.

**Ghost runners.** When an ephemeral runner's own acquire of its job fails (409
`TaskOrchestrationJobAlreadyAcquiredException`, 404 `TaskOrchestrationJobNotFoundException`), the
service deregisters it but its process keeps running, "Listening for Jobs", for as long as
observed (32 minutes) [server-observed: actions/runner #4617, 2026-08; open upstream]. So the
guest is never trusted to exit: mayfly retires a VM whose runner id reads 404 while its VM still
runs, and every attempt has a deadline (CP-05, KER-09 note).

## 7. Runner removal

`DELETE {svc}/_apis/distributedtask/pools/0/agents/{runnerId}` answers **204 when the runner was
removed or did not exist** [maintainer-stated: ARC@c6e4c94:
docs/adrs/2022-12-27-pick-the-right-runner-to-scale-down.md]; so a 204 proves the runner is gone,
not that mayfly removed it, and mayfly records `deregistered` (gone), never "removed by us", from
a 204 alone. A busy runner answers **400** with `JobStillRunningException` in that design record
and 409 in the reference client's tests; a gone one 404 `AgentNotFoundException`
[scaleset: errors.go, responseErrorCause] (client-assumed). mayfly classifies by `typeName`
only (section 2). ARC retries a busy runner after 30 s and treats not-found as removed [ARC:
controllers/actions.github.com/runner_unregistration.go]. mayfly: gone (204 or not-found)
completes the retirement; busy cancels it (plan §7.3, I21); the reaper removes only journaled
ids (ADR 0029). The REST endpoint for the same operation (`DELETE
/orgs/{org}/actions/runners/{id}`) documents 422 "Validation failed, or the endpoint has been
spammed" [docs]; mayfly does not use it for its scale set's runners. Every removal has its
own deadline (GH-02). The exact bodies are [open: HW-02, item 9].

## 8. Retry, backoff and rate limits

The reference client's policy, which mayfly takes as its starting point [scaleset:
http_client.go, DefaultRetryConfig, DefaultShouldRetry, DefaultBackoff, DefaultOnRateLimit,
parseRetryAfter; errors.go, rateLimitFromResponse]:

- Retry transport errors (but not certificate errors), 429, status 0 and 5xx except 501; at
  most 4 retries, from 1 s doubling to a 30 s cap, with up to 50 % added jitter. Jitter comes
  from mayfly's randomness port; waits use Tokio time, so tests run under `tokio::time::pause`.
- A 429 or 503 with `Retry-After` (seconds or an HTTP date) waits that long, uncapped.
- **Rate limits**: a 429, or a 403 with `Retry-After`, `X-RateLimit-Remaining: 0` or a body
  mentioning a secondary rate limit. Primary means remaining 0 without the secondary text;
  `X-RateLimit-Reset` is epoch seconds. By default a rate-limited 429 is retried and a
  rate-limited 403 is not, except during the admin-token exchange.
- **JIT throttling**: `generatejitconfig` answered 429 `ConfigureTooManyRunnersException` for
  about 40 minutes during load testing [server-observed, through the maintainer's message of
  scaleset commit `6a58b52`, #134]. A throttled mint backs off within the directive's deadline
  and is not re-minted under a new name (I19); whether it carries `Retry-After` is
  [open: HW-02, item 6].
- GitHub's REST limits for an installation and the advice to wait at least a minute on a
  secondary limit without `Retry-After` are [docs]. Whether the Actions service and the queue
  host send `Retry-After` or `X-RateLimit-*`, and what a throttled poll looks like, are
  [open: HW-02, item 6].

mayfly adds what the reference leaves to its caller: a request that runs out of retries moves
the session to `Backoff` (never closes it on a transient error), repeated failure to `Degraded`;
the reaper lists runners at most once an hour (ADR 0029); every request has a deadline, and a
wait that would outlast it is not taken; a connection whose attempt timed out is not reused
(GH-02H).

## 9. Where mayfly differs from the reference listener

| Reference behaviour | mayfly | Why |
|:--|:--|:--|
| Acks after an in-memory handler returns [scaleset: listener/listener.go, Run] | Acks **only after the inbox record commits**: inbox key, statistics, job facts, declined offers, refused assignments and desired count in one store transaction (plan §7.2 steps 3-4); a refused command acks once its refusal receipt commits (GH-04, ADR 0041) | A crash between handling and ack must lose no fact; a redelivery is a `DuplicateMessage`, acked without a write |
| The handler may do VM and JIT work inline, blocking the poll [scaleset: examples/dockerscaleset/scaler.go] | The poll loop records and acks only; the domain decides; VM work and JIT minting are directives and effects run elsewhere; no blocking call inside the domain (`decide`/`evolve` are pure) | A slow clone must not starve the 50 s poll or delay acks into redelivery storms |
| `lastMessageId` kept in memory only [scaleset: listener/listener.go, Run] | The last acked message id and every inbox key are journaled; after a restart the listener resumes from the journal; a restarted numbering is recognised and recorded | No reprocessing after a restart; a redelivered id is recognised |
| Any error ends the listener, and its process exits [scaleset: listener/listener.go, Run] | Errors are classified: token expiry is handled in the client; transient errors back off and keep the session; a lost or expired session re-opens; a deleted scale set is re-created; protocol drift degrades the session, never safe mode | The daemon survives service hiccups |
| Session created in the client's constructor, deleted on close [scaleset: client.go, MessageSessionClient] | Opening and closing a session are commands with journaled outcomes; on start the journaled stale session is deleted before a new one is created (section 3.2) | Crash recovery without a 409 loop |
| Capacity is a configured number [scaleset: listener/listener.go] | Capacity is the scale set's reported max, changing with the host's capacity and the pool's mode (section 5) | ADR 0024, ADR 0029 |
| Every assigned job is scaled for | A runner starts only for a job whose run was looked up and classified trusted, never while one is unclassified or refused (section 6.2) | ADR 0090 |
| A completed job removes its runner's container | `JobCompleted` is an observation; a VM is deleted only with the ownership proof (plan §8.4) | Ownership rule |
| Initial statistics passed as a message with id −1 | Recorded as the session-open fact | Message ids mean service ids only |

**Listener crash recovery.** If the daemon dies between commit and ack, the message is
redelivered and recognised by its inbox key, acked without a write. If it dies before commit,
nothing was recorded and the redelivery is handled afresh. If it dies holding a session, the next
start deletes the journaled session, opens a new one, and recomputes demand from that session's
statistics (plan §7.2 step 5). Acquires are journaled before the call, so a redelivered offer is
not re-planned past what was already planned (I23); a message recorded before the restart and
redelivered in the new session applies nothing again (A1), and one emitted before the restart
but recorded after it applies its older statistics until the backlog is consumed (A6).

## 10. The fake and the contract

`mayfly-fake-github` (GH-01) implements this document, independently of any production crate
(ADR 0012) and of the reference implementation's tests (ADR 0048): the auth chain (with
short-lived admin and queue tokens to exercise refresh), scale-set lookup and create, sessions
with one live session per scale set (409 `RunnerScaleSetSessionConflictException`), refresh
answering the expired exception, the long poll with a 202 hold, redelivery until delete, the
50-message cap, statistics, **both flows** (direct assignment capped by the advertised
`maxCapacity`, with `runnerRequestId` 0; offers and acquire for GHES), release and requeue of an
assigned job no runner takes (failed at the third release), a `JobCompleted` that names no job
(knob), JIT, runner get/remove (204 also for an absent
runner), the REST workflow-run lookup and cancel of section 6.2 (a lookup that fails, a cancel answering
409 or lost: knobs), the runner-group read of the public-repository report, dormant deletion of a scale set,
and fault knobs (401, 403, 409, 422 on the REST path, 429 with `Retry-After`, 429
`ConfigureTooManyRunnersException`, a 200 with an empty body, a response dropped after its
effect, a stalled poll, a restarted message numbering). Every behaviour marked client-assumed or
[open: HW-02] is a knob or a named assumption until it is server-observed. GH-03's recorded,
redacted fixtures (`tests/fixtures/github-scaleset/`) are the contract; the same suite runs
against the fake and, owner-gated, against a live disposable organisation (ADR 0015, HW-02).

## 11. Attribution

This specification was informed by reading GitHub's `actions/scaleset` (MIT, Copyright GitHub,
Inc.) at commit `716908e`, and `actions/runner` (MIT) at `310a370`; ARC (Apache-2.0) at
`89f34c4` and `c6e4c94` was read for behaviour only, and public issues and GitHub's
documentation for evidence. No code, tests, test vectors or recorded payloads were copied or
translated, so no third-party notice is required today; the short quotations of error messages
and documentation above are cited as evidence. Should any fragment ever be copied, ADR 0048's
rule applies: it ships with the upstream licence notice in `THIRD-PARTY-NOTICES` and is listed
there. A payload quoted in an issue is never copied into a fixture: HW-02 records its own.

## 12. The HW-02 remainder (GH-00)

GH-00R (2026-11-11) settled what public sources settle; this is what remains, for GH-00's
redacting capture in HW-02's session on a disposable App and organisation. Each item names what
to capture; an answer becomes a fixture under `tests/fixtures/github-scaleset/`, raises its
entry above to server-observed, and a refuted assumption changes `formal/scaleset.qnt` first.

The items were renumbered by GH-00R: the former item 6 (does the capacity header cap
`totalAssignedJobs`) is answered in section 5 and its remainder is item 3 below; references to
"§12 item 6" in records written before 2026-11-11 mean that.

Already settled and **not** on the list: the second session's 409 and `typeName` (section 3.1),
the queue token's lifetime of about an hour (section 1), the cap at 0 (section 5), the hosts
(section 1), the App permission list (section 13; HW-02 only confirms it).

Until then the `ScaleSet` aggregate and its model (KER-06, `formal/scaleset.qnt`) take the safe
side of these, each named where it is used: **A1** (item 4; client-assumed, refuted in part)
within a numbering ids increase and are redelivered oldest first, so the statistics cursor
survives a new session, no reservation depends on a message being recorded once, and a message
at or below a session's floor (the cursor at its open) applies nothing; the only such message A1
explains is the stream's last record redelivered as the session's first; a session's first
message strictly below the floor starts a new numbering (`MessageIdsRestarted`), assumed to leave
no message of the old numbering queued; any other degrades the session
(`message_ids_regressed`); a restarted id that happens to equal the floor, as the session's first
message, cannot be told from the redelivery; **A2** (items 2, 3) the acquire budget counts
every open reservation on top of `totalAssignedJobs` (it may acquire less, never more); **A3**
(item 9) a 409 on create backs off, mayfly deletes only a session it journaled, and the fifth 409
in a row is escalated to the operator (`SessionConflictEscalated`) while the creates go on;
**A4** (item 2) acquire answers the subset it acquired, and a lost answer leaves the ids pending
and counted; **A5** (items 1, 8) an offer or an assignment whose classifying fields are missing or
unknown is unclassifiable and declined or refused (`unclassifiable`, apart from an untrusted
trigger's `tier`); **A6** (items 1, 4) a message's statistics are the scale set's when the message
was emitted, and a create answers no message id, so a message emitted before a session opened but
recorded after it (a crash before its record, or a backlog queued while no session was live)
cannot be told from a newer one: statistics are ordered by message id only, and such a message's
older statistics replace the opening's until the backlog is consumed. That stays within I23,
because every acquisition remains a counted reservation until a message at least as new releases
it (A2), and it converges, because every change of the count emits a job event; keeping the
opening's statistics instead would never converge. If the capture shows that statistics are read
at delivery, A6 goes away. **A7** (items 1, 2; ADR 0090, owner 2026-11-12) a refusal or a
classification that no job fact releases lapses after the pool's `refusal_release_window`, which
is assumed to outlast GitHub's three releases, so the job is no longer assigned when it lapses;
the default errs long (30 minutes) until item 2 is measured.

1. **Payload corpus.** Full envelopes for all four job types on github.com: a success, a
   failure, a cancellation, a `workflow_dispatch` and a `pull_request`: field presence, nulls,
   zero times, `result` and `eventName` values, `requestLabels`, and which fields identify the
   job in a `JobStarted` and a `JobCompleted` when `runnerRequestId` is 0 (A5, A6, A7, section
   4.3).
2. **Which flow, and the release.** Whether github.com ever sends `JobAvailable` (if an offer
   appears: acquire a valid id, the same id twice, and a bogus id: A2, A4); and the time from a
   `JobAssigned` no runner takes to its `JobCompleted canceled` and requeue, three times, and to
   the job's failure (section 4.3): it sets `refusal_release_window` (ADR 0090, A7).
3. **Capacity beyond 0.** With `maxCapacity` 0, queue one matching job and read
   `totalAvailableJobs` and `totalAssignedJobs` for 2 minutes, then advertise 1 and see it
   assigned; with 2 assigned, lower the header to 1 and see whether one is un-assigned; the
   plain-text 400 "capacity" error if any (section 5). This decides whether any `demand` variant
   can see waiting jobs (ADR 0069).
4. **Message ids and acks.** Ids across a session delete and create, a daemon restart, and a
   scale set deleted and re-created; a `lastMessageId` behind the last acked id (implicit ack);
   deleting the same message id twice (A1, A6, sections 4.1, 4.4).
5. **Tokens and sessions.** Decode the admin token's `exp − iat`; force a session refresh and
   compare `messageQueueUrl`; note when a session expires on its own (sections 1, 3.1).
6. **The 202 and headers.** The 202's body bytes, and every response header on the Actions
   service and queue hosts (`Retry-After`, `X-RateLimit-*`), including on a JIT 429 if one occurs
   (sections 4.1, 8).
7. **Permissions.** The whole chain, the run lookup and (with the options) the cancel and the
   fork-setting read, with exactly the published permissions of section 13 and nothing else.
8. **Classification (ADR 0090).** Whether every `JobAssigned` carries repository, owner,
   `workflowRunId`, `eventName` and labels; that `workflowRunId` is the run the REST API returns;
   and the run lookup's `head_repository` for a pull request from a fork against one from a
   branch (a fork in the disposable organisation only) (section 6.2).
9. **Conflict, removal and deletion answers.** How long an abandoned session blocks a create
   after `kill -9`; what a session create and a scale-set read answer after the scale set is
   deleted (404 is client-assumed; delete one in the disposable organisation); delete an unknown runner id (204 or 404) and a busy runner (400 or 409) and record
   the bodies; create a scale set twice under one name; the presence of `runnerJitConfigUrl` and
   `statistics` in a create's answer (sections 3.1, 7).
10. **Unconnected JIT runner.** Mint a JIT config, never start it, and read its registration
    hourly until it goes (up to a day) (section 6.3).
11. **Job-started hook (if the owner accepts ADR 0090's amendment).** When the hook runs
    relative to a job container's setup, and that a hook exiting non-zero fails the job with no
    step run (section 6.2).

## 13. GitHub App permissions (ADR 0045, draft)

The minimal list, from GitHub's documentation and the owner's decisions of 2026-11-12 (ADR
0090), to be confirmed against a real App in HW-02 (item 7) before it is published as verified
(`PUBLISHED_PERMISSIONS` stays unverified until then):

| Permission | Level | When | Why |
|:--|:--|:--|:--|
| Organisation: Self-hosted runners | Read and write | always | the runner-registration exchange, scale sets, sessions, JIT configs, runner reads and removal [docs: Authenticating ARC to the GitHub API] |
| Repository: Actions | Read | always, on the allowed repositories | the workflow-run lookup of every assigned job (section 6.2) [docs: Permissions required for GitHub Apps] |
| Repository: Actions | Write (instead of Read) | only with `cancel_refused_runs` | cancelling a refused run (section 6.2) |
| Organisation: Administration | Read | only with `check_fork_pr_setting` | reading the fork-PR setting for private repositories (section 6.2) |

Nothing else. The runner-group read of the public-repository report (section 6.2) is covered by
"Self-hosted runners". A GitHub App that holds any repository permission also
holds "Metadata: Read-only", which GitHub grants with it; mayfly does not use it. mayfly does
not read repository contents and does not manage the organisation. `mayfly doctor` warns when
the App holds more than its pools' options need (ADR 0045). Repository scope, for tests only,
needs the repository permissions "Administration: Read and write" and "Metadata: Read-only"
instead [docs: Authenticating ARC to the GitHub API]. Enterprise scope cannot use an App at all
(section 1).
