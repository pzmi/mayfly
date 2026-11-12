//! The cases of the wire contract, each citing the spec section it checks.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rsa::PrivateKey;
use rsa::pkcs1::DecodeRsaPrivateKey;
use rsa::sha2::Sha256;
use serde_json::{Value, json};

use super::{JobScript, WireRequest, WireResponse, WireWorld};
use crate::knobs::Flow;

/// A case of the suite.
#[derive(Debug, Clone, Copy)]
pub struct Case {
    /// `WC-nn`.
    pub id: &'static str,
    /// The flow its world serves.
    pub flow: Flow,
    /// Whether it needs a [`JobScript`].
    pub needs_script: bool,
    /// What it checks.
    pub what: &'static str,
}

const fn case(id: &'static str, flow: Flow, needs_script: bool, what: &'static str) -> Case {
    Case {
        id,
        flow,
        needs_script,
        what,
    }
}

/// Every case, in the order they run.
pub const CASES: &[Case] = &[
    case(
        "WC-01",
        Flow::Direct,
        false,
        "the auth chain yields a service URL and an admin token with exp (§1)",
    ),
    case(
        "WC-02",
        Flow::Direct,
        false,
        "scale set lookup by name, create, lookup by id; seven statistics (§2)",
    ),
    case(
        "WC-03",
        Flow::Direct,
        false,
        "one live session per scale set: 409 SessionConflict, delete 204 frees it (§3.1)",
    ),
    case(
        "WC-04",
        Flow::Direct,
        true,
        "refresh mints a working token; an expired session's refresh is 400 SessionExpired (§3.1)",
    ),
    case(
        "WC-05",
        Flow::Direct,
        false,
        "a poll with nothing to deliver answers 202 (§4.1)",
    ),
    case(
        "WC-06",
        Flow::Direct,
        true,
        "a message is redelivered until its ack, then not (§4.4)",
    ),
    case(
        "WC-07",
        Flow::Direct,
        true,
        "a message holds at most 50 job messages; the backlog follows (§5)",
    ),
    case(
        "WC-08",
        Flow::Direct,
        true,
        "capacity 0 assigns nothing; capacity 1 assigns (§5)",
    ),
    case(
        "WC-09",
        Flow::Direct,
        true,
        "JobAssigned on github.com has runnerRequestId 0 and the classifying fields (§4.3)",
    ),
    case(
        "WC-10",
        Flow::Direct,
        false,
        "JIT config minted; a taken name is 409 AgentExists (§6.3)",
    ),
    case(
        "WC-11",
        Flow::Direct,
        false,
        "runner get, list by name, remove 204, removal of an absent runner gone (§7)",
    ),
    case(
        "WC-12",
        Flow::Direct,
        true,
        "a busy runner's removal is refused with JobStillRunning (§7)",
    ),
    case(
        "WC-13",
        Flow::Direct,
        true,
        "an expired queue token answers 401 and a refresh restores the poll (§3.3)",
    ),
    case(
        "WC-14",
        Flow::Direct,
        true,
        "the workflow run of an assigned job is looked up; cancel 202 then 409 (§6.2)",
    ),
    case(
        "WC-15",
        Flow::Acquire,
        true,
        "GHES offers: JobAvailable, acquirablejobs, partial acquire, JobAssigned (§6.1)",
    ),
];

type Outcome = Result<(), String>;

fn ensure(ok: bool, what: impl FnOnce() -> String) -> Outcome {
    if ok { Ok(()) } else { Err(what()) }
}

fn expect_status(response: &WireResponse, status: u16, what: &str) -> Outcome {
    ensure(response.status == status, || {
        format!(
            "{what}: expected {status}, got {} ({})",
            response.status,
            String::from_utf8_lossy(&response.body)
                .chars()
                .take(200)
                .collect::<String>()
        )
    })
}

fn expect_type(response: &WireResponse, class: &str, what: &str) -> Outcome {
    let name = response.type_name();
    ensure(name.contains(class), || {
        format!("{what}: expected a typeName containing {class}, got {name:?}")
    })
}

