//! Routes: which endpoint of spec §1, §2 and §4 a request names, by method and path alone.
//!
//! Hosts are ignored: the service URL, the queue URL and the REST base may share one listener,
//! so each lives under a distinct path (the service's under the path of its URL, the queue's
//! under `/_queue/`, the control API under `/_fake/`).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// An endpoint the fake serves, the unit faults and gates are queued on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Route {
    /// `POST /app/installations/{id}/access_tokens`.
    InstallationToken,
    /// `POST /orgs/{org}/actions/runners/registration-token` (or the repository's).
    RegistrationToken,
    /// `POST /actions/runner-registration`.
    RunnerRegistration,
    /// `GET {svc}/_apis/runtime/runnergroups/?groupName=`.
    RunnerGroupByName,
    /// `GET {svc}/_apis/runtime/runnerscalesets?runnerGroupId=&name=`.
    ScaleSetByName,
    /// `GET {svc}/_apis/runtime/runnerscalesets/{id}`.
    ScaleSetById,
    /// `POST {svc}/_apis/runtime/runnerscalesets`.
    CreateScaleSet,
    /// `PATCH {svc}/_apis/runtime/runnerscalesets/{id}`.
    UpdateScaleSet,
    /// `POST {svc}/_apis/runtime/runnerscalesets/{id}/sessions`.
    CreateSession,
    /// `PATCH {svc}/_apis/runtime/runnerscalesets/{id}/sessions/{sessionId}`.
    RefreshSession,
    /// `DELETE {svc}/_apis/runtime/runnerscalesets/{id}/sessions/{sessionId}`.
    DeleteSession,
    /// `GET <messageQueueUrl>`: the long poll.
    Poll,
    /// `DELETE <messageQueueUrl>/{messageId}`.
    Ack,
    /// `POST {svc}/_apis/runtime/runnerscalesets/{id}/acquirejobs` (GHES).
    AcquireJobs,
    /// `GET {svc}/_apis/runtime/runnerscalesets/{id}/acquirablejobs` (GHES).
    AcquirableJobs,
    /// `POST {svc}/_apis/runtime/runnerscalesets/{id}/generatejitconfig`.
    GenerateJitConfig,
    /// `GET {svc}/_apis/distributedtask/pools/0/agents/{runnerId}`.
    RunnerById,
    /// `GET {svc}/_apis/distributedtask/pools/0/agents[?agentName=]`.
    ListRunners,
    /// `DELETE {svc}/_apis/distributedtask/pools/0/agents/{runnerId}`.
    RemoveRunner,
    /// `GET /repos/{owner}/{repo}/actions/runs/{run_id}`.
    WorkflowRun,
    /// `POST /repos/{owner}/{repo}/actions/runs/{run_id}/cancel`.
    CancelRun,
    /// `GET /orgs/{org}/actions/permissions/fork-pr-workflows-private-repos`.
    ForkPrSetting,
    /// `GET /orgs/{org}/actions/runner-groups/{group_id}`.
    RunnerGroup,
}

/// A request matched to its route, with the parts of its path and query it carries.
#[derive(Debug, Clone, Default)]
pub(crate) struct Params {
    /// The scale set's id, or the run's, or the runner group's.
    pub id: Option<u64>,
    /// The second id: the runner's, the message's, the queue URL's generation.
    pub sub: Option<u64>,
    /// A session id, a repository owner or an organisation.
    pub text: Option<String>,
    /// A repository name.
    pub repo: Option<String>,
    /// The query, percent-decoded.
    pub query: BTreeMap<String, String>,
}

/// The prefix the service's routes live under: the path of its URL, without a trailing slash.
pub(crate) fn url_path(url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let path = rest.find('/').map_or("", |at| &rest[at..]);
    path.trim_end_matches('/').to_owned()
}

/// Splits `path?query` into the path and its decoded query.
pub(crate) fn split_query(target: &str) -> (&str, BTreeMap<String, String>) {
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let query = query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            (decode(k), decode(v))
        })
        .collect();
    (path, query)
}

fn decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3])
                    .ok()
                    .and_then(|h| u8::from_str_radix(h, 16).ok());
                match hex {
                    Some(byte) => {
                        out.push(byte);
                        i += 2;
                    }
                    None => out.push(b'%'),
                }
            }
            byte => out.push(byte),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Matches `method path` to a route, given the service's and the queue's path prefixes and the
