//! HTTP routing and authentication.

pub mod identity;
pub mod native;
pub mod policies;
pub mod registry;
pub mod tfe;
pub mod vcs;

use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::Router;

use crate::app::{token_from, ApiError, AppRef};

/// Paths reachable without a token.
fn is_public(path: &str) -> bool {
    matches!(path, "/healthz" | "/readyz" | "/metrics" | "/.well-known/terraform.json" | "/api/v2/ping"
        | "/auth/login" | "/auth/callback" | "/auth/config")
        // Pre-signed (checked in the handler).
        || (path.starts_with("/api/v2/configuration-versions/") && path.ends_with("/upload"))
        // Verified by provider signature in the handler.
        || path.starts_with("/webhooks/vcs/")
        // Pre-signed registry downloads.
        || path.starts_with("/registry/dl/")
}

async fn auth(State(app): State<AppRef>, mut req: Request, next: Next) -> Response {
    app.count_request();
    let path = req.uri().path().to_string();
    if is_public(&path) {
        return next.run(req).await;
    }
    let principal = match token_from(req.headers()) {
        Some(t) => app.principal(&t).await,
        None => None,
    };
    let Some(principal) = principal else {
        let mut r = ApiError::new(StatusCode::UNAUTHORIZED, "missing or invalid token").into_response();
        r.headers_mut().insert("WWW-Authenticate", "Bearer realm=\"cairn\"".parse().unwrap());
        return r;
    };
    let method = req.method().clone();
    let who = principal.label();
    req.extensions_mut().insert(principal);
    let resp = next.run(req).await;
    // Audit every mutation (not reads, not the high-volume state lock/unlock pairs' bodies).
    if method != axum::http::Method::GET && method != axum::http::Method::HEAD {
        let status = resp.status().as_u16();
        let _ = app.db.write(|c| crate::auth::audit(c, &who, method.as_str(), &path, status));
    }
    resp
}

pub fn router(app: AppRef) -> Router {
    use native as n;
    let v1 = Router::new()
        .route("/whoami", get(identity::whoami))
        .route("/tokens", get(identity::list_tokens).post(identity::create_token))
        .route("/tokens/{id}", axum::routing::delete(identity::delete_token))
        .route("/spaces", get(identity::list_spaces).post(identity::create_space))
        .route("/spaces/{space}", get(identity::get_space).delete(identity::delete_space))
        .route("/spaces/{space}/roles", get(identity::list_roles).post(identity::put_role))
        .route("/spaces/{space}/roles/{id}", axum::routing::delete(identity::delete_role))
        .route("/users", get(identity::list_users))
        .route("/policies", get(policies::list).post(policies::create))
        .route("/policies/evaluate", post(policies::evaluate_adhoc))
        .route("/policies/{id}", get(policies::get).patch(policies::patch).delete(policies::delete))
        .route("/policies/{id}/evaluate", post(policies::evaluate_stored))
        .route("/policies/{id}/attach/{stack}", post(policies::attach).delete(policies::detach))
        .route("/stacks/{id}/policies", get(policies::stack_policies))
        .route("/runs/{id}/policies", get(policies::run_policies))
        .route("/runs/{id}/reviews", post(policies::review))
        .route("/registry/modules", get(registry::list_modules))
        .route("/registry/modules/{ns}/{name}/{system}", get(registry::get_module).delete(registry::delete_module))
        .route("/registry/modules/{ns}/{name}/{system}/{version}", post(registry::publish_module).delete(registry::delete_module_version))
        .route("/registry/providers", get(registry::list_providers))
        .route("/registry/providers/{ns}/{kind}", get(registry::get_provider))
        .route("/registry/providers/{ns}/{kind}/{version}", post(registry::create_provider_version).delete(registry::delete_provider_version))
        .route("/registry/providers/{ns}/{kind}/{version}/{os}/{arch}", axum::routing::put(registry::upload_platform))
        .route("/vcs", get(vcs::list).post(vcs::create))
        .route("/vcs/{id}", get(vcs::get).delete(vcs::delete))
        .route("/stacks/{id}/sync", post(vcs::sync))
        .route("/spaces/{space}/webhooks", get(policies::list_webhooks).post(policies::create_webhook))
        .route("/spaces/{space}/webhooks/{id}", axum::routing::delete(policies::delete_webhook))
        .route("/audit", get(identity::audit))
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
        .route("/auth/config", get(identity::auth_config))
        .route("/auth/login", get(identity::login))
        .route("/webhooks/vcs/{id}", post(vcs::webhook))
        .route("/registry/modules/v1/{ns}/{name}/{system}/versions", get(registry::module_versions))
        .route("/registry/modules/v1/{ns}/{name}/{system}/{version}/download", get(registry::module_download))
        .route("/registry/providers/v1/{ns}/{kind}/versions", get(registry::provider_versions))
        .route("/registry/providers/v1/{ns}/{kind}/{version}/download/{os}/{arch}", get(registry::provider_download))
        .route("/registry/dl/{exp}/{sig}/{kind}/{id}/{file}", get(registry::download))
        .route("/auth/callback", get(identity::callback))
        .nest("/api/v1", v1)
        .nest("/api/v2", v2)
        .layer(DefaultBodyLimit::max(100 << 20))
        .layer(middleware::from_fn_with_state(app.clone(), auth))
        .with_state(app)
}
