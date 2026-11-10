# The Runner Scale Set protocol, as mayfly uses it

This is mayfly's own specification of the GitHub Actions Runner Scale Set protocol: the
credentials, endpoints, message flow and failure behaviour that `mayfly-adapter-github` (GH-02,
GH-03), `mayfly-fake-github` (GH-01) and the listener use case (GH-04) are built from. It is
written in our words from reading a reference implementation and GitHub's documentation; nothing
here is copied code (ADR 0048).

**Sources and how facts are marked.** GitHub publishes the protocol only through its
implementations. We read, and did not port:

| Short name | Repository | Pinned commit | Licence |
|:--|:--|:--|:--|
| `scaleset` | `github.com/actions/scaleset` (GitHub's Go client and listener) | `716908e` | MIT |
| `runner` | `github.com/actions/runner` (the runner that consumes a JIT config) | `310a370` | MIT |
| `ARC` | `github.com/actions/actions-runner-controller` | `89f34c4` | Apache-2.0 (behaviour cited, never code) |

Each fact carries one of these marks:

- **[scaleset: file, function]** (or `runner:`, `ARC:`): verified by reading that file at the
  pinned commit. It says what the reference client *does*, which is evidence of what the service
  expects, not a guarantee of what the service does.
- **[docs]**: stated in GitHub's public documentation.
- **[unverified: GH-00]**: not settled by either; GH-00's live capture answers it (section 12).
  Until then the fake implements the stated assumption and the contract tests name it.

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
   getRunnerRegistrationToken]; mayfly also uses it for the App permission probe (ADR 0045, GH-02).
3. **Registration token.** `POST {api}/orgs/{org}/actions/runners/registration-token` (repository
   scope: `/repos/{owner}/{repo}/...`), `Authorization: Bearer <installation token>`, empty body;
   201 with `token` and `expires_at` [scaleset: client.go, getRunnerRegistrationToken,
   createRegistrationTokenPath]. Lifetime one hour [docs]. **mayfly uses it for one thing only:
   as the input of step 4.** It never registers a runner with it (no classic or `--ephemeral`
   mode, ADR 0028; ADR 0048 records this refinement).
4. **Admin token and Actions service URL.** `POST {api}/actions/runner-registration`,
   `Authorization: RemoteAuth <registration token>`, JSON body `{"url": "<config URL>",
   "runner_event": "register"}`, where the config URL is the organisation's (or repository's)
   `https://github.com/...` URL; a 2xx answers `url` (the Actions service base, opaque to us) and
   `token` (a JWT) [scaleset: client.go, getActionsServiceAdminConnection]. Both fields are
   required. The token's expiry is read from its `exp` claim without verifying the signature;
   a token without `exp` is refused [scaleset: client.go, actionsServiceAdminTokenExpiresAt].
   This one exchange also retries 401 and 403, because a freshly minted registration token is not
   always usable at once [scaleset: client.go, getActionsServiceAdminConnectionRequest]. Lifetime
   [unverified: GH-00].
5. **Message-queue token.** Returned with the session (section 3) as `messageQueueAccessToken`,
   with the queue's URL `messageQueueUrl`. It authorises poll, ack **and acquire**
   [scaleset: session_client.go, getMessage, deleteMessage, acquireJobs]. Lifetime
   [unverified: GH-00]; expiry shows as 401 (section 3.3).

**Refresh.** The admin token is refreshed lazily, before a request, when it expires within 60 s;
refresh reruns steps 1–4, serialised so that concurrent callers refresh once
[scaleset: client.go, updateTokenIfNeeded, actionsServiceAdminTokenSnapshot]. mayfly keeps the
same 60 s margin and single-flight refresh; its clock comes from the time port (ADR 0020), never
read inline. The App key is asked of its provider again on each refresh, so rotation needs no
restart (ADR 0033).

