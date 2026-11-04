//! VCS integrations: management API, the webhook receiver, run triggering
//! and reporting back (commit statuses and one pull request comment).
//!
//! Flow: a webhook is verified and parsed (see [`crate::vcs`]); matching
//! stacks (same integration and repository) decide what to do — by default
//! pushes to the stack's branch start tracked runs and pull requests against
//! it start proposed runs; `push` policies can override either. Source is
//! fetched as an archive of the exact commit. Closing or merging a pull
//! request stops its queued and planned runs, so no lock outlives it.

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::api::identity::find_space;
use crate::api::native::{enqueue_run, find_stack, run_view, stop_run, valid_name, SpaceQ};
use crate::app::{ApiError, ApiResult, AppRef};
use crate::auth::{Principal, Role};
use crate::store::{self, Stack, StackVcs};
use crate::util::new_token;
use crate::vcs::{self, Event, Integration, PrAction};
use crate::policy;

// ---------------------------------------------------------------- management

fn view(app: &AppRef, i: &Integration) -> Value {
    json!({"id": i.id, "space_id": i.space_id, "provider": i.provider, "name": i.name, "api_url": i.api_url,
           "web_url": i.web_url, "username": i.username, "created_at": i.created_at,
           "webhook_url": format!("{}/webhooks/vcs/{}", app.public_url(), i.id)})
}

pub async fn list(State(app): State<AppRef>, p: Principal) -> ApiResult<Json<Value>> {
    let allowed = app.authz.readable_spaces(&p);
    let items: Vec<Value> = app
        .db
        .read(vcs::list)?
        .iter()
        .filter(|i| allowed.as_ref().map(|a| a.contains(&i.space_id)).unwrap_or(true))
        .map(|i| view(&app, i))
        .collect();
    Ok(Json(json!({"items": items})))
}

#[derive(Deserialize)]
pub struct NewIntegrationReq {
    name: String,
    provider: String,
    #[serde(default)]
    space: Option<String>,
    /// API base or server URL (Gitea/Forgejo: the server; Azure DevOps: https://dev.azure.com/org).
    #[serde(default)]
    url: Option<String>,
    /// Bitbucket: where archives are downloaded from (default https://bitbucket.org).
    #[serde(default)]
    web_url: String,
    /// Bitbucket: account name for app passwords / API tokens (empty: bearer token).
    #[serde(default)]
    username: String,
    token: String,
    /// Webhook secret (generated when omitted; returned once).
    #[serde(default)]
    webhook_secret: Option<String>,
}

pub async fn create(State(app): State<AppRef>, p: Principal, Json(r): Json<NewIntegrationReq>) -> ApiResult<impl IntoResponse> {
    valid_name(&r.name)?;
    if !vcs::PROVIDERS.contains(&r.provider.as_str()) {
        return Err(ApiError::bad_request(format!("provider must be one of {}", vcs::PROVIDERS.join(", "))));
    }
    let space = find_space(&app, r.space.as_deref().unwrap_or(crate::api::native::DEFAULT_SPACE))?;
    app.authz.require(&p, &space.id, Role::Admin)?;
    let api_url = vcs::api_base(&r.provider, r.url.as_deref()).map_err(|e| ApiError::bad_request(format!("{e:#}")))?;
    let secret = r.webhook_secret.clone().filter(|s| !s.is_empty()).unwrap_or_else(new_token);
    let i = app.db.write(|c| {
        vcs::create(
            c,
            &vcs::NewIntegration {
                space_id: &space.id,
                provider: &r.provider,
                name: &r.name,
                api_url: &api_url,
                web_url: &r.web_url,
                username: &r.username,
                token: &app.secrets.seal(&r.token),
                secret: &app.secrets.seal(&secret),
            },
        )
    })?;
    let mut v = view(&app, &i);
    v["webhook_secret"] = json!(secret);
    Ok((StatusCode::CREATED, Json(v)))
}

fn find(app: &AppRef, p: &Principal, key: &str, need: Role) -> ApiResult<Integration> {
    let i = app.db.read(|c| vcs::get(c, key))?.ok_or_else(|| ApiError::not_found("VCS integration"))?;
    app.authz.require(p, &i.space_id, need)?;
    Ok(i)
}

pub async fn get(State(app): State<AppRef>, p: Principal, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    Ok(Json(view(&app, &find(&app, &p, &id, Role::Reader)?)))
}

