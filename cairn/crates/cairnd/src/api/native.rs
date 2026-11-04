//! Native JSON API (`/api/v1`): what the CLI and MCP server use.

use std::convert::Infallible;
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;

use crate::app::{ApiError, ApiResult, AppRef};
use crate::events::{Follow, LogMsg};
use crate::runner;
use crate::store::{self, Run, Stack};
use crate::util::{new_token, sha256_hex};

pub const DEFAULT_SPACE: &str = "default";

// ---------------------------------------------------------------- helpers

/// Resolve `{stack}` path segments: an ID (`stk-…`) or a name in `?space=` (default space otherwise).
pub fn find_stack(app: &AppRef, key: &str, space: Option<&str>) -> ApiResult<Stack> {
    let s = if key.starts_with("stk-") {
        app.db.read(|c| store::get_stack(c, key))?
    } else {
        app.db.read(|c| store::stack_by_name(c, space.unwrap_or(DEFAULT_SPACE), key))?
    };
    s.ok_or_else(|| ApiError::not_found("stack"))
}

pub fn find_run(app: &AppRef, id: &str) -> ApiResult<Run> {
    app.db.read(|c| store::get_run(c, id))?.ok_or_else(|| ApiError::not_found("run"))
}

#[derive(Serialize)]
pub struct RunView {
    #[serde(flatten)]
    pub run: Run,
    /// Operations valid right now, with the CLI command for each.
    pub next_actions: Vec<Value>,
}

pub fn run_view(run: Run) -> RunView {
    let id = run.id.clone();
    let mut next = vec![];
    match run.status.as_str() {
        "planned" => {
            next.push(json!({"action": "confirm", "cli": format!("cairn run confirm {id}")}));
            next.push(json!({"action": "discard", "cli": format!("cairn run discard {id}")}));
        }
        "plan_queued" | "planning" | "apply_queued" | "applying" => {
            next.push(json!({"action": "cancel", "cli": format!("cairn run cancel {id}")}));
        }
        _ => {}
    }
    RunView { run, next_actions: next }
}

#[derive(Deserialize)]
pub struct SpaceQ {
    pub space: Option<String>,
}

// ---------------------------------------------------------------- misc

pub async fn whoami() -> Json<Value> {
    Json(json!({"subject": "token", "role": "admin"}))
}

#[derive(Deserialize)]
pub struct NewTokenReq {
    #[serde(default)]
    description: String,
}

pub async fn create_token(State(app): State<AppRef>, Json(req): Json<NewTokenReq>) -> ApiResult<impl IntoResponse> {
    let token = new_token();
    let sha = sha256_hex(token.as_bytes());
    let id = app.db.write(|c| store::insert_token(c, &sha, &req.description))?;
    app.tokens.write().unwrap().insert(sha);
    Ok((StatusCode::CREATED, Json(json!({"id": id, "token": token, "description": req.description}))))
}

pub async fn metrics(State(app): State<AppRef>) -> impl IntoResponse {
    use std::fmt::Write;
    use std::sync::atomic::Ordering;
    let m = &app.metrics;
    let mut s = String::new();
    let queued = app.db.read(store::queued_runs).map(|q| q.len()).unwrap_or(0);
    let _ = writeln!(s, "# TYPE cairn_runs_created_total counter\ncairn_runs_created_total {}", m.runs_created.load(Ordering::Relaxed));
    let _ = writeln!(s, "# TYPE cairn_http_requests_total counter\ncairn_http_requests_total {}", m.http_requests.load(Ordering::Relaxed));
    let _ = writeln!(s, "# TYPE cairn_queue_depth gauge\ncairn_queue_depth {queued}");
    let _ = writeln!(s, "# TYPE cairn_runner_active gauge\ncairn_runner_active {}", app.sched.active());
    let _ = writeln!(s, "# TYPE cairn_runner_slots gauge\ncairn_runner_slots {}", app.sched.concurrency);
    let _ = writeln!(s, "# TYPE cairn_runs_finished_total counter");
    for (status, n) in m.finished.lock().unwrap().iter() {
        let _ = writeln!(s, "cairn_runs_finished_total{{status=\"{status}\"}} {n}");
    }
    let _ = writeln!(s, "# TYPE cairn_phase_seconds summary");
    for (phase, (n, sum)) in m.phase_runs.lock().unwrap().iter() {
        let _ = writeln!(s, "cairn_phase_seconds_count{{phase=\"{phase}\"}} {n}\ncairn_phase_seconds_sum{{phase=\"{phase}\"}} {sum:.3}");
    }
    ([(header::CONTENT_TYPE, "text/plain; version=0.0.4")], s)
}

