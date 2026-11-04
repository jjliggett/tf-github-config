//! Policy, review and webhook endpoints.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::api::identity::find_space;
use crate::api::native::{authorized_run, find_stack, run_view, valid_name, RunView, SpaceQ};
use crate::app::{ApiError, ApiResult, AppRef};
use crate::auth::{Principal, Role};
use crate::policy::{self, Policy, KINDS};
use crate::{runner, store};

fn find_policy(app: &AppRef, p: &Principal, key: &str, need: Role) -> ApiResult<Policy> {
    let pol = app.db.read(|c| policy::get(c, key))?.ok_or_else(|| ApiError::not_found("policy"))?;
    if pol.kind == "login" && need > Role::Reader {
        app.authz.require_site_admin(p)?;
    }
    app.authz.require(p, &pol.space_id, need)?;
    Ok(pol)
}

#[derive(Deserialize)]
pub struct ListQ {
    kind: Option<String>,
}

pub async fn list(State(app): State<AppRef>, p: Principal, Query(q): Query<ListQ>) -> ApiResult<Json<Value>> {
    let allowed = app.authz.readable_spaces(&p);
    let items: Vec<Policy> = app
        .db
        .read(|c| policy::list(c, q.kind.as_deref()))?
        .into_iter()
        .filter(|pol| allowed.as_ref().map(|a| a.contains(&pol.space_id)).unwrap_or(true))
        .collect();
    Ok(Json(json!({"items": items})))
}

#[derive(Deserialize)]
pub struct NewPolicy {
    name: String,
    kind: String,
    body: String,
    #[serde(default)]
    space: Option<String>,
    #[serde(default)]
    labels: Vec<String>,
    #[serde(default)]
    description: String,
}

pub async fn create(State(app): State<AppRef>, p: Principal, Json(r): Json<NewPolicy>) -> ApiResult<impl IntoResponse> {
    valid_name(&r.name)?;
    if !KINDS.contains(&r.kind.as_str()) {
        return Err(ApiError::bad_request(format!("kind must be one of {}", KINDS.join(", "))));
    }
    let space = find_space(&app, r.space.as_deref().unwrap_or(crate::api::native::DEFAULT_SPACE))?;
    app.authz.require(&p, &space.id, Role::Admin)?;
    if r.kind == "login" {
        app.authz.require_site_admin(&p)?;
    }
    policy::compile(&r.name, &r.body).map_err(|e| ApiError::bad_request(format!("invalid policy: {e:#}")))?;
    let pol = app.db.write(|c| {
        policy::create(
            c,
            &policy::NewPolicy { space_id: &space.id, name: &r.name, kind: &r.kind, body: &r.body, labels: &r.labels, description: &r.description },
        )
    })?;
    if pol.kind == "login" {
        app.jwt_cache.lock().unwrap().clear();
    }
    Ok((StatusCode::CREATED, Json(pol)))
}

pub async fn get(State(app): State<AppRef>, p: Principal, Path(id): Path<String>) -> ApiResult<Json<Policy>> {
    Ok(Json(find_policy(&app, &p, &id, Role::Reader)?))
}

#[derive(Deserialize)]
pub struct PatchPolicy {
    body: Option<String>,
    labels: Option<Vec<String>>,
    description: Option<String>,
}

pub async fn patch(State(app): State<AppRef>, p: Principal, Path(id): Path<String>, Json(r): Json<PatchPolicy>) -> ApiResult<Json<Policy>> {
    let pol = find_policy(&app, &p, &id, Role::Admin)?;
    if let Some(b) = &r.body {
        policy::compile(&pol.name, b).map_err(|e| ApiError::bad_request(format!("invalid policy: {e:#}")))?;
    }
    app.db.write(|c| policy::update(c, &pol.id, r.body.as_deref(), r.labels.as_deref(), r.description.as_deref()))?;
    if pol.kind == "login" {
        app.jwt_cache.lock().unwrap().clear();
    }
    Ok(Json(app.db.read(|c| policy::get(c, &pol.id))?.expect("exists")))
}

pub async fn delete(State(app): State<AppRef>, p: Principal, Path(id): Path<String>) -> ApiResult<StatusCode> {
    let pol = find_policy(&app, &p, &id, Role::Admin)?;
    app.db.write(|c| policy::delete(c, &pol.id))?;
    if pol.kind == "login" {
        app.jwt_cache.lock().unwrap().clear();
    }
    Ok(StatusCode::NO_CONTENT)
}