**API base.** For `github.com` the REST base is `https://api.github.com`; for GitHub Enterprise
Server it is `https://<host>/api/v3` [scaleset: config.go, gitHubAPIURL]. mayfly supports
organisation scope, repository scope for tests, and enterprise scope later (ADR 0040); GHES is a
config field, never an environment read.

## 2. Requests to the Actions service

Every call to the Actions service (`{svc}` below, the URL of step 4) carries
`Authorization: Bearer <admin token>`, `Content-Type: application/json` and the query parameter
`api-version=6.0-preview`, added when absent [scaleset: client.go,
newActionsServiceRequestWithQuery, requestURL]. Exceptions: the message-queue calls use the queue
URL and token (section 4), and acquire uses `{svc}` with the queue token.

**User-Agent.** The reference client sends a JSON object (system, version, commit, scale-set id,
`kind`) [scaleset: common_client.go, client.go, userAgentString]. mayfly sends its own values
(`system: "mayfly"`, version, commit); whether the service reads it is [unverified: GH-00].

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
| Acquire jobs | `POST {svc}/_apis/runtime/runnerscalesets/{id}/acquirejobs`, body a JSON array of request ids, queue token | 200 `{count, value: [ids]}` | session_client.go, acquireJobs |
| JIT config | `POST {svc}/_apis/runtime/runnerscalesets/{id}/generatejitconfig`, body `{"name", "workFolder"}` | 200 `{runner, encodedJITConfig}` | client.go, GenerateJitRunnerConfig |
| Runner by id | `GET {svc}/_apis/distributedtask/pools/0/agents/{runnerId}` | 200 runner | client.go, GetRunner |
| Runner by name | `GET {svc}/_apis/distributedtask/pools/0/agents?agentName=<name>` | 200 list; none is "absent" | client.go, GetRunnerByName |
| Remove runner | `DELETE {svc}/_apis/distributedtask/pools/0/agents/{runnerId}` | **204** | client.go, RemoveRunner |

mayfly never calls the delete-scale-set endpoint (ADR 0040), and the fake need not implement it.

**Shapes.** Field names are camelCase as listed; ids are 64-bit integers in mayfly's types.

- *Scale set*: `id`, `name`, `runnerGroupId`, `runnerGroupName`, `labels` (each `{type, name}`;
  the reference client defaults to one label equal to the name with type `System`),
  `RunnerSetting` (capital R, carrying `disableUpdate`), `createdOn`, `runnerJitConfigUrl`,
  `statistics` [scaleset: types.go]. A zero `createdOn` (`0001-01-01T00:00:00Z`) must decode.
- *Session*: `sessionId` (a UUID), `ownerName`, `runnerScaleSet`, `messageQueueUrl`,
  `messageQueueAccessToken`, `statistics` [scaleset: types.go].
- *Statistics*: `totalAvailableJobs`, `totalAcquiredJobs`, `totalAssignedJobs`,
  `totalRunningJobs`, `totalRegisteredRunners`, `totalBusyRunners`, `totalIdleRunners`, all
  integers [scaleset: types.go].
- *Runner reference*: `id`, `name`, `runnerScaleSetId` [scaleset: types.go].
- *Error body*: `{"typeName": "...", "message": "..."}`; a `text/plain` error body also occurs;
  a body may start with a UTF-8 byte-order mark, which is trimmed before decoding; the response
  headers `ActivityId` and `X-GitHub-Request-Id` identify the request for support
  [scaleset: errors.go, responseErrorCause; common_client.go, bufferResponseBody].

Error classes that matter to mayfly (by `typeName`, independent of the status code
[scaleset: errors.go, responseErrorCause, wrapResponseErrorType]):

| Status | `typeName` contains | Meaning for mayfly |
|:--|:--|:--|
| 404 | `AgentNotFoundException` | the runner is gone: removal is already done |
| 409 | `AgentExistsException` | JIT name taken: look the runner up by name (section 6) |
| 409 | `JobStillRunningException` | removal refused, the runner is busy (section 7) |
| 400, 401, 404, 409 otherwise | — | definite refusal of this request |