const STATISTICS: [&str; 7] = [
    "totalAvailableJobs",
    "totalAcquiredJobs",
    "totalAssignedJobs",
    "totalRunningJobs",
    "totalRegisteredRunners",
    "totalBusyRunners",
    "totalIdleRunners",
];

fn expect_statistics(value: &Value, what: &str) -> Outcome {
    for field in STATISTICS {
        ensure(value[field].is_u64(), || {
            format!("{what}: statistics.{field} missing or not an integer")
        })?;
    }
    Ok(())
}

/// A client of the suite's own, holding the admin connection.
struct Client<'a, W> {
    world: &'a W,
    service: String,
    admin: String,
}

/// A session as the suite holds it.
struct Session {
    scale_set: u64,
    id: String,
    queue_url: String,
    token: String,
}

fn request(
    method: &str,
    url: String,
    credential: Option<String>,
    body: Option<Value>,
) -> WireRequest {
    let mut headers = vec![("accept".to_owned(), "application/json".to_owned())];
    if let Some(credential) = credential {
        headers.push(("authorization".to_owned(), credential));
    }
    if body.is_some() {
        headers.push(("content-type".to_owned(), "application/json".to_owned()));
    }
    WireRequest {
        method: method.to_owned(),
        url,
        headers,
        body: body.map(|b| b.to_string().into_bytes()).unwrap_or_default(),
    }
}

fn app_jwt<W: WireWorld>(world: &W) -> Result<String, String> {
    let setup = world.setup();
    let now = (setup.now_secs)();
    let key = PrivateKey::from_pkcs1_pem(&setup.app_private_pem).map_err(|e| e.to_string())?;
    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256","typ":"JWT"}"#);
    let claims = URL_SAFE_NO_PAD.encode(
        json!({"iat": now - 60, "exp": now + 540, "iss": setup.app_client_id})
            .to_string()
            .as_bytes(),
    );
    let signed = format!("{header}.{claims}");
    let signature = key
        .pkcs1v15_sign::<Sha256>(signed.as_bytes())
        .map_err(|e| e.to_string())?;
    Ok(format!("{signed}.{}", URL_SAFE_NO_PAD.encode(signature)))
}

/// The installation token (spec §1 step 2).
async fn installation_token<W: WireWorld>(world: &W) -> Result<String, String> {
    let setup = world.setup();
    let jwt = app_jwt(world)?;
    let answer = world
        .send(request(
            "POST",
            format!(
                "{}/app/installations/{}/access_tokens",
                setup.api_base, setup.installation_id
            ),
            Some(format!("Bearer {jwt}")),
            None,
        ))
        .await?;
    expect_status(&answer, 201, "installation token")?;
    answer.json()["token"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| "installation token: no token".to_owned())
}

fn exp_claim(token: &str) -> Option<u64> {
    let claims = token.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(claims).ok()?;
    serde_json::from_slice::<Value>(&bytes).ok()?["exp"].as_u64()
}

impl<'a, W: WireWorld> Client<'a, W> {
    /// Runs the auth chain to the admin connection (spec §1 steps 1–4).
    async fn connect(world: &'a W) -> Result<Self, String> {
        let setup = world.setup();
        let installation = installation_token(world).await?;
        let answer = world
            .send(request(
                "POST",
                format!("{}{}", setup.api_base, setup.registration_path),
                Some(format!("Bearer {installation}")),
                None,
            ))
            .await?;
        expect_status(&answer, 201, "registration token")?;
        let registration = answer.json()["token"]
            .as_str()
            .ok_or("registration token: no token")?
            .to_owned();
        let exchange = || {
            world.send(request(
                "POST",
                format!("{}/actions/runner-registration", setup.api_base),
                Some(format!("RemoteAuth {registration}")),
                Some(json!({"url": setup.config_url, "runner_event": "register"})),
            ))
        };
        let mut answer = exchange().await?;
        // A fresh registration token is not always usable at once: the exchange is retried
        // once on 401 or 403 (spec §1 step 4).
        if matches!(answer.status, 401 | 403) {
            answer = exchange().await?;
        }
        ensure((200..300).contains(&answer.status), || {
            format!("runner registration: status {}", answer.status)
        })?;
        let body = answer.json();
        let service = body["url"].as_str().ok_or("runner registration: no url")?;
        let admin = body["token"]
            .as_str()
            .ok_or("runner registration: no token")?;
        ensure(exp_claim(admin).is_some(), || {
            "runner registration: the admin token has no exp claim".to_owned()
        })?;
        Ok(Self {
            world,
            service: service.trim_end_matches('/').to_owned(),
            admin: admin.to_owned(),
        })
    }