pub async fn delete(State(app): State<AppRef>, p: Principal, Path(id): Path<String>) -> ApiResult<StatusCode> {
    let i = find(&app, &p, &id, Role::Admin)?;
    app.db.write(|c| vcs::delete(c, &i.id))?;
    Ok(StatusCode::NO_CONTENT)
}

/// Resolve a stack's `vcs` setting: the integration must live in the
/// stack's space or one of its ancestors.
pub fn resolve_stack_vcs(app: &AppRef, space_id: &str, v: &StackVcsReq) -> ApiResult<StackVcs> {
    let i = app.db.read(|c| vcs::get(c, &v.integration))?.ok_or_else(|| ApiError::bad_request("unknown VCS integration"))?;
    if !app.space_chain(space_id).contains(&i.space_id) {
        return Err(ApiError::bad_request("the VCS integration must belong to the stack's space or a parent space"));
    }
    if v.repo.split('/').filter(|s| !s.is_empty()).count() < 2 {
        return Err(ApiError::bad_request("repo must be owner/name (Azure DevOps: project/repo)"));
    }
    Ok(StackVcs {
        integration_id: i.id,
        repo: v.repo.trim_matches('/').to_string(),
        branch: v.branch.clone().unwrap_or_else(|| "main".into()),
        pr_apply: v.pr_apply,
    })
}

#[derive(Deserialize, Clone)]
pub struct StackVcsReq {
    /// Integration ID or name.
    pub integration: String,
    pub repo: String,
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub pr_apply: bool,
}

/// `POST /stacks/{id}/sync`: a tracked run from the head of the stack's branch.
pub async fn sync(State(app): State<AppRef>, p: Principal, Path(id): Path<String>, Query(q): Query<SpaceQ>) -> ApiResult<impl IntoResponse> {
    let s = find_stack(&app, &p, &id, q.space.as_deref(), Role::Writer)?;
    let v = s.vcs.clone().ok_or_else(|| ApiError::bad_request("stack has no VCS source"))?;
    let i = app.db.read(|c| vcs::get(c, &v.integration_id))?.ok_or_else(|| ApiError::not_found("VCS integration"))?;
    let source = json!({"integration": i.id, "repo": v.repo, "branch": v.branch, "sha": null, "pr": null});
    let run = start_run(&app, &i, &s, "tracked", &v.branch, source, &format!("Sync {} from {}", v.branch, v.repo), &p.label()).await?;
    Ok((StatusCode::CREATED, Json(run_view(run))))
}

// ---------------------------------------------------------------- webhooks

/// `POST /webhooks/vcs/{id}` (unauthenticated; verified by signature).
pub async fn webhook(State(app): State<AppRef>, Path(id): Path<String>, headers: HeaderMap, body: Bytes) -> ApiResult<impl IntoResponse> {
    let i = app.db.read(|c| vcs::get(c, &id))?.ok_or_else(|| ApiError::not_found("VCS integration"))?;
    let secret = app.secrets.open(&i.secret)?;
    vcs::verify(&i.provider, &secret, &headers, &body).map_err(|e| ApiError::new(StatusCode::UNAUTHORIZED, format!("{e:#}")))?;
    let events = vcs::parse(&i.provider, &headers, &body).map_err(|e| ApiError::bad_request(format!("{e:#}")))?;
    let mut actions = vec![];
    for e in events {
        actions.extend(handle(&app, &i, e).await?);
    }
    Ok((StatusCode::ACCEPTED, Json(json!({"actions": actions}))))
}

fn stack_input(s: &Stack) -> Value {
    json!({"id": s.id, "name": s.name, "space": s.space, "labels": s.labels, "project_root": s.project_root,
           "repo": s.vcs.as_ref().map(|v| &v.repo), "branch": s.vcs.as_ref().map(|v| &v.branch)})
}

/// Does a change list touch the stack's project root? (Unknown lists do.)
fn affects(files: &Option<Vec<String>>, root: &str) -> bool {
    let root = root.trim_matches('/');
    match files {
        None => true,
        Some(_) if root.is_empty() || root == "." => true,
        Some(f) => f.iter().any(|p| p.starts_with(&format!("{root}/"))),
    }
}