// ---------------------------------------------------------------- spaces

pub async fn list_spaces(State(app): State<AppRef>) -> ApiResult<Json<Value>> {
    let items = app.db.read(store::list_spaces)?;
    Ok(Json(json!({ "items": items })))
}

#[derive(Deserialize)]
pub struct NewSpace {
    name: String,
    #[serde(default)]
    email: String,
}

pub async fn create_space(State(app): State<AppRef>, Json(req): Json<NewSpace>) -> ApiResult<impl IntoResponse> {
    valid_name(&req.name)?;
    let s = app.db.write(|c| store::create_space(c, &req.name, &req.email))?;
    Ok((StatusCode::CREATED, Json(s)))
}

pub fn valid_name(n: &str) -> ApiResult<()> {
    if n.is_empty() || n.len() > 90 || !n.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.') {
        return Err(ApiError::bad_request("names may contain only letters, digits, '-', '_' and '.' (1-90 chars)"));
    }
    Ok(())
}

// ---------------------------------------------------------------- stacks

#[derive(Deserialize)]
pub struct ListQ {
    space: Option<String>,
    limit: Option<i64>,
    after: Option<String>,
}

#[derive(Serialize)]
struct Page<T: Serialize, N: Serialize> {
    items: Vec<T>,
    next: Option<N>,
}

/// Serialize straight into a pre-sized buffer (no intermediate `Value` tree).
fn json_bytes<T: Serialize>(v: &T, capacity: usize) -> Response {
    let mut buf = Vec::with_capacity(capacity);
    match serde_json::to_writer(&mut buf, v) {
        Ok(()) => ([(header::CONTENT_TYPE, "application/json")], buf).into_response(),
        Err(e) => ApiError::from(anyhow::Error::from(e)).into_response(),
    }
}

pub async fn list_stacks(State(app): State<AppRef>, Query(q): Query<ListQ>) -> ApiResult<Response> {
    let limit = q.limit.unwrap_or(50).clamp(1, 200);
    let items = app
        .db
        .read(|c| store::list_stacks(c, q.space.as_deref(), q.after.as_deref().unwrap_or(""), limit))?;
    let next = (items.len() as i64 == limit).then(|| items.last().map(|s| format!("{}/{}", s.space, s.name))).flatten();
    Ok(json_bytes(&Page { items, next }, 512 + limit as usize * 600))
}

#[derive(Deserialize)]
pub struct NewStackReq {
    #[serde(default)]
    space: Option<String>,
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    engine_version: Option<String>,
    #[serde(default)]
    auto_apply: bool,
    #[serde(default)]
    project_root: String,
    #[serde(default)]
    labels: Vec<String>,
}

pub async fn create_stack(State(app): State<AppRef>, Json(r): Json<NewStackReq>) -> ApiResult<impl IntoResponse> {
    valid_name(&r.name)?;
    let space_name = r.space.as_deref().unwrap_or(DEFAULT_SPACE);
    let id = app.db.write(|c| {
        let space = match store::space_by_name(c, space_name)? {
            Some(s) => s,
            None if space_name == DEFAULT_SPACE => store::create_space(c, DEFAULT_SPACE, "")?,
            None => anyhow::bail!("space {space_name} not found"),
        };
        store::create_stack(
            c,
            &store::NewStack {
                space_id: &space.id,
                name: &r.name,
                description: &r.description,
                engine_version: r.engine_version.as_deref(),
                auto_apply: r.auto_apply,
                project_root: &r.project_root,
                labels: &r.labels,
            },
        )
    })?;
    let s = find_stack(&app, &id, None)?;
    Ok((StatusCode::CREATED, Json(s)))
}