    async fn svc(
        &self,
        method: &str,
        path: &str,
        body: Option<Value>,
    ) -> Result<WireResponse, String> {
        let separator = if path.contains('?') { '&' } else { '?' };
        let url = format!(
            "{}/_apis/{path}{separator}api-version=6.0-preview",
            self.service
        );
        self.world
            .send(request(
                method,
                url,
                Some(format!("Bearer {}", self.admin)),
                body,
            ))
            .await
    }

    async fn ensure_scale_set(&self, name: &str) -> Result<u64, String> {
        let setup = self.world.setup();
        let groups = self
            .svc(
                "GET",
                &format!("runtime/runnergroups/?groupName={}", setup.runner_group),
                None,
            )
            .await?;
        expect_status(&groups, 200, "runner group by name")?;
        let group = groups.json()["value"][0]["id"]
            .as_u64()
            .ok_or("runner group by name: none")?;
        let found = self
            .svc(
                "GET",
                &format!("runtime/runnerscalesets?runnerGroupId={group}&name={name}"),
                None,
            )
            .await?;
        expect_status(&found, 200, "scale set by name")?;
        if let Some(id) = found.json()["value"][0]["id"].as_u64() {
            return Ok(id);
        }
        let created = self
            .svc(
                "POST",
                "runtime/runnerscalesets",
                Some(json!({
                    "name": name,
                    "runnerGroupId": group,
                    "labels": [{"type": "System", "name": name}],
                    "RunnerSetting": {"disableUpdate": true},
                })),
            )
            .await?;
        expect_status(&created, 200, "create scale set")?;
        created.json()["id"]
            .as_u64()
            .ok_or_else(|| "create scale set: no id".to_owned())
    }

    async fn open(&self, scale_set: u64) -> Result<Result<Session, WireResponse>, String> {
        let answer = self
            .svc(
                "POST",
                &format!("runtime/runnerscalesets/{scale_set}/sessions"),
                Some(json!({"ownerName": "mayfly-contract"})),
            )
            .await?;
        if answer.status != 200 {
            return Ok(Err(answer));
        }
        Ok(Ok(session_from(scale_set, &answer.json())?))
    }

    async fn open_ok(&self, scale_set: u64) -> Result<Session, String> {
        match self.open(scale_set).await? {
            Ok(session) => Ok(session),
            Err(answer) => Err(format!(
                "create session: expected 200, got {}",
                answer.status
            )),
        }
    }

    async fn refresh(&self, session: &Session) -> Result<WireResponse, String> {
        self.svc(
            "PATCH",
            &format!(
                "runtime/runnerscalesets/{}/sessions/{}",
                session.scale_set, session.id
            ),
            None,
        )
        .await
    }

    async fn close(&self, session: &Session) -> Result<WireResponse, String> {
        self.svc(
            "DELETE",
            &format!(
                "runtime/runnerscalesets/{}/sessions/{}",
                session.scale_set, session.id
            ),
            None,
        )
        .await
    }

    async fn poll(
        &self,
        session: &Session,
        capacity: u64,
        last: Option<u64>,
    ) -> Result<WireResponse, String> {
        let url = match last.filter(|l| *l > 0) {
            Some(last) => format!("{}?lastMessageId={last}", session.queue_url),
            None => session.queue_url.clone(),
        };
        let mut req = request("GET", url, Some(format!("Bearer {}", session.token)), None);
        req.headers.retain(|(n, _)| n != "accept");
        req.headers.push((
            "accept".to_owned(),
            "application/json; api-version=6.0-preview".to_owned(),
        ));
        req.headers
            .push(("x-scalesetmaxcapacity".to_owned(), capacity.to_string()));
        self.world.send(req).await
    }

