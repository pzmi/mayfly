//! Answer shapes shared by every route (spec §2).

use serde_json::{Value, json};

use crate::state::{FakeRequest, FakeResponse};

/// The error classes mayfly tells apart, by `typeName` (spec §2). The conflict's full name is
/// server-observed (spec §3.1); for the others the spec states only the class name the
/// `typeName` contains, so the fake writes that alone.
pub const SESSION_CONFLICT: &str =
    "GitHub.Actions.Runtime.WebApi.RunnerScaleSetSessionConflictException";
/// The session is gone (spec §3.1).
pub const SESSION_EXPIRED: &str = "RunnerScaleSetSessionExpiredException";
/// The runner is gone (spec §7).
pub const AGENT_NOT_FOUND: &str = "AgentNotFoundException";
/// The JIT name is taken (spec §6.3).
pub const AGENT_EXISTS: &str = "AgentExistsException";
/// The runner is busy (spec §7).
pub const JOB_STILL_RUNNING: &str = "JobStillRunningException";
/// JIT minting is throttled (spec §8).
pub const TOO_MANY_RUNNERS: &str = "ConfigureTooManyRunnersException";

/// A JSON answer.
pub fn json(status: u16, body: &Value) -> FakeResponse {
    FakeResponse {
        status,
        headers: vec![("content-type".to_owned(), "application/json".to_owned())],
        body: body.to_string().into_bytes(),
    }
}

/// An answer with no body.
pub fn empty(status: u16) -> FakeResponse {
    FakeResponse {
        status,
        headers: Vec::new(),
        body: Vec::new(),
    }
}

/// An error with a message only: a definite refusal of no particular class.
pub fn status(code: u16, message: &str) -> FakeResponse {
    json(code, &json!({"message": message}))
}

/// An error of a class, `{"typeName", "message"}` (spec §2).
pub fn typed(code: u16, type_name: &str, message: &str) -> FakeResponse {
    json(code, &json!({"typeName": type_name, "message": message}))
}

/// A header of `request`, by name in any case.
pub fn header(request: &FakeRequest, name: &str) -> Option<String> {
    request
        .headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.clone())
}

/// The bearer credential of `request`, if any.
pub fn bearer(request: &FakeRequest) -> Option<String> {
    header(request, "authorization")?
        .strip_prefix("Bearer ")
        .map(str::to_owned)
}