pub async fn get_stack(State(app): State<AppRef>, Path(id): Path<String>, Query(q): Query<SpaceQ>) -> ApiResult<Json<Stack>> {
    Ok(Json(find_stack(&app, &id, q.space.as_deref())?))
}

#[derive(Deserialize)]
pub struct PatchStackReq {
    name: Option<String>,
    description: Option<String>,
    #[serde(default, with = "double_option")]
    engine_version: Option<Option<String>>,
    auto_apply: Option<bool>,
    project_root: Option<String>,
    labels: Option<Vec<String>>,
}

mod double_option {
    use serde::{Deserialize, Deserializer};
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Option<String>>, D::Error> {
        Ok(Some(Option::deserialize(d)?))
    }
}

pub async fn patch_stack(
    State(app): State<AppRef>,
    Path(id): Path<String>,
    Query(q): Query<SpaceQ>,
    Json(p): Json<PatchStackReq>,
) -> ApiResult<Json<Stack>> {
    let s = find_stack(&app, &id, q.space.as_deref())?;
    if let Some(n) = &p.name {
        valid_name(n)?;
    }
    app.db.write(|c| {
        store::update_stack(
            c,
            &s.id,
            &store::StackPatch {
                name: p.name,
                description: p.description,
                engine_version: p.engine_version,
                auto_apply: p.auto_apply,
                project_root: p.project_root,
                labels: p.labels,
            },
        )
    })?;
    Ok(Json(find_stack(&app, &s.id, None)?))
}

pub async fn delete_stack(State(app): State<AppRef>, Path(id): Path<String>, Query(q): Query<SpaceQ>) -> ApiResult<StatusCode> {
    let s = find_stack(&app, &id, q.space.as_deref())?;
    let busy = app.db.read(store::active_runs)?.into_iter().any(|r| r.stack_id == s.id);
    if busy {
        return Err(ApiError::conflict("stack has active runs; cancel or discard them first"));
    }
    app.db.write(|c| store::delete_stack(c, &s.id))?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize, Default)]
pub struct LockReq {
    #[serde(default)]
    note: String,
}

pub async fn lock_stack(
    State(app): State<AppRef>,
    Path(id): Path<String>,
    Query(q): Query<SpaceQ>,
    body: Option<Json<LockReq>>,
) -> ApiResult<Json<Stack>> {
    let s = find_stack(&app, &id, q.space.as_deref())?;
    let note = body.map(|b| b.0.note).unwrap_or_default();
    let lock_id = format!("manual-{}", crate::util::new_id("lck"));
    let info = json!({"ID": lock_id, "Who": "cairn user", "Info": note, "Operation": "manual", "Created": crate::util::now()});
    if let Err(h) = app.db.write(|c| store::try_lock_stack(c, &s.id, &lock_id, &info))? {
        let mut e = ApiError::conflict("stack is already locked");
        e.detail = Some(h);
        return Err(e);
    }
    Ok(Json(find_stack(&app, &s.id, None)?))
}

#[derive(Deserialize)]
pub struct UnlockQ {
    space: Option<String>,
    #[serde(default)]
    force: bool,
}

pub async fn unlock_stack(State(app): State<AppRef>, Path(id): Path<String>, Query(q): Query<UnlockQ>) -> ApiResult<Json<Stack>> {
    let s = find_stack(&app, &id, q.space.as_deref())?;
    let manual = s.lock_id.as_deref().map(|l| l.starts_with("manual-")).unwrap_or(true);
    if !manual && !q.force {
        return Err(ApiError::conflict("stack is locked by a run or a tofu client; pass force=true to override"));
    }
    app.db.write(|c| store::unlock_stack(c, &s.id, None))?;
    Ok(Json(find_stack(&app, &s.id, None)?))
}

// ---------------------------------------------------------------- variables

pub async fn list_vars(State(app): State<AppRef>, Path(id): Path<String>, Query(q): Query<SpaceQ>) -> ApiResult<Json<Value>> {
    let s = find_stack(&app, &id, q.space.as_deref())?;
    let mut vars = app.db.read(|c| store::list_variables(c, &s.id))?;
    for v in &mut vars {
        if v.sensitive {
            v.value.clear();
        }
    }
    Ok(Json(json!({ "items": vars })))
}