    async fn ack(&self, session: &Session, message: u64) -> Result<WireResponse, String> {
        self.world
            .send(request(
                "DELETE",
                format!("{}/{message}", session.queue_url.trim_end_matches('/')),
                Some(format!("Bearer {}", session.token)),
                None,
            ))
            .await
    }

    async fn jit(&self, scale_set: u64, name: &str) -> Result<WireResponse, String> {
        self.svc(
            "POST",
            &format!("runtime/runnerscalesets/{scale_set}/generatejitconfig"),
            Some(json!({"name": name, "workFolder": "_work"})),
        )
        .await
    }
}

fn session_from(scale_set: u64, body: &Value) -> Result<Session, String> {
    let field = |name: &str| {
        body[name]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| format!("session: no {name}"))
    };
    expect_statistics(&body["statistics"], "session")?;
    Ok(Session {
        scale_set,
        id: field("sessionId")?,
        queue_url: field("messageQueueUrl")?,
        token: field("messageQueueAccessToken")?,
    })
}

/// The id and job messages of a 200 poll.
fn message(answer: &WireResponse) -> Result<(u64, Vec<Value>, Value), String> {
    expect_status(answer, 200, "poll")?;
    let body = answer.json();
    let id = body["messageId"].as_u64().ok_or("poll: no messageId")?;
    ensure(body["messageType"] == "RunnerScaleSetJobMessages", || {
        format!("poll: messageType {}", body["messageType"])
    })?;
    let inner = body["body"].as_str().ok_or("poll: body is not a string")?;
    let jobs: Vec<Value> = if inner.is_empty() {
        Vec::new()
    } else {
        serde_json::from_str(inner).map_err(|e| format!("poll: body is not a JSON array: {e}"))?
    };
    expect_statistics(&body["statistics"], "poll")?;
    Ok((id, jobs, body["statistics"].clone()))
}

fn script<W: WireWorld>(world: &W) -> Result<&W::Script, String> {
    world.script().ok_or_else(|| "no job script".to_owned())
}

/// Runs case `id` on `world`.
pub async fn run<W: WireWorld>(id: &str, world: &W) -> Outcome {
    let name = format!("mf-{}-{}", world.setup().unique, id.to_ascii_lowercase());
    match id {
        "WC-01" => Client::connect(world).await.map(|_| ()),
        "WC-02" => wc02(world, &name).await,
        "WC-03" => wc03(world, &name).await,
        "WC-04" => wc04(world, &name).await,
        "WC-05" => wc05(world, &name).await,
        "WC-06" => wc06(world, &name).await,
        "WC-07" => wc07(world, &name).await,
        "WC-08" => wc08(world, &name).await,
        "WC-09" => wc09(world, &name).await,
        "WC-10" => wc10(world, &name).await,
        "WC-11" => wc11(world, &name).await,
        "WC-12" => wc12(world, &name).await,
        "WC-13" => wc13(world, &name).await,
        "WC-14" => wc14(world, &name).await,
        "WC-15" => wc15(world, &name).await,
        other => Err(format!("unknown case {other}")),
    }
}

async fn wc02<W: WireWorld>(world: &W, name: &str) -> Outcome {
    let client = Client::connect(world).await?;
    let id = client.ensure_scale_set(name).await?;
    let again = client.ensure_scale_set(name).await?;
    ensure(id == again, || {
        format!("lookup by name found {again}, created {id}")
    })?;
    let by_id = client
        .svc("GET", &format!("runtime/runnerscalesets/{id}"), None)
        .await?;
    expect_status(&by_id, 200, "scale set by id")?;
    let body = by_id.json();
    ensure(body["name"] == name, || {
        format!("scale set by id: name {}", body["name"])
    })?;
    expect_statistics(&body["statistics"], "scale set")
}

async fn wc03<W: WireWorld>(world: &W, name: &str) -> Outcome {
    let client = Client::connect(world).await?;
    let set = client.ensure_scale_set(name).await?;
    let first = client.open_ok(set).await?;
    let second = match client.open(set).await? {
        Ok(_) => return Err("a second session opened beside a live one".to_owned()),
        Err(answer) => answer,
    };
    expect_status(&second, 409, "second session")?;
    expect_type(
        &second,
        "RunnerScaleSetSessionConflictException",
        "second session",
    )?;
    expect_status(&client.close(&first).await?, 204, "delete session")?;
    let third = client.open_ok(set).await?;
    expect_status(&client.close(&third).await?, 204, "delete session")
}