/// `push` policies decide (`track` / `propose` / `ignore`); without any, `default`.
fn decide(app: &AppRef, s: &Stack, input: &Value, default: &'static str) -> ApiResult<&'static str> {
    let chain = app.space_chain(&s.space_id);
    let pols = app.db.read(|c| policy::for_stack(c, "push", &s.id, &chain, &s.labels))?;
    if pols.is_empty() {
        return Ok(default);
    }
    let v = app.policies.run(&pols, input)?;
    Ok(if v.iter().any(|v| v.flag("ignore")) {
        "ignore"
    } else if v.iter().any(|v| v.flag("track")) {
        "track"
    } else if v.iter().any(|v| v.flag("propose")) {
        "propose"
    } else {
        "ignore"
    })
}

fn kind_of(decision: &str) -> Option<&'static str> {
    match decision {
        "track" => Some("tracked"),
        "propose" => Some("proposed"),
        _ => None,
    }
}

async fn handle(app: &AppRef, i: &Integration, e: Event) -> ApiResult<Vec<Value>> {
    let stacks: Vec<Stack> = app.db.read(|c| store::stacks_for_repo(c, &i.id, e.repo()))?;
    let mut out = vec![];
    let by = |author: &str| format!("vcs:{}:{author}", i.provider);
    match &e {
        Event::Push { repo, branch, sha, message, author, files } => {
            for s in &stacks {
                let v = s.vcs.as_ref().expect("matched by repo");
                let default = if *branch == v.branch && affects(files, &s.project_root) { "track" } else { "ignore" };
                let input = json!({"push": {"branch": branch, "sha": sha, "message": message, "author": author, "affected_files": files},
                                   "stack": stack_input(s)});
                let d = decide(app, s, &input, default)?;
                out.push(json!({"stack": s.name, "space": s.space, "event": "push", "decision": d}));
                if let Some(kind) = kind_of(d) {
                    let source = json!({"integration": i.id, "repo": repo, "sha": sha, "branch": branch, "pr": null});
                    spawn_run(app, i, s, kind, sha, source, message, &by(author));
                }
            }
        }
        Event::PullRequest { repo, number, action: PrAction::Opened | PrAction::Updated, head_sha, head_branch, base_branch, title, author, .. } => {
            for s in &stacks {
                let v = s.vcs.as_ref().expect("matched by repo");
                let default = if *base_branch == v.branch { "propose" } else { "ignore" };
                let input = json!({"pull_request": {"number": number, "head_sha": head_sha, "head_branch": head_branch,
                                                    "base_branch": base_branch, "title": title, "author": author},
                                   "stack": stack_input(s)});
                let d = decide(app, s, &input, default)?;
                out.push(json!({"stack": s.name, "space": s.space, "event": "pull_request", "decision": d}));
                if let Some(kind) = kind_of(d) {
                    // A new head supersedes the PR's unfinished runs on this stack.
                    for old in app.db.read(|c| store::runs_for_pr(c, &i.id, repo, *number))?.iter().filter(|r| r.stack_id == s.id) {
                        let _ = stop_run(app, old, true);
                    }
                    let source = json!({"integration": i.id, "repo": repo, "sha": head_sha, "branch": head_branch, "pr": number});
                    spawn_run(app, i, s, kind, head_sha, source, &format!("PR #{number}: {title}"), &by(author));
                }
            }
        }
        Event::PullRequest { repo, number, action, .. } => {
            // Closed or merged: nothing from this PR may keep running or hold a lock.
            let mut stopped = 0;
            for r in app.db.read(|c| store::runs_for_pr(c, &i.id, repo, *number))? {
                if stop_run(app, &r, true)? {
                    stopped += 1;
                }
            }
            out.push(json!({"event": "pull_request", "action": action, "stopped_runs": stopped}));
        }
        Event::Comment { repo, number, body, author } => {
            let Some((cmd, only)) = vcs::parse_command(body) else { return Ok(out) };
            let runs = app.db.read(|c| store::runs_for_pr(c, &i.id, repo, *number))?;
            for s in stacks.iter().filter(|s| only.is_empty() || only.contains(&s.name)) {
                let v = s.vcs.as_ref().expect("matched by repo");
                // The PR head we last saw for this stack.
                let Some(src) = runs.iter().find(|r| r.stack_id == s.id).and_then(|r| r.source.clone()) else {
                    out.push(json!({"stack": s.name, "decision": "ignore", "reason": "no run for this pull request yet"}));
                    continue;
                };
                let default = match cmd.as_str() {
                    "apply" if v.pr_apply => "track",
                    "apply" => "ignore",
                    _ => "propose",
                };
                let input = json!({"comment": {"body": body, "author": author, "command": cmd},
                                   "pull_request": {"number": number, "head_sha": src["sha"], "head_branch": src["branch"]},
                                   "stack": stack_input(s)});
                let d = decide(app, s, &input, default)?;
                out.push(json!({"stack": s.name, "space": s.space, "event": "comment", "command": cmd, "decision": d}));
                if let Some(kind) = kind_of(d) {
                    let sha = src["sha"].as_str().unwrap_or_default().to_string();
                    spawn_run(app, i, s, kind, &sha, src, &format!("PR #{number}: /cairn {cmd} by {author}"), &by(author));
                }
            }
        }
    }
    Ok(out)
}