#[derive(Deserialize)]
pub struct VarReq {
    key: String,
    value: String,
    #[serde(default = "default_category")]
    category: String,
    #[serde(default)]
    sensitive: bool,
    #[serde(default)]
    hcl: bool,
}

fn default_category() -> String {
    "terraform".into()
}

pub async fn set_var(
    State(app): State<AppRef>,
    Path(id): Path<String>,
    Query(q): Query<SpaceQ>,
    Json(r): Json<VarReq>,
) -> ApiResult<impl IntoResponse> {
    let s = find_stack(&app, &id, q.space.as_deref())?;
    if !matches!(r.category.as_str(), "terraform" | "env") {
        return Err(ApiError::bad_request("category must be 'terraform' or 'env'"));
    }
    if r.key.is_empty() || !r.key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(ApiError::bad_request("variable keys may contain only letters, digits and '_'"));
    }
    let value = if r.sensitive { app.secrets.seal(&r.value) } else { r.value.clone() };
    let v = store::Variable { id: String::new(), key: r.key, value, category: r.category, sensitive: r.sensitive, hcl: r.hcl };
    let vid = app.db.write(|c| store::upsert_variable(c, &s.id, &v))?;
    let shown = if v.sensitive { String::new() } else { v.value.clone() };
    Ok((StatusCode::CREATED, Json(store::Variable { id: vid, value: shown, ..v })))
}

#[derive(Deserialize)]
pub struct DelVarQ {
    space: Option<String>,
    category: Option<String>,
}

pub async fn delete_var(
    State(app): State<AppRef>,
    Path((id, key)): Path<(String, String)>,
    Query(q): Query<DelVarQ>,
) -> ApiResult<StatusCode> {
    let s = find_stack(&app, &id, q.space.as_deref())?;
    if app.db.write(|c| store::delete_variable(c, &s.id, &key, q.category.as_deref()))? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::not_found("variable"))
    }
}

// ---------------------------------------------------------------- configs & runs

/// Store an uploaded configuration tarball. Validates gzip+tar framing cheaply.
pub fn save_config(app: &AppRef, cv_id: &str, body: &Bytes) -> ApiResult<()> {
    if body.len() < 2 || body[0] != 0x1f || body[1] != 0x8b {
        return Err(ApiError::bad_request("configuration must be a .tar.gz archive"));
    }
    if body.len() > 100 << 20 {
        return Err(ApiError::new(StatusCode::PAYLOAD_TOO_LARGE, "configuration larger than 100 MiB"));
    }
    let path = app.dir("blobs/config").join(format!("{cv_id}.tar.gz"));
    std::fs::write(&path, body).map_err(anyhow::Error::from)?;
    let sha = sha256_hex(body);
    app.db.write(|c| store::mark_config_uploaded(c, cv_id, &sha, body.len() as i64))?;
    Ok(())
}

pub fn enqueue_run(app: &AppRef, stack: &Stack, kind: &str, config_id: &str, auto_apply: bool, message: &str, by: &str) -> ApiResult<Run> {
    let run = app.db.write(|c| {
        store::create_run(
            c,
            &store::NewRun { stack_id: &stack.id, kind, config_id: Some(config_id), auto_apply, message, triggered_by: by },
        )
    })?;
    app.metrics.runs_created.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    runner::publish(app, &stack.id, &run.id, "plan_queued");
    app.sched.kick();
    Ok(run)
}

pub async fn upload_config(
    State(app): State<AppRef>,
    Path(id): Path<String>,
    Query(q): Query<SpaceQ>,
    body: Bytes,
) -> ApiResult<impl IntoResponse> {
    let s = find_stack(&app, &id, q.space.as_deref())?;
    let cv = app.db.write(|c| store::create_config(c, &s.id, false, false))?;
    save_config(&app, &cv.id, &body)?;
    let cv = app.db.read(|c| store::get_config(c, &cv.id))?;
    Ok((StatusCode::CREATED, Json(cv)))
}

#[derive(Deserialize, Default)]
pub struct RunQ {
    space: Option<String>,
    kind: Option<String>,
    auto_apply: Option<bool>,
    message: Option<String>,
}

