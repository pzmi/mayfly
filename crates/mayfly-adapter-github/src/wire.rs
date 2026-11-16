//! The anti-corruption layer of the Scale Set protocol (spec §2, §4): GitHub's JSON in, the
//! domain's values out. Every string is untrusted input: bounded ([`MAX_TEXT_BYTES`],
//! [`MAX_LABELS`]) and never interpolated; a value too long, of the wrong type or absent is
//! absent, and a field the classifier needs that is absent leaves the job unclassifiable (A5).
//! Ids are 64-bit (spec §2); statistics saturate into the domain's 32-bit counts.

use mayfly_control::github::{
    AssignedJob, EventName, JobFact, JobKey, JobResult, MessageId, MessageItem, Offer,
    PolledMessage, RepositoryName, RunnerId, RunnerInfo, RunnerRequestId, ScaleSetId, ScaleSetInfo,
    SessionId, Statistics, WorkflowJobId, WorkflowRun, WorkflowRunId,
};
use mayfly_domain::OwnedVmName;
use serde_json::Value;

use crate::config::split_full_name;

/// The longest string kept from GitHub (a name, a label, a job id); a longer one is dropped.
pub const MAX_TEXT_BYTES: usize = 256;

/// The most labels kept from one job message; further ones are dropped.
pub const MAX_LABELS: usize = 32;

/// The envelope type of job messages (spec §4.2).
pub const JOB_MESSAGES: &str = "RunnerScaleSetJobMessages";

/// A polled 200's body as a message, `None` when it is empty or does not decode (spec §4.1: a
/// transient poll failure, nothing recorded or acked). An envelope of another type is one
/// [`MessageItem::Unknown`] (protocol drift, spec §4.2), and so is a `body` that is not a JSON
/// array of objects; each unknown job message type is one `Unknown` in its place (spec §4.3).
/// Pinned by: `wire::tests::should_keep_arrival_order_and_unknown_types_when_decoding_a_batch`.
#[must_use]
pub fn decode_message(body: &[u8]) -> Option<PolledMessage> {
    let body = body.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(body);
    let envelope: Value = serde_json::from_slice(body).ok()?;
    let message_id = envelope.get("messageId").and_then(Value::as_u64)?;
    let statistics = envelope.get("statistics").and_then(decode_statistics);
    let items = if envelope.get("messageType").and_then(Value::as_str) == Some(JOB_MESSAGES) {
        decode_batch(envelope.get("body"))
    } else {
        vec![MessageItem::Unknown]
    };
    Some(PolledMessage {
        message_id: MessageId(message_id),
        statistics,
        items,
    })
}

/// The job messages of an envelope's `body`, a string holding a JSON array (empty: none).
fn decode_batch(body: Option<&Value>) -> Vec<MessageItem> {
    let text = match body {
        None | Some(Value::Null) => return Vec::new(),
        Some(Value::String(text)) => text,
        Some(_) => return vec![MessageItem::Unknown],
    };
    if text.trim().is_empty() {
        return Vec::new();
    }
    match serde_json::from_str::<Value>(text) {
        Ok(Value::Array(jobs)) => jobs.iter().map(decode_job).collect(),
        _ => vec![MessageItem::Unknown],
    }
}

/// One job message (spec §4.3).
#[must_use]
pub fn decode_job(job: &Value) -> MessageItem {
    let request_id = RunnerRequestId(job.get("runnerRequestId").and_then(id).unwrap_or(0));
    match job.get("messageType").and_then(Value::as_str) {
        Some("JobAvailable") => decode_offer(job).map_or(MessageItem::Unknown, MessageItem::Offer),
        Some("JobAssigned") => MessageItem::Fact(JobFact::Assigned {
            request_id,
            job: assigned_job(job),
        }),
        Some("JobStarted") => MessageItem::Fact(JobFact::Started {
            request_id,
            runner_id: runner_id(job),
            runner_name: text(job, "runnerName").and_then(|n| OwnedVmName::parse(&n).ok()),
            job: job_key(job),
        }),
        Some("JobCompleted") => MessageItem::Fact(JobFact::Completed {
            request_id,
            result: text(job, "result").map_or(JobResult::Other, |r| job_result(&r)),
            runner_id: runner_id(job),
            job: job_key(job),
        }),
        _ => MessageItem::Unknown,
    }
}