/// Fetch the source and enqueue the run in the background (webhook senders
/// time out after ~10 s; archives can take longer).
#[allow(clippy::too_many_arguments)]
fn spawn_run(app: &AppRef, i: &Integration, s: &Stack, kind: &'static str, rev: &str, source: Value, message: &str, by: &str) {
    let (app, i, s, rev, message, by) = (app.clone(), i.clone(), s.clone(), rev.to_string(), message.to_string(), by.to_string());
    tokio::spawn(async move {
        if let Err(e) = start_run(&app, &i, &s, kind, &rev, source.clone(), &message, &by).await {
            tracing::warn!(stack = %s.name, "VCS-triggered run failed to start: {}", e.detail());
            if let Some(sha) = source["sha"].as_str() {
                let repo = source["repo"].as_str().unwrap_or_default().to_string();
                let (ctx, sha, msg) = (format!("cairn/{}/{}", s.space, s.name), sha.to_string(), format!("could not start: {}", e.detail()));
                let url = app.public_url();
                let _ = tokio::task::spawn_blocking(move || {
                    let token = app.secrets.open(&i.token)?;
                    vcs::Client { i: &i, token }.status(&repo, &sha, vcs::State::Failure, &ctx, &msg, &url)
                })
                .await;
            }
        }
    });
}

#[allow(clippy::too_many_arguments)]
async fn start_run(
    app: &AppRef, i: &Integration, s: &Stack, kind: &str, rev: &str, source: Value, message: &str, by: &str,
) -> ApiResult<store::Run> {
    let (i2, repo, rev2) = (i.clone(), source["repo"].as_str().unwrap_or_default().to_string(), rev.to_string());
    let token = app.secrets.open(&i.token)?;
    let tarball = tokio::task::spawn_blocking(move || vcs::Client { i: &i2, token }.archive(&repo, &rev2))
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?
        .map_err(|e| ApiError::new(StatusCode::BAD_GATEWAY, format!("{e:#}")))?;
    let cv = app.db.write(|c| store::create_config(c, &s.id, false, kind == "proposed"))?;
    crate::api::native::save_config(app, &cv.id, &Bytes::from(tarball)).await?;
    let auto_apply = s.auto_apply || (kind == "tracked" && source["pr"].is_i64());
    enqueue_run(app, s, kind, &cv.id, auto_apply, message, by, Some(&source))
}

// ---------------------------------------------------------------- reporting

/// Background task: commit statuses and the pull request comment.
pub async fn reporter(app: AppRef) {
    let mut rx = app.hub.events.subscribe();
    loop {
        let e = match rx.recv().await {
            Ok(e) => e,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            Err(_) => return,
        };
        if e.kind != "run.status" {
            continue;
        }
        let (Some(run_id), Some(status)) = (e.run_id.clone(), e.status.clone()) else { continue };
        if let Err(err) = report(&app, &run_id, &status).await {
            tracing::warn!(run = %run_id, "VCS report failed: {err:#}");
        }
    }
}

fn summary(r: &store::Run) -> String {
    match (r.adds, r.changes, r.destroys) {
        (Some(a), Some(c), Some(d)) => format!("+{a} ~{c} -{d}"),
        _ => String::new(),
    }
}