#[derive(Deserialize, Default)]
pub struct RunReq {
    kind: Option<String>,
    config_id: Option<String>,
    auto_apply: Option<bool>,
    #[serde(default)]
    message: String,
}

/// Create a run. JSON body `{kind, config_id}`, or a `.tar.gz` body (upload + run in one request).
pub async fn create_run(
    State(app): State<AppRef>,
    Path(id): Path<String>,
    Query(q): Query<RunQ>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<impl IntoResponse> {
    let s = find_stack(&app, &id, q.space.as_deref())?;
    let is_json = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.contains("json"))
        .unwrap_or(false);
    let req: RunReq = if is_json {
        serde_json::from_slice(&body).map_err(|e| ApiError::bad_request(e.to_string()))?
    } else {
        RunReq { kind: q.kind.clone(), auto_apply: q.auto_apply, message: q.message.clone().unwrap_or_default(), ..Default::default() }
    };
    let kind = req.kind.as_deref().unwrap_or("tracked");
    if !matches!(kind, "proposed" | "tracked") {
        return Err(ApiError::bad_request("kind must be 'proposed' or 'tracked'"));
    }
    let cv_id = if is_json {
        match req.config_id {
            Some(cv) => cv,
            None => latest_config(&app, &s)?,
        }
    } else {
        let cv = app.db.write(|c| store::create_config(c, &s.id, false, kind == "proposed"))?;
        save_config(&app, &cv.id, &body)?;
        cv.id
    };
    let cv = app.db.read(|c| store::get_config(c, &cv_id))?.ok_or_else(|| ApiError::not_found("configuration"))?;
    if cv.stack_id != s.id || cv.status != "uploaded" {
        return Err(ApiError::bad_request("configuration is not uploaded for this stack"));
    }
    let auto = req.auto_apply.unwrap_or(s.auto_apply);
    let run = enqueue_run(&app, &s, kind, &cv.id, auto, &req.message, "api")?;
    Ok((StatusCode::CREATED, Json(run_view(run))))
}

fn latest_config(app: &AppRef, s: &Stack) -> ApiResult<String> {
    let r: Option<String> = app.db.read(|c| {
        Ok(c.query_row(
            "SELECT id FROM config_versions WHERE stack_id = ?1 AND status = 'uploaded' ORDER BY rowid DESC LIMIT 1",
            [&s.id],
            |r| r.get(0),
        )
        .ok())
    })?;
    r.ok_or_else(|| ApiError::bad_request("stack has no uploaded configuration; upload one or send a .tar.gz body"))
}

#[derive(Deserialize)]
pub struct RunsQ {
    space: Option<String>,
    limit: Option<i64>,
    before: Option<i64>,
}

pub async fn list_runs(State(app): State<AppRef>, Path(id): Path<String>, Query(q): Query<RunsQ>) -> ApiResult<Response> {
    let s = find_stack(&app, &id, q.space.as_deref())?;
    let limit = q.limit.unwrap_or(20).clamp(1, 200);
    let runs = app.db.read(|c| store::list_runs(c, &s.id, q.before.unwrap_or(i64::MAX), limit))?;
    let next = (runs.len() as i64 == limit).then(|| runs.last().map(|r| r.seq)).flatten();
    let items: Vec<RunView> = runs.into_iter().map(run_view).collect();
    #[derive(Serialize)]
    struct RunPage {
        items: Vec<RunView>,
        next_before: Option<i64>,
    }
    Ok(json_bytes(&RunPage { items, next_before: next }, 512 + limit as usize * 700))
}

#[derive(Deserialize)]
pub struct WaitQ {
    /// Long-poll: hold the request up to this many seconds until the run changes status.
    wait: Option<f64>,
}