/// A `JobAvailable` as an offer: its request id (required, above 0) and its event (GHES, spec
/// §6.1).
#[must_use]
pub fn decode_offer(job: &Value) -> Option<Offer> {
    let request = job.get("runnerRequestId").and_then(id).filter(|r| *r > 0)?;
    Some(Offer {
        request_id: RunnerRequestId(request),
        event_name: text(job, "eventName").map_or(EventName::Unknown, |e| event_name(&e)),
    })
}

/// A `JobAssigned`'s key and classifying fields; `None` without a `jobId` and a
/// `workflowRunId` (KER-06G: it cannot be keyed). `runnerRequestId` is never part of it: 0 on
/// github.com (spec §4.3).
fn assigned_job(job: &Value) -> Option<AssignedJob> {
    let key = job_key(job)?;
    let repository = match (text(job, "ownerName"), text(job, "repositoryName")) {
        (Some(owner), Some(name)) if !owner.contains('/') && !name.contains('/') => {
            Some(RepositoryName(format!("{owner}/{name}")))
        }
        _ => None,
    };
    let labels = job
        .get("requestLabels")
        .and_then(Value::as_array)
        .map(|labels| {
            labels
                .iter()
                .filter_map(|l| l.as_str().and_then(bounded))
                .take(MAX_LABELS)
                .collect()
        })
        .unwrap_or_default();
    Some(AssignedJob {
        key,
        repository,
        event_name: text(job, "eventName").map_or(EventName::Unknown, |e| event_name(&e)),
        labels,
    })
}

/// `(jobId, workflowRunId)`, both present (KER-06G). `jobId` may come as a string or a number
/// (its type is `[open: HW-02, item 1]`).
fn job_key(job: &Value) -> Option<JobKey> {
    let job_id = match job.get("jobId")? {
        Value::String(s) => bounded(s)?,
        Value::Number(n) => n.as_u64()?.to_string(),
        _ => return None,
    };
    let run = job.get("workflowRunId").and_then(id).filter(|r| *r > 0)?;
    Some(JobKey {
        job_id: WorkflowJobId(job_id),
        run_id: WorkflowRunId(run),
    })
}

fn runner_id(job: &Value) -> Option<RunnerId> {
    job.get("runnerId")
        .and_then(id)
        .filter(|r| *r > 0)
        .map(RunnerId)
}

/// The seven counts (spec §2), each saturated to 32 bits; `None` when one is missing.
#[must_use]
pub fn decode_statistics(value: &Value) -> Option<Statistics> {
    let count = |name: &str| {
        value
            .get(name)
            .and_then(Value::as_u64)
            .map(|n| u32::try_from(n).unwrap_or(u32::MAX))
    };
    Some(Statistics {
        total_available_jobs: count("totalAvailableJobs")?,
        total_acquired_jobs: count("totalAcquiredJobs")?,
        total_assigned_jobs: count("totalAssignedJobs")?,
        total_running_jobs: count("totalRunningJobs")?,
        total_registered_runners: count("totalRegisteredRunners")?,
        total_busy_runners: count("totalBusyRunners")?,
        total_idle_runners: count("totalIdleRunners")?,
    })
}

/// A scale set (spec §2 "Scale set"): `id` and `name` required.
#[must_use]
pub fn decode_scale_set(value: &Value) -> Option<ScaleSetInfo> {
    Some(ScaleSetInfo {
        id: ScaleSetId(value.get("id").and_then(id).filter(|i| *i > 0)?),
        name: text(value, "name")?,
        runner_group_id: value.get("runnerGroupId").and_then(id).unwrap_or(0),
        labels: value
            .get("labels")
            .and_then(Value::as_array)
            .map(|labels| {
                labels
                    .iter()
                    .filter_map(|l| text(l, "name"))
                    .take(MAX_LABELS)
                    .collect()
            })
            .unwrap_or_default(),
    })
}