pub async fn attach(State(app): State<AppRef>, p: Principal, Path((id, stack)): Path<(String, String)>, Query(q): Query<SpaceQ>) -> ApiResult<StatusCode> {
    let pol = find_policy(&app, &p, &id, Role::Admin)?;
    let s = find_stack(&app, &p, &stack, q.space.as_deref(), Role::Admin)?;
    app.db.write(|c| policy::attach(c, &pol.id, &s.id))?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn detach(State(app): State<AppRef>, p: Principal, Path((id, stack)): Path<(String, String)>, Query(q): Query<SpaceQ>) -> ApiResult<StatusCode> {
    let pol = find_policy(&app, &p, &id, Role::Admin)?;
    let s = find_stack(&app, &p, &stack, q.space.as_deref(), Role::Admin)?;
    app.db.write(|c| policy::detach(c, &pol.id, &s.id))?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct EvalReq {
    kind: Option<String>,
    body: Option<String>,
    #[serde(default)]
    input: Value,
}

/// Dry-run a policy: a stored one (`/policies/{id}/evaluate`) or an ad-hoc body.
pub async fn evaluate_adhoc(State(_app): State<AppRef>, _p: Principal, Json(r): Json<EvalReq>) -> ApiResult<Json<Value>> {
    let kind = r.kind.as_deref().ok_or_else(|| ApiError::bad_request("kind is required"))?;
    let body = r.body.as_deref().ok_or_else(|| ApiError::bad_request("body is required"))?;
    let e = policy::compile("adhoc", body).map_err(|e| ApiError::bad_request(format!("invalid policy: {e:#}")))?;
    let result = policy::evaluate(&e, kind, &r.input).map_err(|e| ApiError::bad_request(format!("{e:#}")))?;
    Ok(Json(json!({"kind": kind, "result": result})))
}

pub async fn evaluate_stored(State(app): State<AppRef>, p: Principal, Path(id): Path<String>, Json(r): Json<EvalReq>) -> ApiResult<Json<Value>> {
    let pol = find_policy(&app, &p, &id, Role::Reader)?;
    let v = app.policies.run(std::slice::from_ref(&pol), &r.input).map_err(|e| ApiError::bad_request(format!("{e:#}")))?;
    Ok(Json(json!({"kind": pol.kind, "result": v[0].result})))
}

pub async fn stack_policies(State(app): State<AppRef>, p: Principal, Path(id): Path<String>, Query(q): Query<SpaceQ>) -> ApiResult<Json<Value>> {
    let s = find_stack(&app, &p, &id, q.space.as_deref(), Role::Reader)?;
    let chain = app.space_chain(&s.space_id);
    let mut items = vec![];
    for kind in KINDS {
        items.extend(app.db.read(|c| policy::for_stack(c, kind, &s.id, &chain, &s.labels))?);
    }
    Ok(Json(json!({"items": items})))
}

// ---------------------------------------------------------------- reviews

pub async fn run_policies(State(app): State<AppRef>, p: Principal, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let run = authorized_run(&app, &p, &id, Role::Reader)?;
    let (evals, reviews) = app.db.read(|c| Ok((policy::evaluations(c, &run.id)?, policy::reviews(c, &run.id)?)))?;
    Ok(Json(json!({"run_id": run.id, "approval": run.approval, "evaluations": evals, "reviews": reviews})))
}

#[derive(Deserialize)]
pub struct ReviewReq {
    decision: String,
    #[serde(default)]
    note: String,
}

/// Approve or reject a planned run; approval policies decide what it takes.
pub async fn review(State(app): State<AppRef>, p: Principal, Path(id): Path<String>, Json(r): Json<ReviewReq>) -> ApiResult<Json<RunView>> {
    let run = authorized_run(&app, &p, &id, Role::Reader)?;
    if !matches!(r.decision.as_str(), "approve" | "reject") {
        return Err(ApiError::bad_request("decision must be approve or reject"));
    }
    if run.status != "planned" {
        return Err(ApiError::conflict(format!("run is {}; only planned runs can be reviewed", run.status)));
    }
    app.db.write(|c| policy::add_review(c, &run.id, &p.label(), &r.decision, &r.note))?;
    runner::evaluate_approval(&app, &run.id).await?;
    let run = crate::api::native::find_run(&app, &run.id)?;
    Ok(Json(run_view(run)))
}

// ---------------------------------------------------------------- notification webhooks

pub async fn list_webhooks(State(app): State<AppRef>, p: Principal, Path(space): Path<String>) -> ApiResult<Json<Value>> {
    let s = find_space(&app, &space)?;
    app.authz.require(&p, &s.id, Role::Admin)?;
    let items = app.db.read(|c| crate::notify::list(c, &s.id))?;
    Ok(Json(json!({"items": items})))
}

#[derive(Deserialize)]
pub struct WebhookReq {
    url: String,
    #[serde(default)]
    secret: String,
}

pub async fn create_webhook(State(app): State<AppRef>, p: Principal, Path(space): Path<String>, Json(r): Json<WebhookReq>) -> ApiResult<impl IntoResponse> {
    let s = find_space(&app, &space)?;
    app.authz.require(&p, &s.id, Role::Admin)?;
    crate::notify::check_url(&r.url, app.cfg.allow_private_webhooks).map_err(|e| ApiError::bad_request(format!("{e:#}")))?;
    let secret = if r.secret.is_empty() { String::new() } else { app.secrets.seal(&r.secret) };
    let id = app.db.write(|c| crate::notify::create(c, &s.id, &r.url, &secret))?;
    Ok((StatusCode::CREATED, Json(json!({"id": id, "space_id": s.id, "url": r.url}))))
}

pub async fn delete_webhook(State(app): State<AppRef>, p: Principal, Path((space, id)): Path<(String, String)>) -> ApiResult<StatusCode> {
    let s = find_space(&app, &space)?;
    app.authz.require(&p, &s.id, Role::Admin)?;
    if !app.db.write(|c| crate::notify::delete(c, &s.id, &id))? {
        return Err(ApiError::not_found("webhook"));
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Used by the runner when a confirm request arrives: is it allowed by approval policies?
pub fn confirm_allowed(app: &AppRef, run: &store::Run) -> ApiResult<()> {
    match run.approval.as_deref() {
        Some("pending") => Err(ApiError::conflict("run needs approval first (see approval policies; review with POST /runs/{id}/reviews)")),
        Some("rejected") => Err(ApiError::conflict("run was rejected by an approval policy")),
        _ => {
            let _ = app;
            Ok(())
        }
    }
}
