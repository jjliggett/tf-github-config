//! Identity endpoints: whoami, tokens, spaces, role bindings, users, audit,
//! and the OIDC login flow.

use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::api::native::{valid_name, DEFAULT_SPACE};
use crate::app::{ApiError, ApiResult, AppRef};
use crate::auth::{self, Principal, Role};
use crate::store;
use crate::util::{new_token, now_plus_secs, sha256_hex};

fn reload(app: &AppRef) -> ApiResult<()> {
    app.authz.reload(&app.db)?;
    Ok(())
}

pub async fn whoami(State(app): State<AppRef>, p: Principal) -> ApiResult<Json<Value>> {
    let spaces = app.db.read(store::list_spaces)?;
    let roles: Vec<Value> = spaces
        .iter()
        .filter_map(|s| app.authz.role(&p, &s.id).map(|r| json!({"space": s.name, "space_id": s.id, "role": r})))
        .collect();
    Ok(Json(json!({"principal": p, "roles": roles})))
}

// ---------------------------------------------------------------- tokens

#[derive(Deserialize, Default)]
pub struct NewTokenReq {
    #[serde(default)]
    description: String,
    /// `user` (acts as you; default for users) or `service` (its own role bindings).
    kind: Option<String>,
    #[serde(default)]
    site_admin: bool,
    expires_in_hours: Option<u64>,
    /// Roles for a service token: [{"space": "prod", "role": "writer"}].
    #[serde(default)]
    roles: Vec<SpaceRole>,
}

#[derive(Deserialize)]
pub struct SpaceRole {
    space: String,
    role: String,
}

pub async fn create_token(State(app): State<AppRef>, p: Principal, Json(req): Json<NewTokenReq>) -> ApiResult<impl IntoResponse> {
    let kind = req.kind.clone().unwrap_or_else(|| if p.kind == "user" { "user".into() } else { "service".into() });
    match kind.as_str() {
        "user" if p.kind != "user" => return Err(ApiError::bad_request("only users can create user tokens")),
        "user" => {}
        "service" => {
            // Space admins may mint service tokens limited to their spaces.
            for r in &req.roles {
                let space = find_space(&app, &r.space)?;
                app.authz.require(&p, &space.id, Role::Admin)?;
            }
            if req.roles.is_empty() && !p.site_admin {
                return Err(ApiError::bad_request("service tokens need `roles` (or a site admin caller)"));
            }
        }
        _ => return Err(ApiError::bad_request("kind must be 'user' or 'service'")),
    }
    if req.site_admin {
        app.authz.require_site_admin(&p)?;
    }
    let token = new_token();
    let sha = sha256_hex(token.as_bytes());
    let expires = req.expires_in_hours.map(|h| now_plus_secs(h * 3600));
    let roles: Vec<(String, Role)> = req
        .roles
        .iter()
        .map(|r| Ok((find_space(&app, &r.space)?.id, Role::parse(&r.role).ok_or_else(|| ApiError::bad_request("bad role"))?)))
        .collect::<ApiResult<_>>()?;
    let owner = (kind == "user").then(|| p.id.clone());
    let id = app.db.write(|c| {
        let id = auth::insert_token(
            c,
            &auth::NewToken {
                sha: &sha,
                kind: &kind,
                owner: owner.as_deref(),
                description: &req.description,
                site_admin: req.site_admin,
                expires_at: expires.as_deref(),
            },
        )?;
        for (space, role) in &roles {
            auth::put_binding(c, space, "token", &id, *role)?;
        }
        Ok(id)
    })?;
    reload(&app)?;
    Ok((
        StatusCode::CREATED,
        Json(json!({"id": id, "token": token, "kind": kind, "description": req.description, "expires_at": expires})),
    ))
}

#[derive(Deserialize)]
pub struct AllQ {
    #[serde(default)]
    all: bool,
}

pub async fn list_tokens(State(app): State<AppRef>, p: Principal, Query(q): Query<AllQ>) -> ApiResult<Json<Value>> {
    let owner = if q.all {
        app.authz.require_site_admin(&p)?;
        None
    } else {
        Some(p.id.clone())
    };
    let items = app.db.read(|c| auth::list_tokens(c, owner.as_deref()))?;
    Ok(Json(json!({"items": items})))
}

