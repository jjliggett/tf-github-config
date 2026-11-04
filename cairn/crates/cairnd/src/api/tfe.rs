//! Terraform Enterprise / Terraform Cloud compatible API subset (`/api/v2`).
//!
//! Organizations map to spaces and workspaces to stacks. Covered: what a
//! CLI-driven run needs (workspaces, configuration versions, runs, plans,
//! applies, state versions and outputs) plus workspace locking. Responses are
//! JSON:API. Permissions are computed once per request, never per row.

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::OnceLock;

use crate::api::native::{enqueue_run, find_run, save_config, valid_name};
use crate::app::{ApiError, ApiResult, AppRef};
use crate::store::{self, Run, Space, Stack};

const JSONAPI: &str = "application/vnd.api+json";

fn ok(status: StatusCode, body: Value) -> Response {
    (status, [(header::CONTENT_TYPE, JSONAPI)], body.to_string()).into_response()
}

fn attrs(body: &Value) -> &Value {
    &body["data"]["attributes"]
}

fn parse(body: &Bytes) -> ApiResult<Value> {
    if body.is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_slice(body).map_err(|e| ApiError::bad_request(format!("invalid JSON:API document: {e}")))
}

/// Admin token: everything is allowed. Built once per request.
fn permissions() -> Value {
    let t = true;
    json!({
        "can-update": t, "can-destroy": t, "can-queue-run": t, "can-read-variable": t,
        "can-update-variable": t, "can-read-state-versions": t, "can-read-state-outputs": t,
        "can-create-state-versions": t, "can-queue-apply": t, "can-lock": t, "can-unlock": t,
        "can-force-unlock": t, "can-read-settings": t, "can-queue-destroy": t,
    })
}

// ---------------------------------------------------------------- discovery

pub async fn well_known() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "application/json")],
        json!({"tfe.v2": "/api/v2/", "tfe.v2.1": "/api/v2/", "tfe.v2.2": "/api/v2/", "state.v2": "/api/v2/"})
            .to_string(),
    )
}

pub async fn ping() -> impl IntoResponse {
    (StatusCode::NO_CONTENT, [("TFP-API-Version", "2.5"), ("TFP-AppName", "Cairn")])
}

pub async fn account_details() -> Response {
    ok(
        StatusCode::OK,
        json!({"data": {"id": "user-cairn-admin", "type": "users",
               "attributes": {"username": "admin", "email": "", "is-service-account": false,
                               "permissions": {"can-create-organizations": true}}}}),
    )
}

// ---------------------------------------------------------------- organizations

fn org_json(s: &Space) -> Value {
    json!({"id": s.name, "type": "organizations",
           "attributes": {"name": s.name, "email": s.email, "created-at": s.created_at, "external-id": s.id,
                           "permissions": {"can-update": true, "can-destroy": true, "can-create-workspace": true}}})
}

pub async fn create_org(State(app): State<AppRef>, body: Bytes) -> ApiResult<Response> {
    let b = parse(&body)?;
    let name = attrs(&b)["name"].as_str().unwrap_or_default().to_string();
    valid_name(&name)?;
    let email = attrs(&b)["email"].as_str().unwrap_or_default().to_string();
    let s = app.db.write(|c| store::create_space(c, &name, &email))?;
    Ok(ok(StatusCode::CREATED, json!({"data": org_json(&s)})))
}

fn find_org(app: &AppRef, name: &str) -> ApiResult<Space> {
    app.db.read(|c| store::space_by_name(c, name))?.ok_or_else(|| ApiError::not_found("organization"))
}

pub async fn get_org(State(app): State<AppRef>, Path(org): Path<String>) -> ApiResult<Response> {
    Ok(ok(StatusCode::OK, json!({"data": org_json(&find_org(&app, &org)?)})))
}

pub async fn entitlements(State(app): State<AppRef>, Path(org): Path<String>) -> ApiResult<Response> {
    find_org(&app, &org)?;
    Ok(ok(
        StatusCode::OK,
        json!({"data": {"id": format!("org-{org}"), "type": "entitlement-sets",
               "attributes": {"operations": true, "state-storage": true, "teams": false, "vcs-integrations": false,
                               "private-module-registry": false, "sentinel": false, "agents": false, "sso": false}}}),
    ))
}

// ---------------------------------------------------------------- workspaces

// Workspaces are serialized through typed structs (no intermediate
// `serde_json::Value` trees): list responses are the hot path.