pub async fn get_run(State(app): State<AppRef>, Path(id): Path<String>, Query(q): Query<WaitQ>) -> ApiResult<Json<RunView>> {
    let mut rx = app.hub.events.subscribe();
    let run = find_run(&app, &id)?;
    let wait = q.wait.unwrap_or(0.0).clamp(0.0, 60.0);
    if wait > 0.0 && !run.is_resting() {
        let initial = run.status.clone();
        let deadline = tokio::time::Instant::now() + Duration::from_secs_f64(wait);
        loop {
            match tokio::time::timeout_at(deadline, rx.recv()).await {
                Ok(Ok(e)) if e.run_id.as_deref() == Some(id.as_str()) && e.status.as_deref() != Some(initial.as_str()) => break,
                Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => break,
                Ok(_) => continue,
                Err(_) => break,
            }
        }
        return Ok(Json(run_view(find_run(&app, &id)?)));
    }
    Ok(Json(run_view(run)))
}

pub async fn run_plan(State(app): State<AppRef>, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let run = find_run(&app, &id)?;
    let summary = run.plan_summary.as_deref().and_then(|s| serde_json::from_str(s).ok()).unwrap_or(Value::Null);
    Ok(Json(json!({"run_id": run.id, "status": run.status, "has_changes": run.has_changes, "summary": summary})))
}

pub async fn confirm_run(State(app): State<AppRef>, Path(id): Path<String>) -> ApiResult<Json<RunView>> {
    let run = find_run(&app, &id)?;
    if !app.db.write(|c| store::transition(c, &run.id, &["planned"], "apply_queued"))? {
        return Err(ApiError::conflict(format!("run is {}, only planned runs can be confirmed", run.status)));
    }
    runner::publish(&app, &run.stack_id, &run.id, "apply_queued");
    app.sched.kick();
    Ok(Json(run_view(find_run(&app, &id)?)))
}

pub async fn discard_run(State(app): State<AppRef>, Path(id): Path<String>) -> ApiResult<Json<RunView>> {
    let run = find_run(&app, &id)?;
    let ok = app.db.write(|c| {
        let ok = store::transition(c, &run.id, &["planned"], "discarded")?;
        if ok {
            store::unlock_stack(c, &run.stack_id, Some(&run.id))?;
        }
        Ok(ok)
    })?;
    if !ok {
        return Err(ApiError::conflict(format!("run is {}, only planned runs can be discarded", run.status)));
    }
    runner::cleanup(&app, &run.id);
    app.metrics.finished("discarded");
    runner::publish(&app, &run.stack_id, &run.id, "discarded");
    app.sched.kick();
    Ok(Json(run_view(find_run(&app, &id)?)))
}

pub async fn cancel_run(State(app): State<AppRef>, Path(id): Path<String>) -> ApiResult<Json<RunView>> {
    let run = find_run(&app, &id)?;
    // Not started yet: cancel directly. Executing: signal the runner.
    let queued = app.db.write(|c| {
        let ok = store::transition(c, &run.id, &["plan_queued", "apply_queued"], "canceled")?;
        if ok {
            store::unlock_stack(c, &run.stack_id, Some(&run.id))?;
        }
        Ok(ok)
    })?;
    if queued {
        runner::cleanup(&app, &run.id);
        runner::publish(&app, &run.stack_id, &run.id, "canceled");
        app.sched.kick();
    } else if !app.sched.cancel(&run.id) {
        return Err(ApiError::conflict(format!("run is {}, nothing to cancel", run.status)));
    }
    Ok(Json(run_view(find_run(&app, &id)?)))
}

#[derive(Deserialize)]
pub struct LogQ {
    #[serde(default)]
    offset: u64,
    #[serde(default)]
    follow: bool,
}