pub async fn delete_token(State(app): State<AppRef>, p: Principal, Path(id): Path<String>) -> ApiResult<StatusCode> {
    let owner = if p.site_admin { None } else { Some(p.id.clone()) };
    if !app.db.write(|c| auth::delete_token(c, &id, owner.as_deref()))? {
        return Err(ApiError::not_found("token"));
    }
    reload(&app)?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------- spaces

pub fn find_space(app: &AppRef, key: &str) -> ApiResult<store::Space> {
    app.db.read(|c| store::space_by_name(c, key))?.ok_or_else(|| ApiError::not_found("space"))
}

pub async fn list_spaces(State(app): State<AppRef>, p: Principal) -> ApiResult<Json<Value>> {
    let allowed = app.authz.readable_spaces(&p);
    let items: Vec<Value> = app
        .db
        .read(store::list_spaces)?
        .into_iter()
        .filter(|s| allowed.as_ref().map(|a| a.contains(&s.id)).unwrap_or(true))
        .map(|s| {
            let role = app.authz.role(&p, &s.id);
            let mut v = serde_json::to_value(s).unwrap_or_default();
            v["your_role"] = json!(role);
            v
        })
        .collect();
    Ok(Json(json!({ "items": items })))
}

#[derive(Deserialize)]
pub struct NewSpace {
    name: String,
    #[serde(default)]
    email: String,
    /// Parent space name or ID; omitted = top level (site admins only).
    parent: Option<String>,
}

pub async fn create_space(State(app): State<AppRef>, p: Principal, Json(req): Json<NewSpace>) -> ApiResult<impl IntoResponse> {
    valid_name(&req.name)?;
    let parent = match &req.parent {
        Some(pk) => {
            let ps = find_space(&app, pk)?;
            app.authz.require(&p, &ps.id, Role::Admin)?;
            Some(ps.id)
        }
        None => {
            app.authz.require_site_admin(&p)?;
            None
        }
    };
    let s = app.db.write(|c| store::create_space(c, &req.name, &req.email, parent.as_deref()))?;
    reload(&app)?;
    Ok((StatusCode::CREATED, Json(s)))
}

pub async fn get_space(State(app): State<AppRef>, p: Principal, Path(key): Path<String>) -> ApiResult<Json<Value>> {
    let s = find_space(&app, &key)?;
    let role = app.authz.role(&p, &s.id).ok_or_else(|| ApiError::not_found("space"))?;
    let mut v = serde_json::to_value(s).unwrap_or_default();
    v["your_role"] = json!(role);
    Ok(Json(v))
}

pub async fn delete_space(State(app): State<AppRef>, p: Principal, Path(key): Path<String>) -> ApiResult<StatusCode> {
    let s = find_space(&app, &key)?;
    match &s.parent_id {
        Some(parent) => app.authz.require(&p, parent, Role::Admin)?,
        None => app.authz.require_site_admin(&p)?,
    }
    if s.name == DEFAULT_SPACE {
        return Err(ApiError::conflict("the default space can't be deleted"));
    }
    let busy: i64 = app.db.read(|c| {
        Ok(c.query_row(
            "SELECT (SELECT COUNT(*) FROM stacks WHERE space_id = ?1) + (SELECT COUNT(*) FROM spaces WHERE parent_id = ?1)",
            [&s.id],
            |r| r.get(0),
        )?)
    })?;
    if busy > 0 {
        return Err(ApiError::conflict("space still has stacks or child spaces"));
    }
    app.db.write(|c| store::delete_space(c, &s.id))?;
    reload(&app)?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------- role bindings

pub async fn list_roles(State(app): State<AppRef>, p: Principal, Path(key): Path<String>) -> ApiResult<Json<Value>> {
    let s = find_space(&app, &key)?;
    app.authz.require(&p, &s.id, Role::Admin)?;
    let items = app.db.read(|c| auth::list_bindings(c, Some(&s.id)))?;
    Ok(Json(json!({ "items": items })))
}

#[derive(Deserialize)]
pub struct BindReq {
    /// `user` (user ID or email), `team` (IdP group name) or `token` (service token ID).
    principal_kind: String,
    principal: String,
    role: String,
}

pub async fn put_role(State(app): State<AppRef>, p: Principal, Path(key): Path<String>, Json(r): Json<BindReq>) -> ApiResult<impl IntoResponse> {
    let s = find_space(&app, &key)?;
    app.authz.require(&p, &s.id, Role::Admin)?;
    let role = Role::parse(&r.role).ok_or_else(|| ApiError::bad_request("role must be reader, planner, writer or admin"))?;
    // Nobody can grant more than they hold.
    if app.authz.role(&p, &s.id) < Some(role) {
        return Err(ApiError::new(StatusCode::FORBIDDEN, "can't grant a role above your own"));
    }
    let principal = match r.principal_kind.as_str() {
        "team" | "token" => r.principal.clone(),
        "user" => {
            let snap = app.authz.snapshot();
            snap.users
                .values()
                .find(|u| u.id == r.principal || u.email == r.principal)
                .map(|u| u.id.clone())
                .ok_or_else(|| ApiError::bad_request("unknown user (users appear after their first login)"))?
        }
        _ => return Err(ApiError::bad_request("principal_kind must be user, team or token")),
    };
    let id = app.db.write(|c| auth::put_binding(c, &s.id, &r.principal_kind, &principal, role))?;
    reload(&app)?;
    Ok((StatusCode::CREATED, Json(json!({"id": id, "space_id": s.id, "principal_kind": r.principal_kind, "principal": principal, "role": role}))))
}

pub async fn delete_role(State(app): State<AppRef>, p: Principal, Path((key, id)): Path<(String, String)>) -> ApiResult<StatusCode> {
    let s = find_space(&app, &key)?;
    app.authz.require(&p, &s.id, Role::Admin)?;
    if !app.db.write(|c| auth::delete_binding(c, &s.id, &id))? {
        return Err(ApiError::not_found("role binding"));
    }
    reload(&app)?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------- users & audit

pub async fn list_users(State(app): State<AppRef>, p: Principal) -> ApiResult<Json<Value>> {
    app.authz.require_site_admin(&p)?;
    Ok(Json(json!({"items": app.db.read(auth::list_users)?})))
}

#[derive(Deserialize)]
pub struct AuditQ {
    before: Option<i64>,
    limit: Option<i64>,
}

pub async fn audit(State(app): State<AppRef>, p: Principal, Query(q): Query<AuditQ>) -> ApiResult<Json<Value>> {
    app.authz.require_site_admin(&p)?;
    let items = app.db.read(|c| auth::list_audit(c, q.before.unwrap_or(i64::MAX), q.limit.unwrap_or(100).clamp(1, 1000)))?;
    Ok(Json(json!({"items": items})))
}

// ---------------------------------------------------------------- OIDC login

pub async fn auth_config(State(app): State<AppRef>) -> Json<Value> {
    Json(json!({"oidc": app.oidc.as_ref().map(|o| json!({"issuer": o.cfg.issuer, "client_id": o.cfg.client_id}))}))
}

#[derive(Deserialize)]
pub struct LoginQ {
    /// Loopback URL for CLI logins (`http://127.0.0.1:<port>/…`).
    redirect: Option<String>,
}

pub async fn login(State(app): State<AppRef>, headers: HeaderMap, Query(q): Query<LoginQ>) -> ApiResult<Response> {
    let Some(_) = &app.oidc else { return Err(ApiError::new(StatusCode::NOT_FOUND, "OIDC is not configured")) };
    if let Some(r) = &q.redirect {
        if !crate::oidc::is_loopback_redirect(r) {
            return Err(ApiError::bad_request("redirect must be a loopback URL"));
        }
    }
    let callback = format!("{}/auth/callback", app.base_url(&headers));
    let app2 = app.clone();
    let url = tokio::task::spawn_blocking(move || app2.oidc.as_ref().unwrap().start(&callback, q.redirect))
        .await
        .map_err(anyhow::Error::from)??;
    Ok(Redirect::to(&url).into_response())
}

#[derive(Deserialize)]
pub struct CallbackQ {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

pub async fn callback(State(app): State<AppRef>, headers: HeaderMap, Query(q): Query<CallbackQ>) -> ApiResult<Response> {
    if let Some(e) = q.error {
        return Err(ApiError::new(StatusCode::UNAUTHORIZED, format!("login failed: {e} {}", q.error_description.unwrap_or_default())));
    }
    let (Some(code), Some(state)) = (q.code, q.state) else {
        return Err(ApiError::bad_request("missing code or state"));
    };
    let callback = format!("{}/auth/callback", app.base_url(&headers));
    let app2 = app.clone();
    let (id, redirect) = tokio::task::spawn_blocking(move || app2.oidc.as_ref().unwrap().finish(&code, &state, &callback))
        .await
        .map_err(anyhow::Error::from)?
        .map_err(|e| ApiError::new(StatusCode::UNAUTHORIZED, format!("login failed: {e:#}")))?;
    let token = new_token();
    let sha = sha256_hex(token.as_bytes());
    let expires = now_plus_secs(app.cfg.session_hours * 3600);
    let user = app.db.write(|c| {
        let (u, _) = auth::upsert_user(c, &id.subject, &id.email, &id.name, &id.groups, id.site_admin)?;
        auth::insert_token(
            c,
            &auth::NewToken { sha: &sha, kind: "user", owner: Some(&u.id), description: "login session", site_admin: false, expires_at: Some(&expires) },
        )?;
        Ok(u)
    })?;
    reload(&app)?;
    if let Some(r) = redirect {
        let sep = if r.contains('?') { '&' } else { '?' };
        return Ok(Redirect::to(&format!("{r}{sep}token={token}")).into_response());
    }
    Ok((
        [(header::CACHE_CONTROL, "no-store")],
        Json(json!({"token": token, "expires_at": expires, "user": {"id": user.id, "email": user.email, "name": user.name, "groups": user.groups}})),
    )
        .into_response())
}