Error text that mayfly logs or returns never includes a URL's query string or user info, nor a
response body that may hold a token (the reference client redacts likewise
[scaleset: errors.go, redactedURL]).

## 3. The session

### 3.1 Lifecycle

One scale set has at most one live session, held by one listener (plan §7.2). mayfly's session
states are plan §7.2's `Unconfigured → Ensured → Opening → Active ⇄ Backoff | Degraded → Closing
→ Closed`.

- **Create.** Answers the session id, the queue URL and token, the scale set, and the current
  statistics [scaleset: session_client.go, createMessageSession]. The reference listener refuses
  to start without a session id and statistics [scaleset: listener/listener.go, Run]. mayfly
  records the opening and its statistics as the session's initial demand (plan §7.2 step 5); it
  does not disguise them as a message with a made-up id, as the reference listener does with
  id −1.
- **Owner name.** A free string; mayfly sends the host identity and the pool, never a secret.
- **A second session.** The reference client has no handling for a conflict on create: any
  non-200 is an error and its listener exits [scaleset: session_client.go, doSessionRequest;
  listener/listener.go, Run]. That a second create while one session lives answers **409**, its
  `typeName`, and how long an abandoned session lives before a new create succeeds are
  [unverified: GH-00]. mayfly's assumption until then: 409 on create means "a session exists";
  mayfly deletes the session id it journaled (if any) and retries with backoff, and never
  steals a session it has no record of.
- **Refresh.** On 401 from poll, ack or acquire, the session is refreshed with a PATCH and the
  call retried once; a refresh is skipped when another caller already replaced the expired
  session, so concurrent expiries refresh once [scaleset: session_client.go, GetMessage,
  refreshMessageSession]. Whether a refresh returns a new `messageQueueUrl` is
  [unverified: GH-00]; mayfly uses whatever the refresh returns.
- **Delete.** On orderly shutdown, drain completion or re-open: DELETE answers 204
  [scaleset: session_client.go, deleteMessageSession]. Pause, drain and safe mode keep the
  session open (ADR 0029).

### 3.2 Persistence

mayfly journals the session id (never its token) when the session opens and its deletion when
it closes, so a daemon restarted after a crash knows which session to delete before creating a
new one. The reference client keeps the session only in memory and deletes it on close
[scaleset: session_client.go, Close].

### 3.3 Token expiry

A 401 on a queue call is the expiry signal [scaleset: session_client.go, getMessage,
deleteMessage, acquireJobs]. After one refresh and one retry, a second 401 is a session failure:
the session goes to `Backoff` and is re-opened, and a re-open recomputes demand from its initial
statistics (plan §7.2).

## 4. Messages

### 4.1 Long poll

`GET <messageQueueUrl>` with `Authorization: Bearer <queue token>`, `Accept: application/json;
api-version=6.0-preview` (here the version rides in `Accept`, not the query) and
`X-ScaleSetMaxCapacity: <n>` [scaleset: session_client.go, getMessage].

- **200** carries one message; **202** means the hold expired with nothing to deliver
  [scaleset: session_client.go, getMessage]. Whether a 202 carries a body or statistics is
  [unverified: GH-00]; mayfly ignores any.
- The service holds an empty poll for about 50 s; the reference client bounds each attempt at
  2 minutes and retries a stalled attempt with a fresh window [scaleset: http_client.go,
  MessagePollTimeout; README.md]. mayfly bounds each poll attempt the same way, and polls again at
  once after handling a response.
- `lastMessageId` is sent only when greater than zero [scaleset: session_client.go, getMessage].
  Passing the last processed id avoids reprocessing; omitting it returns the first available
  message [scaleset: README.md]. Whether it also acknowledges earlier messages implicitly is
  [unverified: GH-00]; mayfly always acks explicitly.

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
| `JobAvailable` | `acquireJobUrl` | an offer to classify and maybe acquire (plan §7.2 step 2) |
| `JobAssigned` | — | a fact; never counted for demand |
| `JobStarted` | `runnerId`, `runnerName` | a fact tying a job to a runner |
| `JobCompleted` | `result`, `runnerId`, `runnerName` | a fact; a cancel arrives as `result: "canceled"` |

The reference client skips unknown inner types silently and splits a batch into four lists by
type, losing the order between them [scaleset: client.go, parseRunnerScaleSetMessageResponse;
types.go]. mayfly keeps the batch in arrival order and records unknown types as protocol drift.
All strings are untrusted input: length-bounded and never interpolated (dm §9.1). Field presence,
null against absent, zero times, the full set of `result` values and `eventName` values come
from real payloads only [unverified: GH-00]; until then fixtures are synthetic and say so.

A job can be assigned, not acquired by a runner in time, completed as `canceled` and re-queued,
up to three times, each as new messages for the same workflow job [scaleset: README.md].

### 4.4 Ack by delete

`DELETE` of the message id under the queue URL acknowledges it; an unacknowledged message is
redelivered on the next poll [scaleset: session_client.go, deleteMessage; README.md]. The
reference listener acks after its in-memory handler returns, with no cancellation
[scaleset: listener/listener.go, Run]. A 404 on ack is reported as an error by the reference
client; whether it means "already acked" is [unverified: GH-00], and mayfly treats it as done
(the inbox record already exists).

## 5. Demand: statistics and capacity

- **Demand** is `statistics.totalAssignedJobs`, which counts jobs waiting for a runner and jobs
  running (`totalAssignedJobs >= totalRunningJobs`); never count job messages, since a response
  holds at most 50 and a backlog is truncated [scaleset: README.md]. mayfly's demand is
  `desired = min(deliverable, assigned)` (plan §7.2, ADR 0028).
- **Capacity** is the `X-ScaleSetMaxCapacity` header on every poll [scaleset: client.go,
  HeaderScaleSetMaxCapacity; session_client.go, getMessage]. It is the scale set's **total**
  capacity, not the free slots: the reference listener sends the same configured value on every
  poll [scaleset: listener/listener.go, Run], and the service uses it to avoid assigning more
  than the scale set can serve [scaleset: README.md]. Zero is valid [scaleset:
  listener/listener.go, Config.Validate].
- **Changing capacity.** The value may change between polls of one session: the reference
  listener exposes `SetMaxRunners`, documented as safe to call while it runs, which stores the
  value atomically and is read on the next poll [scaleset: listener/listener.go,
  Listener.SetMaxRunners, Run]. mayfly does the same with no blocking: each poll sends the
  scale set's current **reported max** (≤ its deliverable carve-out, I24), which pause, drain and
  safe mode change without closing the session (ADR 0029). How fast and how exactly the service
  reacts to a lowered value (does it cap `totalAssignedJobs`, does it un-assign), and the
  plain-text 400 "capacity" error, are [unverified: GH-00].

## 6. Acquire and JIT

**Acquire.** The listener's handler must acquire every `JobAvailable` it wants, or the job stays
unassigned [scaleset: listener/listener.go, Scaler]. The body is a JSON array of
`runnerRequestId`s; the answer's `value` lists the ids acquired [scaleset: session_client.go,
acquireJobs]. The reference example calls re-acquiring an acquired job a no-op
[scaleset: examples/dockerscaleset/scaler.go, Scale]. Whether a partial answer omits ids that
are no longer available or fails the request, and whether a repeat really is a no-op on the
service, are [unverified: GH-00]. mayfly journals the acquire plan before the call (I23, I32) and
the returned set as an observed fact; an id not returned is not acquired.

**JIT config.** `generatejitconfig` takes a runner name (mayfly: the VM name, issued once per
registration, I19) and a work folder, and answers the runner reference (its id is journaled) and
`encodedJITConfig`, a secret [scaleset: client.go, GenerateJitRunnerConfig; types.go]. A taken
name answers 409 `AgentExistsException` [scaleset: errors.go, responseErrorCause]; ARC then
looks the runner up by name and removes it only when it belongs to its own scale set
[ARC: controllers/actions.github.com/ephemeralrunner_controller.go]. mayfly instead removes only
a runner whose id it journaled (ADR 0029) and otherwise reports the collision.

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

**Confirmation.** After delivery the control plane confirms through the runner endpoints that
the issued runner id came online (ADR 0038). How long a JIT runner that never connects stays
registered is [unverified: GH-00]; ARC states that the service drops such a registration once the
runner stops reporting in [ARC: controllers/actions.github.com/runner_unregistration.go], and
that an ephemeral runner which ran a job is deregistered by the service when it finishes.

## 7. Runner removal

`DELETE {svc}/_apis/distributedtask/pools/0/agents/{runnerId}` answers 204 [scaleset:
client.go, RemoveRunner]. A busy runner answers 409 `JobStillRunningException`; a gone one 404
`AgentNotFoundException` [scaleset: errors.go, responseErrorCause]. ARC retries a busy runner
after 30 s and treats not-found as removed [ARC: controllers/actions.github.com/
runner_unregistration.go]. mayfly: not-found completes the retirement; busy cancels it (plan
§7.3, I21); the reaper removes only journaled ids (ADR 0029). The REST endpoint for the same
operation (`DELETE /orgs/{org}/actions/runners/{id}`) documents 422 for a busy runner [docs];
mayfly does not use it for its scale set's runners. The exact bodies of both answers are
[unverified: GH-00].

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
- GitHub's REST limits for an installation and the advice to wait at least a minute on a
  secondary limit without `Retry-After` are [docs]. Whether the Actions service and the queue
  host send `X-RateLimit-*` at all is [unverified: GH-00].

mayfly adds what the reference leaves to its caller: a request that runs out of retries moves
the session to `Backoff` (never closes it on a transient error), repeated failure to `Degraded`;
the reaper lists runners at most once an hour (ADR 0029); every request has a deadline, and a
wait that would outlast it is not taken.

## 9. Where mayfly differs from the reference listener

| Reference behaviour | mayfly | Why |
|:--|:--|:--|
| Acks after an in-memory handler returns [scaleset: listener/listener.go, Run] | Acks **only after the inbox record commits**: inbox key, statistics, job facts, declined offers and desired count in one store transaction (plan §7.2 steps 3-4); a refused command acks once its refusal receipt commits (GH-04, ADR 0041) | A crash between handling and ack must lose no fact; a redelivery is a `DuplicateMessage`, acked without a write |
| The handler may do VM and JIT work inline, blocking the poll [scaleset: examples/dockerscaleset/scaler.go] | The poll loop records and acks only; the domain decides; VM work and JIT minting are directives and effects run elsewhere; no blocking call inside the domain (`decide`/`evolve` are pure) | A slow clone must not starve the 50 s poll or delay acks into redelivery storms |
| `lastMessageId` kept in memory only [scaleset: listener/listener.go, Run] | The last acked message id and every inbox key are journaled; after a restart the listener resumes from the journal | No reprocessing after a restart; a redelivered id is recognised |
| Any error ends the listener, and its process exits [scaleset: listener/listener.go, Run] | Errors are classified: token expiry is handled in the client; transient errors back off and keep the session; a lost session re-opens; protocol drift degrades the session, never safe mode | The daemon survives service hiccups |
| Session created in the client's constructor, deleted on close [scaleset: client.go, MessageSessionClient] | Opening and closing a session are commands with journaled outcomes; on start the journaled stale session is deleted before a new one is created (section 3.2) | Crash recovery without a 409 loop |
| Capacity is a configured number [scaleset: listener/listener.go] | Capacity is the scale set's reported max, changing with the host's capacity and the pool's mode (section 5) | ADR 0024, ADR 0029 |
| A completed job removes its runner's container | `JobCompleted` is an observation; a VM is deleted only with the ownership proof (plan §8.4) | Ownership rule |
| Initial statistics passed as a message with id −1 | Recorded as the session-open fact | Message ids mean service ids only |

**Listener crash recovery.** If the daemon dies between commit and ack, the message is
redelivered and recognised by its inbox key, acked without a write. If it dies before commit,
nothing was recorded and the redelivery is handled afresh. If it dies holding a session, the next
start deletes the journaled session, opens a new one, and recomputes demand from that session's
statistics (plan §7.2 step 5). Acquires are journaled before the call, so a redelivered offer is
not re-planned past what was already planned (I23).

## 10. The fake and the contract

`mayfly-fake-github` (GH-01) implements this document, independently of any production crate
(ADR 0012) and of the reference implementation's tests (ADR 0048): the auth chain (with
short-lived admin and queue tokens to exercise refresh), scale-set lookup and create, sessions
with one live session per scale set, the long poll with a 202 hold, redelivery until delete, the
50-message cap, statistics, acquire, JIT, runner get/remove, and fault knobs (401, 403, 409, 422
on the REST path, 429 with `Retry-After`, a response dropped after its effect, a stalled poll).
Every behaviour marked [unverified: GH-00] is a knob or a named assumption until GH-00's capture
replaces it. GH-03's recorded, redacted fixtures (`tests/fixtures/github-scaleset/`) are the
contract; the same suite runs against the fake and, owner-gated, against a live disposable
organisation (ADR 0015, HW-02).

## 11. Attribution

This specification was informed by reading GitHub's `actions/scaleset` (MIT, Copyright GitHub,
Inc.) at commit `716908e`, and `actions/runner` (MIT) at `310a370`; ARC (Apache-2.0) at
`89f34c4` was read for behaviour only. No code, tests, test vectors or recorded payloads were
copied or translated, so no third-party notice is required today. Should any fragment ever be
copied, ADR 0048's rule applies: it ships with the upstream licence notice in
`THIRD-PARTY-NOTICES` and is listed there.

## 12. Open questions for GH-00

The owner's live capture in a disposable organisation answers each; the answer replaces the
[unverified: GH-00] mark here and becomes a fixture.

1. **Message ids**: where they start (0 or 1), whether they are monotonic and gap-free, whether
   they are per session or per scale set, and whether `lastMessageId` acknowledges implicitly.
2. **Second session**: status and body (`typeName`) of a create while a session lives; how long
   an abandoned session blocks a new one.
3. **Acquire**: the answer for a mix of available, already acquired and expired ids (subset,
   error or both); whether re-acquiring an acquired id is a no-op.
4. **Token lifetimes**: admin token, message-queue token, session; whether a session refresh
   returns a new queue URL.
5. **Real job payloads**: all four job message types and the full envelope with statistics;
   field presence, nulls, zero times, `result` and `eventName` values, `requestLabels`,
   `acquireJobUrl`; offer fields usable for trust classification (ADR 0027).
6. **Capacity header**: whether `X-ScaleSetMaxCapacity` caps assignment and `totalAssignedJobs`;
   what lowering it mid-session does; when a plain-text 400 "capacity" error appears.
7. **Rate limits**: whether the Actions service and the queue host send `Retry-After` or
   `X-RateLimit-*`, and what a throttled poll looks like.
8. **Unconnected JIT runner**: how long its registration lives, and whether a job assigned to it
   is re-queued.
9. **Removal errors**: the bodies of 409 `JobStillRunningException` and 404
   `AgentNotFoundException`; the REST path's 422 body.
10. **The 202**: hold duration and whether it carries a body or statistics.
11. **Ack**: the answer to deleting an already deleted message id.
12. **Scale set create**: duplicate-name answer; presence of `runnerJitConfigUrl` and
    `statistics`; the default runner group's id.
13. **Permissions**: the minimal App permission list that makes the whole chain work, with the
    reason for each (ADR 0045).