async fn wc04<W: WireWorld>(world: &W, name: &str) -> Outcome {
    let client = Client::connect(world).await?;
    let set = client.ensure_scale_set(name).await?;
    let session = client.open_ok(set).await?;
    let refreshed = client.refresh(&session).await?;
    expect_status(&refreshed, 200, "refresh")?;
    let renewed = session_from(set, &refreshed.json())?;
    expect_status(
        &client.poll(&renewed, 0, None).await?,
        202,
        "poll after refresh",
    )?;
    script(world)?.expire_session(set).await?;
    let late = client.refresh(&renewed).await?;
    expect_status(&late, 400, "refresh of an expired session")?;
    expect_type(
        &late,
        "RunnerScaleSetSessionExpiredException",
        "refresh of an expired session",
    )?;
    let next = client.open_ok(set).await?;
    expect_status(&client.close(&next).await?, 204, "delete session")
}

async fn wc05<W: WireWorld>(world: &W, name: &str) -> Outcome {
    let client = Client::connect(world).await?;
    let set = client.ensure_scale_set(name).await?;
    let session = client.open_ok(set).await?;
    expect_status(&client.poll(&session, 0, None).await?, 202, "empty poll")?;
    expect_status(&client.close(&session).await?, 204, "delete session")
}

/// Polls with `capacity` until a message arrives, at most `tries` holds.
async fn next_message<W: WireWorld>(
    client: &Client<'_, W>,
    session: &Session,
    capacity: u64,
    last: Option<u64>,
) -> Result<(u64, Vec<Value>, Value), String> {
    for _ in 0..3 {
        let answer = client.poll(session, capacity, last).await?;
        if answer.status == 202 {
            continue;
        }
        return message(&answer);
    }
    Err("no message after three holds".to_owned())
}

async fn wc06<W: WireWorld>(world: &W, name: &str) -> Outcome {
    let client = Client::connect(world).await?;
    let set = client.ensure_scale_set(name).await?;
    let session = client.open_ok(set).await?;
    script(world)?.queue_jobs(1, name, 9001, "push", "").await?;
    let (first, jobs, _) = next_message(&client, &session, 1, None).await?;
    ensure(!jobs.is_empty(), || {
        "the first message holds no job message".to_owned()
    })?;
    let (again, _, _) = message(&client.poll(&session, 1, None).await?)?;
    ensure(again == first, || {
        format!("redelivered {again}, expected {first}")
    })?;
    expect_status(&client.ack(&session, first).await?, 204, "ack")?;
    let after = client.poll(&session, 1, Some(first)).await?;
    if after.status == 200 {
        let (id, _, _) = message(&after)?;
        ensure(id != first, || {
            format!("message {first} came back after its ack")
        })?;
    }
    let after = client.poll(&session, 1, None).await?;
    if after.status == 200 {
        let (id, _, _) = message(&after)?;
        ensure(id != first, || {
            format!("message {first} came back after its ack")
        })?;
    }
    expect_status(&client.close(&session).await?, 204, "delete session")
}

fn count(jobs: &[Value], kind: &str) -> usize {
    jobs.iter().filter(|j| j["messageType"] == kind).count()
}

async fn wc07<W: WireWorld>(world: &W, name: &str) -> Outcome {
    let client = Client::connect(world).await?;
    let set = client.ensure_scale_set(name).await?;
    let session = client.open_ok(set).await?;
    script(world)?
        .queue_jobs(60, name, 9002, "push", "")
        .await?;
    let mut assigned = 0;
    let mut last = None;
    for _ in 0..6 {
        let answer = client.poll(&session, 100, last).await?;
        if answer.status == 202 {
            break;
        }
        let (id, jobs, statistics) = message(&answer)?;
        ensure(jobs.len() <= 50, || {
            format!("a message held {} job messages", jobs.len())
        })?;
        assigned += count(&jobs, "JobAssigned");
        // Settled whichever moment the statistics are taken at (A6): they count at least the
        // assignments delivered so far.
        let counted = statistics["totalAssignedJobs"].as_u64().unwrap_or_default();
        ensure(
            counted >= u64::try_from(assigned).unwrap_or(u64::MAX),
            || format!("totalAssignedJobs {counted} with {assigned} assignments delivered"),
        )?;
        expect_status(&client.ack(&session, id).await?, 204, "ack")?;
        last = Some(id);
        if assigned >= 60 {
            break;
        }
    }
    ensure(assigned == 60, || {
        format!("{assigned} of 60 assignments delivered")
    })?;
    expect_status(&client.close(&session).await?, 204, "delete session")
}