#[derive(Serialize)]
struct Ref<'a> {
    id: &'a str,
    #[serde(rename = "type")]
    kind: &'static str,
}

#[derive(Serialize)]
struct Rel<'a> {
    data: Option<Ref<'a>>,
}

#[derive(Serialize)]
#[serde(rename_all = "kebab-case")]
struct WsAttrs<'a> {
    name: &'a str,
    description: &'a str,
    auto_apply: bool,
    created_at: &'a str,
    updated_at: &'a str,
    locked: bool,
    execution_mode: &'static str,
    operations: bool,
    environment: &'static str,
    terraform_version: &'a str,
    working_directory: &'a str,
    tag_names: &'a [String],
    speculative_enabled: bool,
    structured_run_output_enabled: bool,
    queue_all_runs: bool,
    file_triggers_enabled: bool,
    global_remote_state: bool,
    trigger_prefixes: [&'static str; 0],
    trigger_patterns: [&'static str; 0],
    actions: &'static RawValue,
    permissions: &'static RawValue,
}

#[derive(Serialize)]
#[serde(rename_all = "kebab-case")]
struct WsRels<'a> {
    organization: Rel<'a>,
    current_run: Rel<'a>,
    current_state_version: Rel<'a>,
}

#[derive(Serialize)]
struct WsLinks {
    #[serde(rename = "self")]
    self_: String,
}

#[derive(Serialize)]
pub struct WsDoc<'a> {
    id: &'a str,
    #[serde(rename = "type")]
    kind: &'static str,
    attributes: WsAttrs<'a>,
    relationships: WsRels<'a>,
    links: WsLinks,
}

fn raw(s: &'static str) -> &'static RawValue {
    Box::leak(RawValue::from_string(s.to_string()).expect("valid JSON"))
}

/// Admin token: everything is allowed. Built once per process.
fn permissions_raw() -> &'static RawValue {
    static P: OnceLock<&'static RawValue> = OnceLock::new();
    P.get_or_init(|| raw(&*Box::leak(permissions().to_string().into_boxed_str())))
}