/// Run log as JSON lines. With `follow=true` the response streams until the
/// run rests (finishes or waits for confirmation); if the next phase hasn't
/// started yet, it waits for it.
pub async fn run_logs(State(app): State<AppRef>, Path(id): Path<String>, Query(q): Query<LogQ>) -> ApiResult<Response> {
    let run = find_run(&app, &id)?;
    let ctype = [(header::CONTENT_TYPE, "application/x-ndjson")];
    // Subscribe before checking anything so no transition is missed.
    let mut events = app.hub.events.subscribe();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(600);
    loop {
        let live = if q.follow { app.hub.follow(&run.id) } else { Follow::Done };
        let upto = match &live {
            Follow::Live { upto, .. } => Some(*upto),
            Follow::Done => None,
        };
        let existing = tokio::fs::read(app.hub.log_path(&run.id)).await.unwrap_or_default();
        let head = lines_between(&existing, q.offset, upto);
        if let Follow::Live { rx, upto } = live {
            let from = q.offset.max(upto);
            let tail = BroadcastStream::new(rx)
                .take_while(|m| !matches!(m, Ok(LogMsg::End)))
                .filter_map(move |m| match m {
                    Ok(LogMsg::Line(seq, l)) if seq >= from => Some(Ok::<_, Infallible>(Bytes::from(format!("{l}\n")))),
                    _ => None,
                });
            let stream = tokio_stream::once(Ok::<_, Infallible>(Bytes::from(head))).chain(tail);
            return Ok((ctype, Body::from_stream(stream)).into_response());
        }
        let current = find_run(&app, &run.id)?;
        if !q.follow || current.is_resting() {
            return Ok((ctype, head).into_response());
        }
        // Not writing right now but not resting either (queued): wait for the log to open.
        match tokio::time::timeout_at(deadline, events.recv()).await {
            Ok(Ok(_)) | Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
            _ => return Ok((ctype, head).into_response()),
        }
    }
}

/// Lines `[from, upto)` of a log file (upto = None: to the end).
fn lines_between(data: &[u8], from: u64, upto: Option<u64>) -> Vec<u8> {
    let mut out = Vec::new();
    for (i, line) in data.split(|&b| b == b'\n').enumerate() {
        let i = i as u64;
        if line.is_empty() || i < from || upto.map(|u| i >= u).unwrap_or(false) {
            continue;
        }
        out.extend_from_slice(line);
        out.push(b'\n');
    }
    out
}

// ---------------------------------------------------------------- state & outputs

#[derive(Deserialize)]
pub struct OutQ {
    space: Option<String>,
    #[serde(default)]
    sensitive: bool,
}

pub async fn outputs(State(app): State<AppRef>, Path(id): Path<String>, Query(q): Query<OutQ>) -> ApiResult<Json<Value>> {
    let s = find_stack(&app, &id, q.space.as_deref())?;
    let sv = app.db.read(|c| store::current_state(c, &s.id))?;
    let mut outs = sv.map(|v| v.outputs).unwrap_or_else(|| json!({}));
    if !q.sensitive {
        if let Some(m) = outs.as_object_mut() {
            for v in m.values_mut() {
                if v["sensitive"].as_bool() == Some(true) {
                    v["value"] = Value::Null;
                }
            }
        }
    }
    Ok(Json(outs))
}

pub async fn state_versions(State(app): State<AppRef>, Path(id): Path<String>, Query(q): Query<SpaceQ>) -> ApiResult<Json<Value>> {
    let s = find_stack(&app, &id, q.space.as_deref())?;
    let items = app.db.read(|c| store::list_states(c, &s.id, 100))?;
    Ok(Json(json!({ "items": items })))
}

/// HTTP-backend GET: the current state document (204 when there is none).
pub async fn state_get(State(app): State<AppRef>, Path(id): Path<String>, Query(q): Query<SpaceQ>) -> ApiResult<Response> {
    let s = find_stack(&app, &id, q.space.as_deref())?;
    match app.db.read(|c| store::current_state(c, &s.id))? {
        Some(sv) => {
            let bytes = tokio::fs::read(app.dir("blobs/state").join(format!("{}.json", sv.sha256)))
                .await
                .map_err(anyhow::Error::from)?;
            Ok(([(header::CONTENT_TYPE, "application/json")], bytes).into_response())
        }
        None => Ok(StatusCode::NO_CONTENT.into_response()),
    }
}

#[derive(Deserialize)]
pub struct StatePostQ {
    space: Option<String>,
    #[serde(rename = "ID")]
    lock_id: Option<String>,
}