/// A runner reference (spec §2): `id` and `name` required; `status` and `busy` read when
/// present (client-assumed shape).
#[must_use]
pub fn decode_runner(value: &Value) -> Option<RunnerInfo> {
    Some(RunnerInfo {
        id: RunnerId(value.get("id").and_then(id).filter(|i| *i > 0)?),
        name: text(value, "name")?,
        scale_set: value
            .get("runnerScaleSetId")
            .and_then(id)
            .filter(|i| *i > 0)
            .map(ScaleSetId),
        online: value
            .get("status")
            .and_then(Value::as_str)
            .map(|s| s.eq_ignore_ascii_case("online")),
        busy: value.get("busy").and_then(Value::as_bool),
    })
}

/// A workflow run's lookup answer (spec §2, §6.2) for the run `asked`; `None` when it lacks
/// what the classification needs, so the lookup fails and the job is refused (ADR 0090: a
/// missing field refuses). Required: `id` equal to `asked`, a non-empty `event`, and
/// `repository.full_name` and `head_repository.full_name`, each a plain `owner/name` (a deleted
/// fork's `null` head is missing too). `actor.login` is kept when present. Every string is
/// bounded ([`MAX_TEXT_BYTES`]); an `event` the domain does not name is
/// [`EventName::Unknown`], which the domain refuses as unclassifiable. Pinned by
/// `wire::tests::should_decode_a_fork_and_refuse_the_run_when_a_field_is_missing`.
#[must_use]
pub fn decode_workflow_run(value: &Value, asked: WorkflowRunId) -> Option<WorkflowRun> {
    let id = value.get("id").and_then(id).filter(|i| *i == asked.0)?;
    let full_name = |field: &str| {
        let name = value.get(field).and_then(|r| text(r, "full_name"))?;
        split_full_name(&name)?;
        Some(RepositoryName(name))
    };
    Some(WorkflowRun {
        id: WorkflowRunId(id),
        repository: full_name("repository")?,
        head_repository: full_name("head_repository")?,
        event: event_name(&text(value, "event")?),
        actor: value.get("actor").and_then(|a| text(a, "login")),
    })
}

/// A `sessionId` (a UUID, spec §2) as its 128 bits; `None` when it is not one.
#[must_use]
pub fn session_id_from_wire(text: &str) -> Option<SessionId> {
    let bytes = text.as_bytes();
    let shaped = bytes.len() == 36
        && bytes.iter().enumerate().all(|(at, b)| match at {
            8 | 13 | 18 | 23 => *b == b'-',
            _ => b.is_ascii_hexdigit(),
        });
    if !shaped {
        return None;
    }
    let hex: String = text.chars().filter(|c| *c != '-').collect();
    u128::from_str_radix(&hex, 16)
        .ok()
        .map(SessionId::from_u128)
}

/// A [`SessionId`] as the UUID text a path carries.
#[must_use]
pub fn session_id_to_wire(id: SessionId) -> String {
    let v = id.as_u128();
    let hex = format!("{v:032x}");
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}

/// An `eventName` or a run's `event` as the domain names it (A5): any other value is
/// `Unknown`.
#[must_use]
pub fn event_name(text: &str) -> EventName {
    match text {
        "push" => EventName::Push,
        "pull_request" => EventName::PullRequest,
        "pull_request_target" => EventName::PullRequestTarget,
        "workflow_dispatch" => EventName::WorkflowDispatch,
        "schedule" => EventName::Schedule,
        "merge_group" => EventName::MergeGroup,
        "release" => EventName::Release,
        "workflow_run" => EventName::WorkflowRun,
        "issue_comment" => EventName::IssueComment,
        _ => EventName::Unknown,
    }
}

/// A `JobCompleted`'s `result` (spec §4.3; the full set is `[open: HW-02, item 1]`).
#[must_use]
pub fn job_result(text: &str) -> JobResult {
    match text.to_ascii_lowercase().as_str() {
        "succeeded" => JobResult::Succeeded,
        "failed" => JobResult::Failed,
        "canceled" | "cancelled" => JobResult::Canceled,
        _ => JobResult::Other,
    }
}

/// A non-negative integer id, as a number or a decimal string.
fn id(value: &Value) -> Option<u64> {
    match value {
        Value::Number(n) => n.as_u64(),
        Value::String(s) if s.len() <= 20 => s.parse().ok(),
        _ => None,
    }
}

/// The string field `name` of `value`, bounded and non-empty.
fn text(value: &Value, name: &str) -> Option<String> {
    value.get(name).and_then(Value::as_str).and_then(bounded)
}

