//! Schedule endpoints and what a schedule does when it fires.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::api::native::{check_task_policies, enqueue, find_stack, latest_config, task_command, SpaceQ};
use crate::app::{ApiError, ApiResult, AppRef};
use crate::auth::{Principal, Role};
use crate::schedule::{self, Schedule, KINDS};
use crate::store::{self, Stack};

fn need_for(kind: &str) -> Role {
    if kind == "destroy" {
        Role::Admin
    } else {
        Role::Writer
    }
}

pub async fn list(State(app): State<AppRef>, p: Principal, Path(id): Path<String>, Query(q): Query<SpaceQ>) -> ApiResult<Json<Value>> {
    let s = find_stack(&app, &p, &id, q.space.as_deref(), Role::Reader)?;
    Ok(Json(json!({"items": app.db.read(|c| schedule::list(c, &s.id))?})))
}

#[derive(Deserialize)]
pub struct NewScheduleReq {
    kind: String,
    #[serde(default)]
    cron: Option<String>,
    #[serde(default)]
    at: Option<String>,
    /// drift: start a tracked run when drift is found.
    #[serde(default)]
    reconcile: bool,
    /// task: the tofu command.
    #[serde(default)]
    command: Option<Value>,
    /// destroy: apply without confirmation (default true).
    #[serde(default)]
    auto_apply: Option<bool>,
}

pub async fn create(
    State(app): State<AppRef>, p: Principal, Path(id): Path<String>, Query(q): Query<SpaceQ>, Json(r): Json<NewScheduleReq>,
) -> ApiResult<impl IntoResponse> {
    if !KINDS.contains(&r.kind.as_str()) {
        return Err(ApiError::bad_request(format!("kind must be one of {}", KINDS.join(", "))));
    }
    let s = find_stack(&app, &p, &id, q.space.as_deref(), need_for(&r.kind))?;
    let command = match (&r.kind[..], &r.command) {
        ("task", Some(c)) => {
            let cmd = task_command(c)?;
            check_task_policies(&app, &s, &p, &cmd)?;
            Some(cmd)
        }
        ("task", None) => return Err(ApiError::bad_request("task schedules need a command")),
        _ => None,
    };
    let params = schedule::params_json(r.reconcile && r.kind == "drift", command.as_deref(), r.auto_apply.filter(|_| r.kind == "destroy"));
    let sch = app
        .db
        .write(|c| schedule::create(c, &s.id, &r.kind, r.cron.as_deref(), r.at.as_deref(), &params))
        .map_err(|e| ApiError::bad_request(format!("{e:#}")))?;
    app.schedule_changed.notify_one();
    Ok((StatusCode::CREATED, Json(sch)))
}

pub async fn delete(
    State(app): State<AppRef>, p: Principal, Path((id, sid)): Path<(String, String)>, Query(q): Query<SpaceQ>,
) -> ApiResult<StatusCode> {
    let s = find_stack(&app, &p, &id, q.space.as_deref(), Role::Reader)?;
    let sch = app.db.read(|c| schedule::get(c, &sid))?.filter(|x| x.stack_id == s.id).ok_or_else(|| ApiError::not_found("schedule"))?;
    app.authz.require(&p, &s.space_id, need_for(&sch.kind))?;
    app.db.write(|c| schedule::delete(c, &s.id, &sid))?;
    app.schedule_changed.notify_one();
    Ok(StatusCode::NO_CONTENT)
}

fn deployed_config(app: &AppRef, s: &Stack) -> ApiResult<String> {
    match app.db.read(|c| store::last_tracked_config(c, &s.id))? {
        Some(cv) => Ok(cv),
        None => latest_config(app, s).map_err(|_| ApiError::conflict("stack has no configuration yet")),
    }
}

/// Start the run a schedule asks for. Returns the run ID (None: skipped).
pub async fn fire(app: &AppRef, sch: &Schedule) -> ApiResult<Option<String>> {
    let s = app.db.read(|c| store::get_stack(c, &sch.stack_id))?.ok_or_else(|| ApiError::not_found("stack"))?;
    let active = app.db.read(store::active_runs)?;
    if sch.kind == "drift" && active.iter().any(|r| r.stack_id == s.id && r.kind == "drift") {
        return Ok(None); // the previous check is still running
    }
    let cv = deployed_config(app, &s)?;
    let by = format!("schedule:{}", sch.id);
    let (kind, auto_apply, params, message) = match sch.kind.as_str() {
        "drift" => ("drift", false, Some(json!({"reconcile": sch.params["reconcile"] == json!(true)})), "Scheduled drift detection".to_string()),
        "tracked" => ("tracked", s.auto_apply, None, "Scheduled run".to_string()),
        "destroy" => ("destroy", sch.params["auto_apply"].as_bool().unwrap_or(true), None, "Scheduled destroy".to_string()),
        "task" => {
            let cmd: Vec<String> = serde_json::from_value(sch.params["command"].clone()).unwrap_or_default();
            let p = Principal { kind: "schedule", id: sch.id.clone(), name: by.clone(), groups: vec![], site_admin: false, policy_roles: Default::default() };
            check_task_policies(app, &s, &p, &cmd)?;
            ("task", false, Some(json!({"command": cmd})), format!("Scheduled task: tofu {}", cmd.join(" ")))
        }
        k => return Err(ApiError::bad_request(format!("unknown schedule kind {k}"))),
    };
    let run = enqueue(app, &s, &store::NewRun {
        stack_id: &s.id, kind, config_id: Some(&cv), auto_apply, message: &message, triggered_by: &by, source: None, params: params.as_ref(),
    })?;
    Ok(Some(run.id))
}