/// HTTP-backend POST: store a new state version. If the stack is locked, the
/// caller must present the lock ID (`?ID=`), as tofu does.
pub async fn state_post(
    State(app): State<AppRef>,
    Path(id): Path<String>,
    Query(q): Query<StatePostQ>,
    body: Bytes,
) -> ApiResult<StatusCode> {
    let s = find_stack(&app, &id, q.space.as_deref())?;
    if let Some(held) = &s.lock_id {
        if q.lock_id.as_deref() != Some(held.as_str()) {
            return Err(ApiError::new(StatusCode::LOCKED, "stack is locked by someone else"));
        }
    }
    let doc: Value = serde_json::from_slice(&body).map_err(|e| ApiError::bad_request(format!("invalid state: {e}")))?;
    let sha = sha256_hex(&body);
    let current = app.db.read(|c| store::current_state(c, &s.id))?;
    if current.as_ref().map(|c| c.sha256 == sha).unwrap_or(false) {
        return Ok(StatusCode::OK);
    }
    if let Some(cur) = &current {
        let serial = doc["serial"].as_i64().unwrap_or(0);
        if doc["lineage"].as_str() == Some(cur.lineage.as_str()) && serial < cur.serial {
            return Err(ApiError::conflict(format!("state serial {serial} is older than current {}", cur.serial)));
        }
    }
    let blob = app.dir("blobs/state").join(format!("{sha}.json"));
    if !blob.exists() {
        tokio::fs::write(&blob, &body).await.map_err(anyhow::Error::from)?;
    }
    let outputs = doc.get("outputs").cloned().unwrap_or_else(|| json!({}));
    app.db.write(|c| {
        store::insert_state(
            c,
            &store::NewState {
                stack_id: &s.id,
                serial: doc["serial"].as_i64().unwrap_or(0),
                lineage: doc["lineage"].as_str().unwrap_or(""),
                sha256: &sha,
                size: body.len() as i64,
                run_id: None,
                outputs: &outputs,
                resource_count: crate::store::count_instances(&doc),
            },
        )
    })?;
    Ok(StatusCode::OK)
}

/// HTTP-backend LOCK (sent as POST to `/state/lock`, configure `lock_method = "POST"`).
pub async fn state_lock(
    State(app): State<AppRef>,
    Path(id): Path<String>,
    Query(q): Query<SpaceQ>,
    Json(info): Json<Value>,
) -> ApiResult<Response> {
    let s = find_stack(&app, &id, q.space.as_deref())?;
    let lock_id = info["ID"].as_str().ok_or_else(|| ApiError::bad_request("lock info needs an ID"))?.to_string();
    match app.db.write(|c| store::try_lock_stack(c, &s.id, &lock_id, &info))? {
        Ok(()) => Ok(StatusCode::OK.into_response()),
        // 423 with the current holder, as the http backend expects.
        Err(holder) => Ok((StatusCode::LOCKED, Json(holder)).into_response()),
    }
}

/// HTTP-backend UNLOCK (DELETE `/state/lock`, configure `unlock_method = "DELETE"`).
pub async fn state_unlock(
    State(app): State<AppRef>,
    Path(id): Path<String>,
    Query(q): Query<SpaceQ>,
    body: Bytes,
) -> ApiResult<StatusCode> {
    let s = find_stack(&app, &id, q.space.as_deref())?;
    let lock_id = serde_json::from_slice::<Value>(&body).ok().and_then(|v| v["ID"].as_str().map(String::from));
    if app.db.write(|c| store::unlock_stack(c, &s.id, lock_id.as_deref()))? || s.lock_id.is_none() {
        Ok(StatusCode::OK)
    } else {
        Err(ApiError::new(StatusCode::CONFLICT, "lock ID does not match"))
    }
}

// ---------------------------------------------------------------- events (SSE)

#[derive(Deserialize)]
pub struct EventsQ {
    stack: Option<String>,
    run: Option<String>,
}

pub async fn events(State(app): State<AppRef>, Query(q): Query<EventsQ>) -> impl IntoResponse {
    let rx = app.hub.events.subscribe();
    let stream = BroadcastStream::new(rx).filter_map(move |e| {
        let e = e.ok()?;
        if q.stack.as_deref().map(|s| s != e.stack_id).unwrap_or(false) {
            return None;
        }
        if q.run.is_some() && q.run != e.run_id {
            return None;
        }
        Some(Ok::<_, Infallible>(SseEvent::default().event(e.kind).data(serde_json::to_string(&e).unwrap_or_default())))
    });
    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(30)))
}
