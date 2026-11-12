//! The REST routes of ADR 0090's trust check (spec §2, §6.2), on the installation token, each
//! refused with 403 when the installation lacks its permission (ADR 0045, spec §13).

use serde_json::json;

use crate::routes::Route;
use crate::state::{Ctx, FakeResponse, Scope, State, not_found};
use crate::wire::{bearer, json, status};
use crate::world::JobState;

/// Answers a REST route.
pub(crate) fn answer(state: &mut State, route: Route, ctx: &Ctx<'_>) -> FakeResponse {
    let live = bearer(ctx.request)
        .and_then(|token| state.installation_tokens.get(&token).copied())
        .is_some_and(|expires| expires > ctx.now);
    if !live {
        return status(401, "Bad credentials");
    }
    let (permission, levels): (&str, &[&str]) = match route {
        Route::WorkflowRun => ("actions", &["read", "write"]),
        Route::CancelRun => ("actions", &["write"]),
        Route::ForkPrSetting => ("organization_administration", &["read", "write"]),
        _ => ("organization_self_hosted_runners", &["read", "write"]),
    };
    let granted = ctx
        .config
        .permissions
        .get(permission)
        .is_some_and(|level| levels.contains(&level.as_str()));
    if !granted {
        return status(403, "Resource not accessible by integration");
    }
    match route {
        Route::WorkflowRun => workflow_run(state, ctx),
        Route::CancelRun => cancel_run(state, ctx),
        Route::ForkPrSetting => fork_pr_setting(state, ctx),
        _ => runner_group(state, ctx),
    }
}

fn org_matches(ctx: &Ctx<'_>) -> bool {
    match &ctx.config.scope {
        Scope::Organization(org) => ctx.params.text.as_deref() == Some(org.as_str()),
        Scope::Repository(owner, _) => ctx.params.text.as_deref() == Some(owner.as_str()),
    }
}

/// The run a path names, in the repository the path names.
fn path_run(state: &State, ctx: &Ctx<'_>) -> Option<u64> {
    let run = state.world.runs.get(&ctx.params.id?)?;
    let same = ctx
        .params
        .text
        .as_deref()
        .is_some_and(|o| o.eq_ignore_ascii_case(&run.owner))
        && ctx
            .params
            .repo
            .as_deref()
            .is_some_and(|r| r.eq_ignore_ascii_case(&run.repository));
    same.then_some(run.id)
}

fn workflow_run(state: &mut State, ctx: &Ctx<'_>) -> FakeResponse {
    let Some(id) = path_run(state, ctx) else {
        return not_found();
    };
    let run = &state.world.runs[&id];
    let finished = run.cancelled
        || state
            .world
            .jobs
            .values()
            .filter(|j| j.spec.run_id == id)
            .all(|j| matches!(j.state, JobState::Finished { .. }));
    let base = format!("{}/{}", run.owner, run.repository);
    let head = if run.head_repository.is_empty() {
        base.clone()
    } else {
        run.head_repository.clone()
    };
    let repo = |full: &str| {
        let (owner, name) = full.split_once('/').unwrap_or((full, ""));
        json!({"full_name": full, "name": name, "owner": {"login": owner}})
    };
    let body = json!({
        "id": run.id,
        "event": run.event,
        "status": if finished { "completed" } else { "in_progress" },
        "conclusion": if run.cancelled { json!("cancelled") } else { json!(null) },
        "actor": {"login": run.actor},
        "triggering_actor": {"login": run.actor},
        "repository": repo(&base),
        "head_repository": repo(&head),
    });
    state.counts.run_lookups += 1;
    json(200, &body)
}

/// 202, or 409 for a run that is over (spec §2). Pinned by
/// `tests/wire/rest.rs::should_answer_409_when_cancelling_a_completed_run`.
fn cancel_run(state: &mut State, ctx: &Ctx<'_>) -> FakeResponse {
    let Some(id) = path_run(state, ctx) else {
        return not_found();
    };
    let open: Vec<(u64, Option<u64>)> = state
        .world
        .jobs
        .values()
        .filter(|j| j.spec.run_id == id)
        .filter_map(|j| match j.state {
            JobState::Finished { .. } => None,
            JobState::Queued => Some((j.id, None)),
            JobState::Offered { scale_set, .. }
            | JobState::Assigned { scale_set, .. }
            | JobState::Running { scale_set, .. } => Some((j.id, Some(scale_set))),
        })
        .collect();
    if state.world.runs[&id].cancelled || open.is_empty() {
        return status(409, "Cannot cancel a workflow run that is completed.");
    }
    let knobs = state.knobs.clone();
    for (job, held) in open {
        match held {
            Some(scale_set) => state
                .world
                .finish(job, scale_set, "canceled", &knobs, ctx.now),
            None => {
                if let Some(j) = state.world.jobs.get_mut(&job) {
                    j.state = JobState::Finished {
                        result: "canceled".to_owned(),
                    };
                    j.finished_at = Some(ctx.now);
                }
            }
        }
    }
    if let Some(run) = state.world.runs.get_mut(&id) {
        run.cancelled = true;
    }
    state.counts.run_cancels += 1;
    json(202, &json!({}))
}

fn fork_pr_setting(state: &State, ctx: &Ctx<'_>) -> FakeResponse {
    if !org_matches(ctx) {
        return not_found();
    }
    json(200, &json!(state.knobs.fork_pr_setting))
}

fn runner_group(state: &State, ctx: &Ctx<'_>) -> FakeResponse {
    if !org_matches(ctx) {
        return not_found();
    }
    let Some(group) = state
        .knobs
        .runner_groups
        .iter()
        .find(|g| Some(g.id) == ctx.params.id)
    else {
        return not_found();
    };
    json(
        200,
        &json!({
            "id": group.id,
            "name": group.name,
            "visibility": "selected",
            "default": group.default,
            "allows_public_repositories": group.allows_public_repositories,
            "restricted_to_workflows": false,
            "selected_workflows": [],
        }),
    )
}