async fn wc08<W: WireWorld>(world: &W, name: &str) -> Outcome {
    let client = Client::connect(world).await?;
    let set = client.ensure_scale_set(name).await?;
    let session = client.open_ok(set).await?;
    script(world)?.queue_jobs(1, name, 9003, "push", "").await?;
    let held = client.poll(&session, 0, None).await?;
    expect_status(&held, 202, "poll at capacity 0")?;
    let (_, jobs, statistics) = next_message(&client, &session, 1, None).await?;
    ensure(count(&jobs, "JobAssigned") == 1, || {
        "capacity 1 assigned nothing".to_owned()
    })?;
    ensure(statistics["totalAssignedJobs"] == 1, || {
        format!("totalAssignedJobs {}", statistics["totalAssignedJobs"])
    })?;
    expect_status(&client.close(&session).await?, 204, "delete session")
}

async fn wc09<W: WireWorld>(world: &W, name: &str) -> Outcome {
    let client = Client::connect(world).await?;
    let set = client.ensure_scale_set(name).await?;
    let session = client.open_ok(set).await?;
    script(world)?.queue_jobs(1, name, 9004, "push", "").await?;
    let (_, jobs, _) = next_message(&client, &session, 1, None).await?;
    let job = jobs
        .iter()
        .find(|j| j["messageType"] == "JobAssigned")
        .ok_or("no JobAssigned")?;
    ensure(job.get("runnerRequestId") == Some(&json!(0)), || {
        format!(
            "runnerRequestId {:?}, expected an explicit 0",
            job.get("runnerRequestId")
        )
    })?;
    for field in [
        "repositoryName",
        "ownerName",
        "jobId",
        "workflowRunId",
        "eventName",
        "requestLabels",
    ] {
        ensure(job.get(field).is_some_and(|v| !v.is_null()), || {
            format!("JobAssigned has no {field}")
        })?;
    }
    ensure(job["workflowRunId"] == 9004, || {
        format!("workflowRunId {}", job["workflowRunId"])
    })?;
    expect_status(&client.close(&session).await?, 204, "delete session")
}

async fn wc10<W: WireWorld>(world: &W, name: &str) -> Outcome {
    let client = Client::connect(world).await?;
    let set = client.ensure_scale_set(name).await?;
    let runner = format!("{name}-runner");
    let minted = client.jit(set, &runner).await?;
    expect_status(&minted, 200, "generate JIT config")?;
    let body = minted.json();
    ensure(body["runner"]["id"].as_u64().is_some(), || {
        "JIT: no runner id".to_owned()
    })?;
    ensure(body["runner"]["name"] == runner.as_str(), || {
        "JIT: runner name".to_owned()
    })?;
    ensure(
        body["encodedJITConfig"]
            .as_str()
            .is_some_and(|c| !c.is_empty()),
        || "JIT: no encodedJITConfig".to_owned(),
    )?;
    let taken = client.jit(set, &runner).await?;
    expect_status(&taken, 409, "JIT with a taken name")?;
    expect_type(&taken, "AgentExistsException", "JIT with a taken name")?;
    let id = body["runner"]["id"].as_u64().unwrap_or_default();
    expect_status(
        &client
            .svc(
                "DELETE",
                &format!("distributedtask/pools/0/agents/{id}"),
                None,
            )
            .await?,
        204,
        "remove runner",
    )
}