fn actions_raw() -> &'static RawValue {
    static A: OnceLock<&'static RawValue> = OnceLock::new();
    A.get_or_init(|| raw(r#"{"is-destroyable":true}"#))
}

fn ws_doc<'a>(app: &'a AppRef, s: &'a Stack) -> WsDoc<'a> {
    let rel = |id: &'a Option<String>, kind| Rel { data: id.as_deref().map(|id| Ref { id, kind }) };
    WsDoc {
        id: &s.id,
        kind: "workspaces",
        attributes: WsAttrs {
            name: &s.name,
            description: &s.description,
            auto_apply: s.auto_apply,
            created_at: &s.created_at,
            updated_at: &s.updated_at,
            locked: s.locked,
            execution_mode: "remote",
            operations: true,
            environment: "default",
            terraform_version: s.engine_version.as_deref().unwrap_or(&app.engines.default_version),
            working_directory: &s.project_root,
            tag_names: &s.labels,
            speculative_enabled: true,
            structured_run_output_enabled: true,
            queue_all_runs: false,
            file_triggers_enabled: false,
            global_remote_state: false,
            trigger_prefixes: [],
            trigger_patterns: [],
            actions: actions_raw(),
            permissions: permissions_raw(),
        },
        relationships: WsRels {
            organization: Rel { data: Some(Ref { id: &s.space, kind: "organizations" }) },
            current_run: rel(&s.latest_run_id, "runs"),
            current_state_version: rel(&s.current_state_id, "state-versions"),
        },
        links: WsLinks { self_: format!("/api/v2/organizations/{}/workspaces/{}", s.space, s.name) },
    }
}

fn ws_json(app: &AppRef, s: &Stack, _perms: &Value) -> Value {
    serde_json::to_value(ws_doc(app, s)).unwrap_or(Value::Null)
}

fn find_ws(app: &AppRef, id: &str) -> ApiResult<Stack> {
    app.db.read(|c| store::get_stack(c, id))?.ok_or_else(|| ApiError::not_found("workspace"))
}

pub async fn list_workspaces(
    State(app): State<AppRef>,
    Path(org): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> ApiResult<Response> {
    let size: i64 = q.get("page[size]").and_then(|v| v.parse().ok()).unwrap_or(20).clamp(1, 100);
    let page: i64 = q.get("page[number]").and_then(|v| v.parse().ok()).unwrap_or(1).max(1);
    let search = q.get("search[name]").map(String::as_str);
    let (stacks, total) = app.db.read(|c| store::list_stacks_page(c, &org, search, size, (page - 1) * size))?;
    if stacks.is_empty() && page == 1 {
        find_org(&app, &org)?; // 404 for unknown organizations
    }
    let pages = (total + size - 1) / size;
    #[derive(Serialize)]
    struct Page<'a> {
        data: Vec<WsDoc<'a>>,
        meta: Value,
    }
    let body = Page {
        data: stacks.iter().map(|s| ws_doc(&app, s)).collect(),
        meta: json!({"pagination": {
            "current-page": page, "page-size": size, "total-count": total, "total-pages": pages,
            "next-page": if page < pages { Some(page + 1) } else { None },
            "prev-page": if page > 1 { Some(page - 1) } else { None }}}),
    };
    let mut bytes = Vec::with_capacity(512 + body.data.len() * 1200);
    serde_json::to_writer(&mut bytes, &body).map_err(anyhow::Error::from)?;
    Ok((StatusCode::OK, [(header::CONTENT_TYPE, JSONAPI)], bytes).into_response())
}

pub async fn create_workspace(State(app): State<AppRef>, Path(org): Path<String>, body: Bytes) -> ApiResult<Response> {
    let b = parse(&body)?;
    let a = attrs(&b);
    let name = a["name"].as_str().unwrap_or_default().to_string();
    valid_name(&name)?;
    let space = find_org(&app, &org)?;
    let labels: Vec<String> = a["tag-names"]
        .as_array()
        .map(|v| v.iter().filter_map(|x| x.as_str().map(String::from)).collect())
        .unwrap_or_default();
    let id = app.db.write(|c| {
        store::create_stack(
            c,
            &store::NewStack {
                space_id: &space.id,
                name: &name,
                description: a["description"].as_str().unwrap_or(""),
                engine_version: a["terraform-version"].as_str(),
                auto_apply: a["auto-apply"].as_bool().unwrap_or(false),
                project_root: a["working-directory"].as_str().unwrap_or(""),
                labels: &labels,
            },
        )
    })?;
    Ok(ok(StatusCode::CREATED, json!({"data": ws_json(&app, &find_ws(&app, &id)?, &permissions())})))
}

pub async fn get_workspace_by_name(State(app): State<AppRef>, Path((org, name)): Path<(String, String)>) -> ApiResult<Response> {
    let s = app.db.read(|c| store::stack_by_name(c, &org, &name))?.ok_or_else(|| ApiError::not_found("workspace"))?;
    Ok(ok(StatusCode::OK, json!({"data": ws_json(&app, &s, &permissions())})))
}

pub async fn get_workspace(State(app): State<AppRef>, Path(id): Path<String>) -> ApiResult<Response> {
    Ok(ok(StatusCode::OK, json!({"data": ws_json(&app, &find_ws(&app, &id)?, &permissions())})))
}

pub async fn update_workspace(State(app): State<AppRef>, Path(id): Path<String>, body: Bytes) -> ApiResult<Response> {
    let s = find_ws(&app, &id)?;
    let b = parse(&body)?;
    let a = attrs(&b);
    app.db.write(|c| {
        store::update_stack(
            c,
            &s.id,
            &store::StackPatch {
                name: a["name"].as_str().map(String::from),
                description: a["description"].as_str().map(String::from),
                engine_version: a.get("terraform-version").map(|v| v.as_str().map(String::from)),
                auto_apply: a["auto-apply"].as_bool(),
                project_root: a["working-directory"].as_str().map(String::from),
                labels: None,
            },
        )
    })?;
    Ok(ok(StatusCode::OK, json!({"data": ws_json(&app, &find_ws(&app, &s.id)?, &permissions())})))
}

pub async fn workspace_action(
    State(app): State<AppRef>,
    Path((id, action)): Path<(String, String)>,
    body: Bytes,
) -> ApiResult<Response> {
    let s = find_ws(&app, &id)?;
    match action.as_str() {
        "lock" => {
            let reason = parse(&body)?["reason"].as_str().unwrap_or("").to_string();
            let lock_id = format!("manual-{}", crate::util::new_id("lck"));
            let info = json!({"ID": lock_id, "Who": "tfe api", "Info": reason, "Operation": "manual", "Created": crate::util::now()});
            if app.db.write(|c| store::try_lock_stack(c, &s.id, &lock_id, &info))?.is_err() {
                return Err(ApiError::conflict("workspace already locked"));
            }
        }
        "unlock" => {
            if s.lock_id.as_deref().map(|l| !l.starts_with("manual-")).unwrap_or(false) {
                return Err(ApiError::conflict("workspace is locked by a run; use force-unlock"));
            }
            app.db.write(|c| store::unlock_stack(c, &s.id, None))?;
        }
        "force-unlock" => {
            app.db.write(|c| store::unlock_stack(c, &s.id, None))?;
        }
        _ => return Err(ApiError::not_found("action")),
    }
    Ok(ok(StatusCode::OK, json!({"data": ws_json(&app, &find_ws(&app, &s.id)?, &permissions())})))
}

// ---------------------------------------------------------------- configuration versions

fn cv_json(app: &AppRef, headers: &HeaderMap, cv: &store::ConfigVersion) -> Value {
    let upload = format!(
        "{}/api/v2/configuration-versions/{}/upload?sig={}",
        app.base_url(headers),
        cv.id,
        app.sign(&cv.id)
    );
    json!({"id": cv.id, "type": "configuration-versions",
           "attributes": {"status": cv.status, "auto-queue-runs": cv.auto_queue, "speculative": cv.speculative,
                           "source": "tfe-api", "upload-url": upload},
           "relationships": {"workspace": {"data": {"id": cv.stack_id, "type": "workspaces"}}}})
}

pub async fn create_cv(State(app): State<AppRef>, Path(id): Path<String>, headers: HeaderMap, body: Bytes) -> ApiResult<Response> {
    let s = find_ws(&app, &id)?;
    let b = parse(&body)?;
    let a = attrs(&b);
    let cv = app.db.write(|c| {
        store::create_config(c, &s.id, a["auto-queue-runs"].as_bool().unwrap_or(true), a["speculative"].as_bool().unwrap_or(false))
    })?;
    Ok(ok(StatusCode::CREATED, json!({"data": cv_json(&app, &headers, &cv)})))
}

pub async fn get_cv(State(app): State<AppRef>, Path(id): Path<String>, headers: HeaderMap) -> ApiResult<Response> {
    let cv = app.db.read(|c| store::get_config(c, &id))?.ok_or_else(|| ApiError::not_found("configuration version"))?;
    Ok(ok(StatusCode::OK, json!({"data": cv_json(&app, &headers, &cv)})))
}

#[derive(Deserialize)]
pub struct SigQ {
    sig: String,
}

/// Pre-signed upload (clients send no token here, as with TFE's archivist URLs).
pub async fn upload_cv(State(app): State<AppRef>, Path(id): Path<String>, Query(q): Query<SigQ>, body: Bytes) -> ApiResult<StatusCode> {
    if !crate::util::ct_eq(app.sign(&id).as_bytes(), q.sig.as_bytes()) {
        return Err(ApiError::new(StatusCode::FORBIDDEN, "invalid upload signature"));
    }
    let cv = app.db.read(|c| store::get_config(c, &id))?.ok_or_else(|| ApiError::not_found("configuration version"))?;
    if cv.status != "pending" {
        return Err(ApiError::conflict("configuration already uploaded"));
    }
    save_config(&app, &cv.id, &body)?;
    if cv.auto_queue {
        let s = find_ws(&app, &cv.stack_id)?;
        let kind = if cv.speculative { "proposed" } else { "tracked" };
        enqueue_run(&app, &s, kind, &cv.id, s.auto_apply, "Queued by configuration upload", "tfe-api")?;
    }
    Ok(StatusCode::OK)
}

// ---------------------------------------------------------------- runs

fn suffix(id: &str) -> &str {
    id.split_once('-').map(|(_, s)| s).unwrap_or(id)
}

fn run_json(r: &Run) -> Value {
    let mut ts = serde_json::Map::new();
    if let Some(m) = r.status_times.as_object() {
        for (k, v) in m {
            ts.insert(format!("{}-at", k.replace('_', "-")), v.clone());
        }
    }
    let active = !r.is_terminal();
    json!({
        "id": r.id, "type": "runs",
        "attributes": {
            "status": r.status, "status-timestamps": ts, "created-at": r.created_at, "message": r.message,
            "has-changes": r.has_changes.unwrap_or(false), "is-destroy": false, "auto-apply": r.auto_apply,
            "plan-only": r.kind == "proposed", "source": "tfe-api", "trigger-reason": "manual",
            "actions": {"is-confirmable": r.status == "planned", "is-discardable": r.status == "planned",
                        "is-cancelable": active && r.status != "planned", "is-force-cancelable": false},
            "permissions": {"can-apply": true, "can-cancel": true, "can-discard": true, "can-force-execute": true},
        },
        "relationships": {
            "workspace": {"data": {"id": r.stack_id, "type": "workspaces"}},
            "configuration-version": {"data": r.config_id.as_ref().map(|c| json!({"id": c, "type": "configuration-versions"}))},
            "plan": {"data": {"id": format!("plan-{}", suffix(&r.id)), "type": "plans"}},
            "apply": {"data": {"id": format!("apply-{}", suffix(&r.id)), "type": "applies"}},
        },
    })
}

pub async fn create_run(State(app): State<AppRef>, body: Bytes) -> ApiResult<Response> {
    let b = parse(&body)?;
    let a = attrs(&b);
    let rel = &b["data"]["relationships"];
    let ws = rel["workspace"]["data"]["id"].as_str().ok_or_else(|| ApiError::bad_request("workspace relationship required"))?;
    let s = find_ws(&app, ws)?;
    let cv_id = match rel["configuration-version"]["data"]["id"].as_str() {
        Some(c) => c.to_string(),
        None => app
            .db
            .read(|c| store::latest_uploaded_config(c, &s.id))?
            .ok_or_else(|| ApiError::bad_request("workspace has no uploaded configuration version"))?,
    };
    let cv = app.db.read(|c| store::get_config(c, &cv_id))?.ok_or_else(|| ApiError::not_found("configuration version"))?;
    if cv.stack_id != s.id || cv.status != "uploaded" {
        return Err(ApiError::bad_request("configuration version is not uploaded for this workspace"));
    }
    if a["is-destroy"].as_bool() == Some(true) {
        return Err(ApiError::new(StatusCode::NOT_IMPLEMENTED, "destroy runs are not supported yet"));
    }
    let kind = if a["plan-only"].as_bool().unwrap_or(cv.speculative) { "proposed" } else { "tracked" };
    let auto = a["auto-apply"].as_bool().unwrap_or(s.auto_apply);
    let run = enqueue_run(&app, &s, kind, &cv.id, auto, a["message"].as_str().unwrap_or(""), "tfe-api")?;
    Ok(ok(StatusCode::CREATED, json!({"data": run_json(&run)})))
}

pub async fn get_run(State(app): State<AppRef>, Path(id): Path<String>) -> ApiResult<Response> {
    Ok(ok(StatusCode::OK, json!({"data": run_json(&find_run(&app, &id)?)})))
}

pub async fn list_ws_runs(State(app): State<AppRef>, Path(id): Path<String>, Query(q): Query<HashMap<String, String>>) -> ApiResult<Response> {
    let s = find_ws(&app, &id)?;
    let size: i64 = q.get("page[size]").and_then(|v| v.parse().ok()).unwrap_or(20).clamp(1, 100);
    let runs = app.db.read(|c| store::list_runs(c, &s.id, i64::MAX, size))?;
    let data: Vec<Value> = runs.iter().map(run_json).collect();
    Ok(ok(StatusCode::OK, json!({"data": data})))
}

pub async fn run_action(
    State(app): State<AppRef>,
    Path((id, action)): Path<(String, String)>,
) -> ApiResult<StatusCode> {
    use crate::api::native as n;
    let p = Path(id);
    match action.as_str() {
        "apply" => n::confirm_run(State(app), p).await.map(|_| ()),
        "discard" => n::discard_run(State(app), p).await.map(|_| ()),
        "cancel" | "force-cancel" => n::cancel_run(State(app), p).await.map(|_| ()),
        _ => Err(ApiError::not_found("action")),
    }?;
    Ok(StatusCode::ACCEPTED)
}

// ---------------------------------------------------------------- plans & applies

fn run_for_phase(app: &AppRef, id: &str) -> ApiResult<Run> {
    let (_, s) = id.split_once('-').ok_or_else(|| ApiError::not_found("plan"))?;
    find_run(app, &format!("run-{s}"))
}

fn phase_status(r: &Run, apply: bool) -> &'static str {
    match (apply, r.status.as_str()) {
        (false, "plan_queued") => "queued",
        (false, "planning") => "running",
        (false, "errored") if r.has_changes.is_none() => "errored",
        (false, "canceled") if r.has_changes.is_none() => "canceled",
        (false, _) => "finished",
        (true, "apply_queued") => "queued",
        (true, "applying") => "running",
        (true, "applied") => "finished",
        (true, "errored") => "errored",
        (true, "canceled") => "canceled",
        (true, _) => "pending",
    }
}