/// installation, registration and scope paths of the auth chain.
pub(crate) fn route(
    method: &str,
    path: &str,
    service: &str,
    queue: &str,
    auth: &AuthPaths,
) -> Option<(Route, Params)> {
    let mut params = Params::default();
    if method == "POST" {
        if path == auth.access {
            return Some((Route::InstallationToken, params));
        }
        if path == auth.registration {
            return Some((Route::RegistrationToken, params));
        }
        if path == "/actions/runner-registration" {
            return Some((Route::RunnerRegistration, params));
        }
    }
    if let Some(rest) = path
        .strip_prefix(queue)
        .and_then(|r| r.strip_prefix("/_queue/"))
    {
        return queue_route(method, rest, params);
    }
    if let Some(rest) = path
        .strip_prefix(service)
        .and_then(|r| r.strip_prefix("/_apis/"))
    {
        return service_route(method, rest, params);
    }
    let segments: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    let found = match (method, segments.as_slice()) {
        ("GET", ["repos", owner, repo, "actions", "runs", id]) => {
            params.text = Some((*owner).to_owned());
            params.repo = Some((*repo).to_owned());
            params.id = Some(id.parse().ok()?);
            Route::WorkflowRun
        }
        ("POST", ["repos", owner, repo, "actions", "runs", id, "cancel"]) => {
            params.text = Some((*owner).to_owned());
            params.repo = Some((*repo).to_owned());
            params.id = Some(id.parse().ok()?);
            Route::CancelRun
        }
        (
            "GET",
            [
                "orgs",
                org,
                "actions",
                "permissions",
                "fork-pr-workflows-private-repos",
            ],
        ) => {
            params.text = Some((*org).to_owned());
            Route::ForkPrSetting
        }
        ("GET", ["orgs", org, "actions", "runner-groups", id]) => {
            params.text = Some((*org).to_owned());
            params.id = Some(id.parse().ok()?);
            Route::RunnerGroup
        }
        _ => return None,
    };
    Some((found, params))
}

/// The auth chain's paths, which depend on the configuration.
pub(crate) struct AuthPaths {
    pub access: String,
    pub registration: String,
}

fn queue_route(method: &str, rest: &str, mut params: Params) -> Option<(Route, Params)> {
    let segments: Vec<&str> = rest.split('/').collect();
    let found = match (method, segments.as_slice()) {
        ("GET", [set, generation, "messages"]) => {
            params.id = Some(set.parse().ok()?);
            params.sub = Some(generation.parse().ok()?);
            Route::Poll
        }
        ("DELETE", [set, generation, "messages", message]) => {
            params.id = Some(set.parse().ok()?);
            params.sub = Some(generation.parse().ok()?);
            params
                .query
                .insert("messageId".to_owned(), (*message).to_owned());
            Route::Ack
        }
        _ => return None,
    };
    Some((found, params))
}