async fn wc11<W: WireWorld>(world: &W, name: &str) -> Outcome {
    let client = Client::connect(world).await?;
    let set = client.ensure_scale_set(name).await?;
    let runner = format!("{name}-runner");
    let minted = client.jit(set, &runner).await?;
    expect_status(&minted, 200, "generate JIT config")?;
    let id = minted.json()["runner"]["id"]
        .as_u64()
        .ok_or("JIT: no runner id")?;
    let got = client
        .svc("GET", &format!("distributedtask/pools/0/agents/{id}"), None)
        .await?;
    expect_status(&got, 200, "runner by id")?;
    ensure(got.json()["name"] == runner.as_str(), || {
        "runner by id: name".to_owned()
    })?;
    let listed = client
        .svc(
            "GET",
            &format!("distributedtask/pools/0/agents?agentName={runner}"),
            None,
        )
        .await?;
    expect_status(&listed, 200, "runner by name")?;
    ensure(listed.json()["value"][0]["id"] == id, || {
        "runner by name: id".to_owned()
    })?;
    let path = format!("distributedtask/pools/0/agents/{id}");
    expect_status(
        &client.svc("DELETE", &path, None).await?,
        204,
        "remove runner",
    )?;
    let again = client.svc("DELETE", &path, None).await?;
    ensure(
        again.status == 204
            || (again.status == 404 && again.type_name().contains("AgentNotFoundException")),
        || format!("removing an absent runner: {}", again.status),
    )?;
    let gone = client.svc("GET", &path, None).await?;
    expect_status(&gone, 404, "a removed runner by id")?;
    let listed = client
        .svc(
            "GET",
            &format!("distributedtask/pools/0/agents?agentName={runner}"),
            None,
        )
        .await?;
    ensure(listed.json()["count"] == 0, || {
        "a removed runner is still listed".to_owned()
    })
}

async fn wc12<W: WireWorld>(world: &W, name: &str) -> Outcome {
    let client = Client::connect(world).await?;
    let set = client.ensure_scale_set(name).await?;
    let session = client.open_ok(set).await?;
    script(world)?.queue_jobs(1, name, 9005, "push", "").await?;
    let (_, jobs, _) = next_message(&client, &session, 1, None).await?;
    let job = jobs
        .iter()
        .find(|j| j["messageType"] == "JobAssigned")
        .and_then(|j| j["jobId"].as_str())
        .ok_or("no JobAssigned with a jobId")?
        .to_owned();
    let minted = client.jit(set, &format!("{name}-runner")).await?;
    expect_status(&minted, 200, "generate JIT config")?;
    let runner = minted.json()["runner"]["id"]
        .as_u64()
        .ok_or("JIT: no runner id")?;
    script(world)?.start_job(&job, runner).await?;
    let refused = client
        .svc(
            "DELETE",
            &format!("distributedtask/pools/0/agents/{runner}"),
            None,
        )
        .await?;
    ensure(refused.status == 400 || refused.status == 409, || {
        format!("removing a busy runner: status {}", refused.status)
    })?;
    expect_type(
        &refused,
        "JobStillRunningException",
        "removing a busy runner",
    )?;
    expect_status(&client.close(&session).await?, 204, "delete session")
}

async fn wc13<W: WireWorld>(world: &W, name: &str) -> Outcome {
    let client = Client::connect(world).await?;
    let set = client.ensure_scale_set(name).await?;
    let session = client.open_ok(set).await?;
    script(world)?.expire_queue_token(set).await?;
    expect_status(
        &client.poll(&session, 0, None).await?,
        401,
        "poll on an expired token",
    )?;
    let refreshed = client.refresh(&session).await?;
    expect_status(&refreshed, 200, "refresh")?;
    let renewed = session_from(set, &refreshed.json())?;
    expect_status(
        &client.poll(&renewed, 0, None).await?,
        202,
        "poll after refresh",
    )?;
    expect_status(&client.close(&renewed).await?, 204, "delete session")
}