fn bounded(text: &str) -> Option<String> {
    (!text.is_empty() && text.len() <= MAX_TEXT_BYTES).then(|| text.to_owned())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn envelope(message_type: &str, jobs: &Value) -> Vec<u8> {
        json!({
            "messageId": 100_000_001_u64,
            "messageType": message_type,
            "body": jobs.to_string(),
            "statistics": {
                "totalAvailableJobs": 0, "totalAcquiredJobs": 0, "totalAssignedJobs": 1,
                "totalRunningJobs": 0, "totalRegisteredRunners": 0, "totalBusyRunners": 0,
                "totalIdleRunners": 0,
            },
        })
        .to_string()
        .into_bytes()
    }

    #[test]
    fn should_keep_arrival_order_and_unknown_types_when_decoding_a_batch() {
        // given: a direct assignment (request id 0), an unknown type, a completion
        let jobs = json!([
            {"messageType": "JobAssigned", "runnerRequestId": 0, "jobId": "j-1",
             "workflowRunId": 77, "ownerName": "acme", "repositoryName": "app",
             "eventName": "push", "requestLabels": ["mayfly"]},
            {"messageType": "JobSomethingNew"},
            {"messageType": "JobCompleted", "runnerRequestId": 0, "jobId": "j-1",
             "workflowRunId": 77, "result": "canceled"},
        ]);

        // when
        let message = decode_message(&envelope(JOB_MESSAGES, &jobs)).expect("decodes");

        // then
        assert_eq!(message.message_id, MessageId(100_000_001));
        assert_eq!(message.statistics.map(|s| s.total_assigned_jobs), Some(1));
        let key = JobKey {
            job_id: WorkflowJobId("j-1".to_owned()),
            run_id: WorkflowRunId(77),
        };
        assert_eq!(
            message.items,
            vec![
                MessageItem::Fact(JobFact::Assigned {
                    request_id: RunnerRequestId(0),
                    job: Some(AssignedJob {
                        key: key.clone(),
                        repository: Some(RepositoryName("acme/app".to_owned())),
                        event_name: EventName::Push,
                        labels: vec!["mayfly".to_owned()],
                    }),
                }),
                MessageItem::Unknown,
                MessageItem::Fact(JobFact::Completed {
                    request_id: RunnerRequestId(0),
                    result: JobResult::Canceled,
                    runner_id: None,
                    job: Some(key),
                }),
            ]
        );
    }

    #[test]
    fn should_leave_an_assignment_unclassifiable_when_its_fields_are_missing_or_too_long() {
        // given: no owner, an event of no known name, an over-long label; and one without a run
        let long = "x".repeat(MAX_TEXT_BYTES + 1);
        let jobs = json!([
            {"messageType": "JobAssigned", "runnerRequestId": 0, "jobId": 5,
             "workflowRunId": "9", "repositoryName": "app", "eventName": "fork_magic",
             "requestLabels": ["mayfly", long]},
            {"messageType": "JobAssigned", "runnerRequestId": 0, "jobId": "j"},
        ]);

        // when
        let message = decode_message(&envelope(JOB_MESSAGES, &jobs)).expect("decodes");

        // then
        assert_eq!(
            message.items,
            vec![
                MessageItem::Fact(JobFact::Assigned {
                    request_id: RunnerRequestId(0),
                    job: Some(AssignedJob {
                        key: JobKey {
                            job_id: WorkflowJobId("5".to_owned()),
                            run_id: WorkflowRunId(9),
                        },
                        repository: None,
                        event_name: EventName::Unknown,
                        labels: vec!["mayfly".to_owned()],
                    }),
                }),
                MessageItem::Fact(JobFact::Assigned {
                    request_id: RunnerRequestId(0),
                    job: None,
                }),
            ]
        );
    }

    #[test]
    fn should_record_drift_or_refuse_when_the_envelope_is_unknown_or_empty() {
        // given
        let other = envelope("SomethingElse", &json!([]));
        let not_a_list = json!({"messageId": 3, "messageType": JOB_MESSAGES, "body": "{}"})
            .to_string()
            .into_bytes();
        let empty_body = json!({"messageId": 4, "messageType": JOB_MESSAGES, "body": ""})
            .to_string()
            .into_bytes();

        // when
        let decoded = [
            decode_message(&other).map(|m| m.items),
            decode_message(&not_a_list).map(|m| m.items),
            decode_message(&empty_body).map(|m| m.items),
            decode_message(b"").map(|m| m.items),
            decode_message(b"{\"messageType\":\"RunnerScaleSetJobMessages\"}").map(|m| m.items),
        ]
        .map(|d| d.map(|items| items.len()));

        // then: drift is one Unknown; an empty batch has none; no body or no id is transient
        assert_eq!(decoded, [Some(1), Some(1), Some(0), None, None]);
    }

    #[test]
    fn should_decode_a_fork_and_refuse_the_run_when_a_field_is_missing() {
        // given
        let repo = |full: &str| json!({"full_name": full, "owner": {"login": "x"}});
        let fork = json!({
            "id": 7,
            "event": "pull_request",
            "actor": {"login": "contributor"},
            "repository": repo("acme/app"),
            "head_repository": repo("contributor/app"),
        });
        let mut broken = Vec::new();
        for field in ["id", "event", "repository", "head_repository"] {
            let mut run = fork.clone();
            if let Some(fields) = run.as_object_mut() {
                fields.remove(field);
            }
            broken.push(run);
        }
        let with = |field: &str, value: Value| {
            let mut run = fork.clone();
            run[field] = value;
            run
        };
        broken.push(with("id", json!(8)));
        broken.push(with("head_repository", Value::Null));
        broken.push(with("repository", repo("acme/app/extra")));
        broken.push(with(
            "repository",
            repo(&format!("acme/{}", "a".repeat(300))),
        ));
        broken.push(with("event", json!("")));
        let odd = with("event", json!("repository_dispatch"));
        let anonymous = with("actor", Value::Null);

        // when
        let decoded = decode_workflow_run(&fork, WorkflowRunId(7));
        let refused: Vec<Option<WorkflowRun>> = broken
            .iter()
            .map(|r| decode_workflow_run(r, WorkflowRunId(7)))
            .collect();

        // then: the fork's head is not its base; any missing or malformed field fails the lookup
        assert_eq!(
            decoded,
            Some(WorkflowRun {
                id: WorkflowRunId(7),
                repository: RepositoryName("acme/app".to_owned()),
                head_repository: RepositoryName("contributor/app".to_owned()),
                event: EventName::PullRequest,
                actor: Some("contributor".to_owned()),
            })
        );
        assert!(refused.iter().all(Option::is_none), "{refused:?}");
        assert_eq!(
            decode_workflow_run(&odd, WorkflowRunId(7)).map(|r| r.event),
            Some(EventName::Unknown)
        );
        assert_eq!(
            decode_workflow_run(&anonymous, WorkflowRunId(7)).map(|r| r.actor),
            Some(None)
        );
    }

    #[test]
    fn should_round_trip_a_session_id_when_it_is_a_uuid() {
        // given
        let text = "0f1e2d3c-4b5a-4968-8776-655443322110";

        // when
        let id = session_id_from_wire(text).expect("uuid");

        // then
        assert_eq!(session_id_to_wire(id), text);
        assert_eq!(session_id_from_wire("not-a-uuid"), None);
        assert_eq!(
            session_id_from_wire("0f1e2d3c4b5a49688776655443322110"),
            None
        );
        assert_eq!(
            session_id_from_wire("0f1e2d3c4-b5a-4968-8776-655443322110"),
            None
        );
    }

    #[test]
    fn should_saturate_statistics_when_a_count_exceeds_32_bits() {
        // given
        let value = json!({
            "totalAvailableJobs": 5_000_000_000_u64, "totalAcquiredJobs": 0,
            "totalAssignedJobs": 2, "totalRunningJobs": 1, "totalRegisteredRunners": 0,
            "totalBusyRunners": 0, "totalIdleRunners": 0,
        });

        // when
        let statistics = decode_statistics(&value).expect("all seven");

        // then
        assert_eq!(statistics.total_available_jobs, u32::MAX);
        assert_eq!(statistics.total_assigned_jobs, 2);
        assert_eq!(decode_statistics(&json!({"totalAssignedJobs": 1})), None);
    }
}