pub async fn get_plan(State(app): State<AppRef>, Path(id): Path<String>, headers: HeaderMap) -> ApiResult<Response> {
    let r = run_for_phase(&app, &id)?;
    Ok(ok(
        StatusCode::OK,
        json!({"data": {"id": id, "type": "plans", "attributes": {
            "status": phase_status(&r, false), "has-changes": r.has_changes.unwrap_or(false),
            "resource-additions": r.adds.unwrap_or(0), "resource-changes": r.changes.unwrap_or(0),
            "resource-destructions": r.destroys.unwrap_or(0),
            "log-read-url": format!("{}/api/v2/plans/{}/logs", app.base_url(&headers), id)}}}),
    ))
}

pub async fn get_apply(State(app): State<AppRef>, Path(id): Path<String>, headers: HeaderMap) -> ApiResult<Response> {
    let r = run_for_phase(&app, &id)?;
    Ok(ok(
        StatusCode::OK,
        json!({"data": {"id": id, "type": "applies", "attributes": {
            "status": phase_status(&r, true),
            "resource-additions": r.adds.unwrap_or(0), "resource-changes": r.changes.unwrap_or(0),
            "resource-destructions": r.destroys.unwrap_or(0),
            "log-read-url": format!("{}/api/v2/applies/{}/logs", app.base_url(&headers), id)}}}),
    ))
}