async fn wc14<W: WireWorld>(world: &W, name: &str) -> Outcome {
    let setup = world.setup();
    let client = Client::connect(world).await?;
    let set = client.ensure_scale_set(name).await?;
    let session = client.open_ok(set).await?;
    let fork = format!("{}-fork/{}", setup.owner, setup.repository);
    script(world)?
        .queue_jobs(1, name, 9006, "pull_request", &fork)
        .await?;
    let (_, jobs, _) = next_message(&client, &session, 1, None).await?;
    let job = jobs
        .iter()
        .find(|j| j["messageType"] == "JobAssigned")
        .ok_or("no JobAssigned")?;
    let (owner, repo, run) = (
        job["ownerName"].as_str().ok_or("no ownerName")?,
        job["repositoryName"].as_str().ok_or("no repositoryName")?,
        job["workflowRunId"].as_u64().ok_or("no workflowRunId")?,
    );
    let installation = installation_token(world).await?;
    let runs = format!("{}/repos/{owner}/{repo}/actions/runs/{run}", setup.api_base);
    let looked = world
        .send(request(
            "GET",
            runs.clone(),
            Some(format!("Bearer {installation}")),
            None,
        ))
        .await?;
    expect_status(&looked, 200, "workflow run lookup")?;
    let body = looked.json();
    ensure(body["event"] == "pull_request", || {
        format!("run event {}", body["event"])
    })?;
    ensure(
        body["repository"]["full_name"] == format!("{owner}/{repo}").as_str(),
        || format!("run repository {}", body["repository"]["full_name"]),
    )?;
    ensure(
        body["head_repository"]["full_name"] == fork.as_str(),
        || {
            format!(
                "run head repository {}",
                body["head_repository"]["full_name"]
            )
        },
    )?;
    let cancel = format!("{runs}/cancel");
    let cancelled = world
        .send(request(
            "POST",
            cancel.clone(),
            Some(format!("Bearer {installation}")),
            None,
        ))
        .await?;
    expect_status(&cancelled, 202, "cancel a run")?;
    let again = world
        .send(request(
            "POST",
            cancel,
            Some(format!("Bearer {installation}")),
            None,
        ))
        .await?;
    expect_status(&again, 409, "cancel a cancelled run")?;
    expect_status(&client.close(&session).await?, 204, "delete session")
}

async fn wc15<W: WireWorld>(world: &W, name: &str) -> Outcome {
    let client = Client::connect(world).await?;
    let set = client.ensure_scale_set(name).await?;
    let session = client.open_ok(set).await?;
    script(world)?.queue_jobs(1, name, 9007, "push", "").await?;
    let (id, jobs, _) = next_message(&client, &session, 1, None).await?;
    let offer = jobs
        .iter()
        .find(|j| j["messageType"] == "JobAvailable")
        .ok_or("no JobAvailable")?;
    let request_id = offer["runnerRequestId"]
        .as_u64()
        .filter(|r| *r != 0)
        .ok_or("offer without a request id")?;
    expect_status(&client.ack(&session, id).await?, 204, "ack")?;
    let listed = client
        .svc(
            "GET",
            &format!("runtime/runnerscalesets/{set}/acquirablejobs"),
            None,
        )
        .await?;
    expect_status(&listed, 200, "acquirable jobs")?;
    ensure(
        listed.json()["value"]
            .as_array()
            .is_some_and(|v| v.iter().any(|j| j["runnerRequestId"] == request_id)),
        || "the offer is not acquirable".to_owned(),
    )?;
    let bogus = request_id + 1_000_000;
    let acquire = request(
        "POST",
        format!(
            "{}/_apis/runtime/runnerscalesets/{set}/acquirejobs?api-version=6.0-preview",
            client.service
        ),
        Some(format!("Bearer {}", session.token)),
        Some(json!([request_id, bogus])),
    );
    let acquired = world.send(acquire).await?;
    expect_status(&acquired, 200, "acquire")?;
    let value = acquired.json()["value"].clone();
    ensure(
        value
            .as_array()
            .is_some_and(|v| v.contains(&json!(request_id))),
        || format!("acquire answered {value} without {request_id}"),
    )?;
    ensure(
        !value.as_array().is_some_and(|v| v.contains(&json!(bogus))),
        || format!("acquire answered {value} with the bogus id"),
    )?;
    let (_, jobs, _) = next_message(&client, &session, 1, Some(id)).await?;
    ensure(
        jobs.iter()
            .any(|j| j["messageType"] == "JobAssigned" && j["runnerRequestId"] == request_id),
        || "no JobAssigned for the acquired job".to_owned(),
    )?;
    expect_status(&client.close(&session).await?, 204, "delete session")
}
