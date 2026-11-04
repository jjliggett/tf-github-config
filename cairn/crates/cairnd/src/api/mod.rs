//! HTTP routing and authentication.

pub mod native;
pub mod tfe;

use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::Router;

use crate::app::{token_from, ApiError, AppRef};

/// Paths reachable without a token.
fn is_public(path: &str) -> bool {
    matches!(path, "/healthz" | "/readyz" | "/metrics" | "/.well-known/terraform.json" | "/api/v2/ping")
        // Pre-signed (checked in the handler).
        || (path.starts_with("/api/v2/configuration-versions/") && path.ends_with("/upload"))
}

async fn auth(State(app): State<AppRef>, req: Request, next: Next) -> Response {
    app.count_request();
    if !is_public(req.uri().path()) {
        let ok = token_from(req.headers()).map(|t| app.check_token(&t)).unwrap_or(false);
        if !ok {
            let mut r = ApiError::new(StatusCode::UNAUTHORIZED, "missing or invalid token").into_response();
            r.headers_mut().insert("WWW-Authenticate", "Bearer realm=\"cairn\"".parse().unwrap());
            return r;
        }
    }
    next.run(req).await
}

pub fn router(app: AppRef) -> Router {
    use native as n;
    let v1 = Router::new()
        .route("/whoami", get(n::whoami))
        .route("/tokens", post(n::create_token))
        .route("/spaces", get(n::list_spaces).post(n::create_space))
        .route("/stacks", get(n::list_stacks).post(n::create_stack))
        .route("/stacks/{id}", get(n::get_stack).patch(n::patch_stack).delete(n::delete_stack))
        .route("/stacks/{id}/lock", post(n::lock_stack).delete(n::unlock_stack))
        .route("/stacks/{id}/variables", get(n::list_vars).post(n::set_var))
        .route("/stacks/{id}/variables/{key}", axum::routing::delete(n::delete_var))
        .route("/stacks/{id}/configs", post(n::upload_config))
        .route("/stacks/{id}/runs", get(n::list_runs).post(n::create_run))
        .route("/stacks/{id}/outputs", get(n::outputs))
        .route("/stacks/{id}/state", get(n::state_get).post(n::state_post))
        .route("/stacks/{id}/state/lock", post(n::state_lock).delete(n::state_unlock))
        .route("/stacks/{id}/state/versions", get(n::state_versions))
        .route("/runs/{id}", get(n::get_run))
        .route("/runs/{id}/plan", get(n::run_plan))
        .route("/runs/{id}/logs", get(n::run_logs))
        .route("/runs/{id}/confirm", post(n::confirm_run))
        .route("/runs/{id}/discard", post(n::discard_run))
        .route("/runs/{id}/cancel", post(n::cancel_run))
        .route("/events", get(n::events));

    let v2 = Router::new()
        .route("/ping", get(tfe::ping))
        .route("/account/details", get(tfe::account_details))
        .route("/organizations", post(tfe::create_org))
        .route("/organizations/{org}", get(tfe::get_org))
        .route("/organizations/{org}/entitlement-set", get(tfe::entitlements))
        .route("/organizations/{org}/workspaces", get(tfe::list_workspaces).post(tfe::create_workspace))
        .route("/organizations/{org}/workspaces/{name}", get(tfe::get_workspace_by_name))
        .route("/workspaces/{id}", get(tfe::get_workspace).patch(tfe::update_workspace))
        .route("/workspaces/{id}/actions/{action}", post(tfe::workspace_action))
        .route("/workspaces/{id}/configuration-versions", post(tfe::create_cv))
        .route("/workspaces/{id}/runs", get(tfe::list_ws_runs))
        .route("/workspaces/{id}/current-state-version", get(tfe::current_state))
        .route("/workspaces/{id}/current-state-version-outputs", get(tfe::current_outputs))
        .route("/configuration-versions/{id}", get(tfe::get_cv))
        .route("/configuration-versions/{id}/upload", put(tfe::upload_cv))
        .route("/runs", post(tfe::create_run))
        .route("/runs/{id}", get(tfe::get_run))
        .route("/runs/{id}/actions/{action}", post(tfe::run_action))
        .route("/plans/{id}", get(tfe::get_plan))
        .route("/plans/{id}/logs", get(tfe::phase_logs))
        .route("/applies/{id}", get(tfe::get_apply))
        .route("/applies/{id}/logs", get(tfe::phase_logs))
        .route("/state-versions/{id}/download", get(tfe::download_state));

    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/readyz", get(|| async { "ok" }))
        .route("/metrics", get(n::metrics))
        .route("/.well-known/terraform.json", get(tfe::well_known))
        .nest("/api/v1", v1)
        .nest("/api/v2", v2)
        .layer(DefaultBodyLimit::max(100 << 20))
        .layer(middleware::from_fn_with_state(app.clone(), auth))
        .with_state(app)
}