pub async fn phase_logs(State(app): State<AppRef>, Path(id): Path<String>) -> ApiResult<Response> {
    let r = run_for_phase(&app, &id)?;
    let bytes = tokio::fs::read(app.hub.log_path(&r.id)).await.unwrap_or_default();
    Ok(([(header::CONTENT_TYPE, "text/plain")], bytes).into_response())
}

// ---------------------------------------------------------------- state versions

fn sv_json(app: &AppRef, headers: &HeaderMap, sv: &store::StateVersion) -> Value {
    json!({"id": sv.id, "type": "state-versions",
           "attributes": {"serial": sv.serial, "created-at": sv.created_at, "size": sv.size,
                           "resources-processed": true, "status": "finalized",
                           "hosted-state-download-url": format!("{}/api/v2/state-versions/{}/download", app.base_url(headers), sv.id)},
           "relationships": {"run": {"data": sv.run_id.as_ref().map(|r| json!({"id": r, "type": "runs"}))}}})
}

pub async fn current_state(State(app): State<AppRef>, Path(id): Path<String>, headers: HeaderMap) -> ApiResult<Response> {
    let s = find_ws(&app, &id)?;
    let sv = app.db.read(|c| store::current_state(c, &s.id))?.ok_or_else(|| ApiError::not_found("state version"))?;
    Ok(ok(StatusCode::OK, json!({"data": sv_json(&app, &headers, &sv)})))
}