async fn report(app: &AppRef, run_id: &str, status: &str) -> anyhow::Result<()> {
    let Some(run) = app.db.read(|c| store::get_run(c, run_id))? else { return Ok(()) };
    let Some(src) = run.source.clone() else { return Ok(()) };
    let Some(i) = app.db.read(|c| vcs::get(c, src["integration"].as_str().unwrap_or_default()))? else { return Ok(()) };
    let Some(stack) = app.db.read(|c| store::get_stack(c, &run.stack_id))? else { return Ok(()) };
    let repo = src["repo"].as_str().unwrap_or_default().to_string();
    let base = app.public_url();
    let url = format!("{base}/api/v1/runs/{}", run.id);
    let (state, desc) = match status {
        "plan_queued" | "planning" | "apply_queued" | "applying" => (vcs::State::Pending, status.replace('_', " ")),
        "planned" if run.approval.as_deref() == Some("pending") => (vcs::State::Pending, format!("{} awaiting approval", summary(&run))),
        "planned" => (vcs::State::Pending, format!("{} awaiting confirmation", summary(&run))),
        "planned_and_finished" => (vcs::State::Success, if run.has_changes == Some(true) { format!("plan {}", summary(&run)) } else { "no changes".into() }),
        "applied" => (vcs::State::Success, format!("applied {}", summary(&run))),
        "errored" => (vcs::State::Failure, run.error.clone().unwrap_or_else(|| "errored".into())),
        "discarded" | "canceled" => (vcs::State::Failure, status.to_string()),
        _ => return Ok(()),
    };
    let token = app.secrets.open(&i.token)?;
    let context = format!("cairn/{}/{}", stack.space, stack.name);
    let pr = src["pr"].as_i64();
    // Only resting states rewrite the PR comment.
    let comment = match pr {
        Some(n) if matches!(status, "planned" | "planned_and_finished" | "applied" | "errored" | "discarded" | "canceled") => {
            let runs = app.db.read(|c| store::runs_for_pr(c, &i.id, &repo, n))?;
            let existing = app.db.read(|c| vcs::comment_id(c, &i.id, &repo, n))?;
            Some((n, render_comment(app, &runs, &base)?, existing))
        }
        _ => None,
    };
    let (app2, i2, repo2) = (app.clone(), i.clone(), repo.clone());
    let sha = src["sha"].as_str().map(str::to_string);
    tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        let c = vcs::Client { i: &i2, token };
        if let Some(sha) = sha.filter(|s| !s.is_empty()) {
            c.status(&repo2, &sha, state, &context, &desc, &url)?;
        }
        if let Some((n, body, existing)) = comment {
            let id = c.comment(&repo2, n, existing.as_deref(), &body)?;
            if existing.as_deref() != Some(id.as_str()) {
                app2.db.write(|db| vcs::set_comment_id(db, &i2.id, &repo2, n, &id))?;
            }
        }
        Ok(())
    })
    .await??;
    Ok(())
}

/// One comment per pull request: the latest run of every stack it touched.
fn render_comment(app: &AppRef, runs: &[store::Run], base: &str) -> anyhow::Result<String> {
    let mut seen = std::collections::HashSet::new();
    let mut rows = String::new();
    let mut details = String::new();
    for r in runs {
        if !seen.insert(r.stack_id.clone()) {
            continue; // older run of a stack already listed
        }
        let Some(s) = app.db.read(|c| store::get_stack(c, &r.stack_id))? else { continue };
        let icon = match r.status.as_str() {
            "applied" | "planned_and_finished" => "✅",
            "errored" => "❌",
            "planned" => "⏸️",
            "discarded" | "canceled" => "⛔",
            _ => "⏳",
        };
        let sha: String = r.source.as_ref().and_then(|s| s["sha"].as_str()).unwrap_or("").chars().take(8).collect();
        let changes = if r.has_changes == Some(false) { "no changes".to_string() } else { summary(r) };
        rows.push_str(&format!(
            "| `{}/{}` | {icon} {} | {} | `{sha}` | [{}]({base}/api/v1/runs/{}) |\n",
            s.space, s.name, r.status.replace('_', " "), changes, r.id, r.id
        ));
        if let Some(e) = &r.error {
            details.push_str(&format!("\n<details><summary><code>{}</code> error</summary>\n\n```\n{}\n```\n</details>\n", s.name, e.chars().take(2000).collect::<String>()));
        } else if let Some(ps) = r.plan_summary.as_deref().and_then(|p| serde_json::from_str::<Value>(p).ok()) {
            let list: Vec<String> = ps["resources"]
                .as_array()
                .into_iter()
                .flatten()
                .take(50)
                .map(|x| format!("- `{}` {}", x["action"].as_str().unwrap_or(""), x["address"].as_str().unwrap_or("")))
                .collect();
            if !list.is_empty() {
                details.push_str(&format!("\n<details><summary><code>{}</code> changes</summary>\n\n{}\n</details>\n", s.name, list.join("\n")));
            }
        }
    }
    Ok(format!(
        "### Cairn\n\n| Stack | Status | Changes | Commit | Run |\n|---|---|---|---|---|\n{rows}{details}\n\
         <sub>Comment `/cairn plan` to plan again, or `/cairn apply` on stacks that allow applying from pull requests.</sub>\n"
    ))
}