fn service_route(method: &str, rest: &str, mut params: Params) -> Option<(Route, Params)> {
    let segments: Vec<&str> = rest.trim_end_matches('/').split('/').collect();
    let found = match (method, segments.as_slice()) {
        ("GET", ["runtime", "runnergroups"]) => Route::RunnerGroupByName,
        ("GET", ["runtime", "runnerscalesets"]) => Route::ScaleSetByName,
        ("POST", ["runtime", "runnerscalesets"]) => Route::CreateScaleSet,
        ("GET", ["runtime", "runnerscalesets", id]) => {
            params.id = Some(id.parse().ok()?);
            Route::ScaleSetById
        }
        ("PATCH", ["runtime", "runnerscalesets", id]) => {
            params.id = Some(id.parse().ok()?);
            Route::UpdateScaleSet
        }
        ("POST", ["runtime", "runnerscalesets", id, "sessions"]) => {
            params.id = Some(id.parse().ok()?);
            Route::CreateSession
        }
        (verb @ ("PATCH" | "DELETE"), ["runtime", "runnerscalesets", id, "sessions", session]) => {
            params.id = Some(id.parse().ok()?);
            params.text = Some((*session).to_owned());
            if verb == "PATCH" {
                Route::RefreshSession
            } else {
                Route::DeleteSession
            }
        }
        ("POST", ["runtime", "runnerscalesets", id, "acquirejobs"]) => {
            params.id = Some(id.parse().ok()?);
            Route::AcquireJobs
        }
        ("GET", ["runtime", "runnerscalesets", id, "acquirablejobs"]) => {
            params.id = Some(id.parse().ok()?);
            Route::AcquirableJobs
        }
        ("POST", ["runtime", "runnerscalesets", id, "generatejitconfig"]) => {
            params.id = Some(id.parse().ok()?);
            Route::GenerateJitConfig
        }
        ("GET", ["distributedtask", "pools", "0", "agents"]) => Route::ListRunners,
        ("GET", ["distributedtask", "pools", "0", "agents", id]) => {
            params.sub = Some(id.parse().ok()?);
            Route::RunnerById
        }
        ("DELETE", ["distributedtask", "pools", "0", "agents", id]) => {
            params.sub = Some(id.parse().ok()?);
            Route::RemoveRunner
        }
        _ => return None,
    };
    Some((found, params))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn auth() -> AuthPaths {
        AuthPaths {
            access: "/app/installations/7/access_tokens".to_owned(),
            registration: "/orgs/acme/actions/runners/registration-token".to_owned(),
        }
    }

    #[test]
    fn should_name_each_route_when_matching_method_and_path() {
        // given
        let cases = [
            (
                "POST",
                "/app/installations/7/access_tokens",
                Route::InstallationToken,
            ),
            (
                "POST",
                "/orgs/acme/actions/runners/registration-token",
                Route::RegistrationToken,
            ),
            (
                "POST",
                "/actions/runner-registration",
                Route::RunnerRegistration,
            ),
            (
                "GET",
                "/svc/_apis/runtime/runnergroups/",
                Route::RunnerGroupByName,
            ),
            (
                "GET",
                "/svc/_apis/runtime/runnerscalesets",
                Route::ScaleSetByName,
            ),
            (
                "POST",
                "/svc/_apis/runtime/runnerscalesets",
                Route::CreateScaleSet,
            ),
            (
                "GET",
                "/svc/_apis/runtime/runnerscalesets/3",
                Route::ScaleSetById,
            ),
            (
                "PATCH",
                "/svc/_apis/runtime/runnerscalesets/3",
                Route::UpdateScaleSet,
            ),
            (
                "POST",
                "/svc/_apis/runtime/runnerscalesets/3/sessions",
                Route::CreateSession,
            ),
            (
                "PATCH",
                "/svc/_apis/runtime/runnerscalesets/3/sessions/u",
                Route::RefreshSession,
            ),
            (
                "DELETE",
                "/svc/_apis/runtime/runnerscalesets/3/sessions/u",
                Route::DeleteSession,
            ),
            (
                "POST",
                "/svc/_apis/runtime/runnerscalesets/3/acquirejobs",
                Route::AcquireJobs,
            ),
            (
                "GET",
                "/svc/_apis/runtime/runnerscalesets/3/acquirablejobs",
                Route::AcquirableJobs,
            ),
            (
                "POST",
                "/svc/_apis/runtime/runnerscalesets/3/generatejitconfig",
                Route::GenerateJitConfig,
            ),
            (
                "GET",
                "/svc/_apis/distributedtask/pools/0/agents",
                Route::ListRunners,
            ),
            (
                "GET",
                "/svc/_apis/distributedtask/pools/0/agents/9",
                Route::RunnerById,
            ),
            (
                "DELETE",
                "/svc/_apis/distributedtask/pools/0/agents/9",
                Route::RemoveRunner,
            ),
            ("GET", "/q/_queue/3/0/messages", Route::Poll),
            ("DELETE", "/q/_queue/3/0/messages/100000001", Route::Ack),
            ("GET", "/repos/acme/app/actions/runs/5", Route::WorkflowRun),
            (
                "POST",
                "/repos/acme/app/actions/runs/5/cancel",
                Route::CancelRun,
            ),
            (
                "GET",
                "/orgs/acme/actions/permissions/fork-pr-workflows-private-repos",
                Route::ForkPrSetting,
            ),
            (
                "GET",
                "/orgs/acme/actions/runner-groups/1",
                Route::RunnerGroup,
            ),
        ];

        // when
        let got: Vec<Option<Route>> = cases
            .iter()
            .map(|(m, p, _)| route(m, p, "/svc", "/q", &auth()).map(|(r, _)| r))
            .collect();

        // then
        let expected: Vec<Option<Route>> = cases.iter().map(|(_, _, r)| Some(*r)).collect();
        assert_eq!(got, expected);
    }

    #[test]
    fn should_match_nothing_when_method_or_path_is_unknown() {
        // given
        let cases = [
            ("GET", "/app/installations/7/access_tokens"),
            ("PUT", "/svc/_apis/runtime/runnerscalesets/3"),
            ("GET", "/svc/_apis/runtime/runnerscalesets/x"),
            ("GET", "/elsewhere"),
            ("POST", "/repos/acme/app/actions/runs/5"),
        ];

        // when
        let got: Vec<bool> = cases
            .iter()
            .map(|(m, p)| route(m, p, "/svc", "/q", &auth()).is_some())
            .collect();

        // then
        assert_eq!(got, vec![false; cases.len()]);
    }

    #[test]
    fn should_decode_the_query_when_splitting_a_target() {
        // given
        let target = "/x?groupName=My%20Group&name=a+b&flag&bad=%zz";

        // when
        let (path, query) = split_query(target);

        // then
        assert_eq!(path, "/x");
        assert_eq!(query["groupName"], "My Group");
        assert_eq!(query["name"], "a b");
        assert_eq!(query["flag"], "");
        assert_eq!(query["bad"], "%zz");
        assert_eq!(url_path("https://pipelines.example/svc/"), "/svc");
        assert_eq!(url_path("http://127.0.0.1:9"), "");
    }
}