pub async fn current_outputs(State(app): State<AppRef>, Path(id): Path<String>) -> ApiResult<Response> {
    let s = find_ws(&app, &id)?;
    let sv = app.db.read(|c| store::current_state(c, &s.id))?;
    let mut data = vec![];
    if let Some(sv) = sv {
        if let Some(m) = sv.outputs.as_object() {
            for (name, o) in m {
                let sensitive = o["sensitive"].as_bool().unwrap_or(false);
                data.push(json!({"id": format!("wsout-{}-{name}", suffix(&sv.id)), "type": "state-version-outputs",
                                 "attributes": {"name": name, "sensitive": sensitive, "type": o["type"],
                                                "value": if sensitive { Value::Null } else { o["value"].clone() }}}));
            }
        }
    }
    Ok(ok(StatusCode::OK, json!({"data": data})))
}

pub async fn download_state(State(app): State<AppRef>, Path(id): Path<String>) -> ApiResult<Response> {
    let sv = app.db.read(|c| store::get_state(c, &id))?.ok_or_else(|| ApiError::not_found("state version"))?;
    let bytes = tokio::fs::read(app.dir("blobs/state").join(format!("{}.json", sv.sha256)))
        .await
        .map_err(anyhow::Error::from)?;
    Ok(([(header::CONTENT_TYPE, "application/json")], bytes).into_response())
}
