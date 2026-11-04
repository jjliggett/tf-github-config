//! Stack dependency endpoints.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::api::native::{find_stack, SpaceQ};
use crate::app::{ApiError, ApiResult, AppRef};
use crate::auth::{Principal, Role};
use crate::deps::{self, Reference};
use crate::store;

fn describe(app: &AppRef, p: &Principal, id: &str) -> Value {
    match app.db.read(|c| store::get_stack(c, id)) {
        Ok(Some(s)) if app.authz.role(p, &s.space_id).is_some() => json!({"id": s.id, "name": s.name, "space": s.space}),
        _ => json!({"id": id, "hidden": true}),
    }
}

/// `GET /stacks/{id}/dependencies`: what the stack depends on and what depends on it.
pub async fn list(State(app): State<AppRef>, p: Principal, Path(id): Path<String>, Query(q): Query<SpaceQ>) -> ApiResult<Json<Value>> {
    let s = find_stack(&app, &p, &id, q.space.as_deref(), Role::Reader)?;
    let (up, down) = app.db.read(|c| Ok((deps::upstream(c, &s.id)?, deps::downstream(c, &s.id)?)))?;
    Ok(Json(json!({
        "depends_on": up.iter().map(|d| json!({"stack": describe(&app, &p, &d.depends_on), "references": d.references})).collect::<Vec<_>>(),
        "dependents": down.iter().map(|d| json!({"stack": describe(&app, &p, &d.stack_id), "references": d.references})).collect::<Vec<_>>(),
    })))
}

#[derive(Deserialize)]
pub struct DependReq {
    /// Upstream stack: ID or name.
    stack: String,
    #[serde(default)]
    space: Option<String>,
    /// Outputs of the upstream stack to pass in as variables.
    #[serde(default)]
    references: Vec<Reference>,
}

/// `POST /stacks/{id}/dependencies`: make the stack depend on another (or replace the references).
pub async fn add(
    State(app): State<AppRef>, p: Principal, Path(id): Path<String>, Query(q): Query<SpaceQ>, Json(r): Json<DependReq>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let s = find_stack(&app, &p, &id, q.space.as_deref(), Role::Admin)?;
    // Reading the upstream's outputs into this stack needs read access there.
    let up = find_stack(&app, &p, &r.stack, r.space.as_deref(), Role::Reader)?;
    app.db
        .write(|c| deps::set(c, &s.id, &up.id, &r.references))
        .map_err(|e| ApiError::bad_request(format!("{e:#}")))?;
    app.sched.kick();
    Ok((StatusCode::CREATED, Json(json!({"stack_id": s.id, "depends_on": up.id, "references": r.references}))))
}

pub async fn remove(
    State(app): State<AppRef>, p: Principal, Path((id, up)): Path<(String, String)>, Query(q): Query<SpaceQ>,
) -> ApiResult<StatusCode> {
    let s = find_stack(&app, &p, &id, q.space.as_deref(), Role::Admin)?;
    if !app.db.write(|c| deps::remove(c, &s.id, &up))? {
        return Err(ApiError::not_found("dependency"));
    }
    app.sched.kick();
    Ok(StatusCode::NO_CONTENT)
}
