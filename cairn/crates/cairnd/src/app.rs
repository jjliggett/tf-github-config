//! Shared application state, configuration, auth and API errors.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use clap::Parser;

use crate::crypto::SecretBox;
use crate::db::Db;
use crate::engine::Engines;
use crate::events::Hub;
use crate::runner::Scheduler;
use crate::util::{ct_eq, sha256_hex};

#[derive(Parser, Debug, Clone)]
#[command(name = "cairnd", version, about = "Cairn server: lightweight OpenTofu automation")]
pub struct Config {
    /// Address to listen on.
    #[arg(long, env = "CAIRN_LISTEN", default_value = "127.0.0.1:8833")]
    pub listen: SocketAddr,
    /// Directory for the database, blobs, logs, engines and run workspaces.
    #[arg(long, env = "CAIRN_DATA", default_value = "./cairn-data")]
    pub data: PathBuf,
    /// Admin token accepted in addition to tokens created via the API.
    #[arg(long, env = "CAIRN_SITE_TOKEN", hide_env_values = true)]
    pub site_token: Option<String>,
    /// Use this OpenTofu binary for every run (otherwise resolve per stack version).
    #[arg(long, env = "CAIRN_TOFU_BIN")]
    pub tofu_bin: Option<PathBuf>,
    /// Engine version for stacks that don't set one.
    #[arg(long, env = "CAIRN_DEFAULT_ENGINE_VERSION", default_value = "1.11.6")]
    pub default_engine_version: String,
    /// Never download engines; they must be in <data>/engines/<version>/tofu.
    #[arg(long, env = "CAIRN_NO_DOWNLOAD", num_args = 0..=1, default_missing_value = "true", value_parser = clap::builder::BoolishValueParser::new())]
    pub no_download: bool,
    /// Maximum plan/apply phases running at once (default: usable CPUs, at least 2).
    #[arg(long, env = "CAIRN_CONCURRENCY")]
    pub concurrency: Option<usize>,
    /// Public base URL used in links handed to clients (default: from the Host header).
    #[arg(long, env = "CAIRN_EXTERNAL_URL")]
    pub external_url: Option<String>,
    /// Hex 32-byte key for secrets at rest (default: <data>/secret.key).
    #[arg(long, env = "CAIRN_SECRET_KEY", hide_env_values = true)]
    pub secret_key: Option<String>,
    /// Keep run workspaces after runs finish (debugging).
    #[arg(long, env = "CAIRN_KEEP_WORKSPACES", num_args = 0..=1, default_missing_value = "true", value_parser = clap::builder::BoolishValueParser::new())]
    pub keep_workspaces: bool,
    /// GOGC for OpenTofu processes. 50 cuts tofu's heap memory ~30% for ~20%
    /// more CPU, which suits small hosts; 0 leaves Go's default (100). A
    /// stack's own `GOGC` env variable wins.
    #[arg(long, env = "CAIRN_ENGINE_GOGC", default_value_t = 50)]
    pub engine_gogc: u32,
    /// Where state, configurations and registry artifacts live: file:///path (default
    /// <data>/blobs), s3://bucket/prefix?region=…&endpoint=…, or azblob://account/container/prefix.
    #[arg(long, env = "CAIRN_BLOB_STORE")]
    pub blob_store: Option<String>,
    /// Store state and configurations unencrypted (they are encrypted at rest by default).
    #[arg(long, env = "CAIRN_NO_BLOB_ENCRYPTION", num_args = 0..=1, default_missing_value = "true", value_parser = clap::builder::BoolishValueParser::new())]
    pub no_blob_encryption: bool,
    /// Serve HTTPS with this PEM certificate chain (needs --tls-key).
    #[arg(long, env = "CAIRN_TLS_CERT", requires = "tls_key")]
    pub tls_cert: Option<PathBuf>,
    #[arg(long, env = "CAIRN_TLS_KEY", requires = "tls_cert")]
    pub tls_key: Option<PathBuf>,
    /// OpenID Connect issuer URL (e.g. https://authentik.example.com/application/o/cairn/).
    #[arg(long, env = "CAIRN_OIDC_ISSUER")]
    pub oidc_issuer: Option<String>,
    #[arg(long, env = "CAIRN_OIDC_CLIENT_ID", default_value = "cairn")]
    pub oidc_client_id: String,
    #[arg(long, env = "CAIRN_OIDC_CLIENT_SECRET", hide_env_values = true)]
    pub oidc_client_secret: Option<String>,
    /// Claim holding group names; groups match team role bindings by name.
    #[arg(long, env = "CAIRN_OIDC_GROUPS_CLAIM", default_value = "groups")]
    pub oidc_groups_claim: String,
    /// Members of this IdP group are site admins.
    #[arg(long, env = "CAIRN_OIDC_ADMIN_GROUP")]
    pub oidc_admin_group: Option<String>,
    #[arg(long, env = "CAIRN_OIDC_SCOPES", default_value = "openid email profile groups")]
    pub oidc_scopes: String,
    /// Extra accepted audiences for bearer ID tokens (comma-separated), e.g. for CI.
    #[arg(long, env = "CAIRN_OIDC_AUDIENCES", value_delimiter = ',')]
    pub oidc_audiences: Vec<String>,
    /// Lifetime of tokens minted by an OIDC login, in hours.
    #[arg(long, env = "CAIRN_SESSION_HOURS", default_value_t = 12)]
    pub session_hours: u64,
    /// Allow notification webhooks to private/loopback addresses (SSRF protection off).
    #[arg(long, env = "CAIRN_ALLOW_PRIVATE_WEBHOOKS", num_args = 0..=1, default_missing_value = "true", value_parser = clap::builder::BoolishValueParser::new())]
    pub allow_private_webhooks: bool,
    /// Tokio worker threads (default: min(CPUs, 4)).
    #[arg(long, env = "CAIRN_WORKERS")]
    pub workers: Option<usize>,
}

#[derive(Default)]
pub struct Metrics {
    pub runs_created: AtomicU64,
    pub phase_runs: Mutex<HashMap<&'static str, (u64, f64)>>,
    pub finished: Mutex<HashMap<String, u64>>,
    pub http_requests: AtomicU64,
}

impl Metrics {
    pub fn phase(&self, phase: &'static str, secs: f64) {
        let mut m = self.phase_runs.lock().unwrap();
        let e = m.entry(phase).or_default();
        e.0 += 1;
        e.1 += secs;
    }
    pub fn finished(&self, status: &str) {
        *self.finished.lock().unwrap().entry(status.to_string()).or_default() += 1;
    }
}

pub struct App {
    pub cfg: Config,
    pub db: Db,
    pub hub: Hub,
    pub engines: Engines,
    pub secrets: Arc<SecretBox>,
    pub blobs: crate::blob::BlobStore,
    pub sched: Scheduler,
    pub metrics: Metrics,
    /// Roles, bindings, users and tokens (in-memory snapshot).
    pub authz: crate::auth::Authz,
    pub oidc: Option<crate::oidc::Oidc>,
    pub policies: crate::policy::PolicyCache,
    /// Verified bearer ID tokens: sha256 -> (principal, valid until).
    pub jwt_cache: Mutex<HashMap<String, (crate::auth::Principal, std::time::Instant)>>,
    /// Run-scoped registry tokens: sha256 -> (principal, valid until).
    pub run_tokens: Mutex<HashMap<String, (crate::auth::Principal, std::time::Instant)>>,
    /// Key for signing pre-authorized upload URLs (process lifetime).
    pub url_key: [u8; 32],
}

pub type AppRef = Arc<App>;

impl App {
    pub fn dir(&self, sub: &str) -> PathBuf {
        self.cfg.data.join(sub)
    }

    /// Resolve a bearer credential: site token, Cairn API token, or (with
    /// OIDC configured) an ID token from the issuer.
    pub async fn principal(self: &Arc<Self>, token: &str) -> Option<crate::auth::Principal> {
        use crate::auth::{principal_for_token, principal_for_user, Principal};
        if let Some(site) = &self.cfg.site_token {
            if ct_eq(site.as_bytes(), token.as_bytes()) {
                return Some(Principal::site());
            }
        }
        if token.starts_with("cairn_run_") {
            let p = self.run_tokens.lock().unwrap().get(&sha256_hex(token.as_bytes())).cloned();
            return p.filter(|(_, until)| *until > std::time::Instant::now()).map(|(p, _)| p);
        }
        if let Some((t, owner)) = self.authz.token(&sha256_hex(token.as_bytes())) {
            return Some(principal_for_token(&t, owner));
        }
        if self.oidc.is_some() && token.matches('.').count() == 2 {
            let key = sha256_hex(token.as_bytes());
            if let Some((p, until)) = self.jwt_cache.lock().unwrap().get(&key) {
                if *until > std::time::Instant::now() {
                    return Some(p.clone());
                }
            }
            let app = self.clone();
            let tok = token.to_string();
            let id = tokio::task::spawn_blocking(move || app.oidc.as_ref().unwrap().verify(&tok)).await.ok()?;
            let id = match id {
                Ok(id) => id,
                Err(e) => {
                    tracing::debug!("rejected ID token: {e:#}");
                    return None;
                }
            };
            let user = self.login(&id).ok()?;
            let p = principal_for_user(&user);
            let mut cache = self.jwt_cache.lock().unwrap();
            if cache.len() > 10_000 {
                cache.clear();
            }
            let left = id.exp.saturating_sub(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0));
            cache.insert(key, (p.clone(), std::time::Instant::now() + std::time::Duration::from_secs(left.min(300))));
            return Some(p);
        }
        None
    }

    /// Record an OIDC login: apply `login` policies, upsert the user.
    pub fn login(&self, id: &crate::oidc::Identity) -> anyhow::Result<crate::auth::User> {
        use crate::auth::{upsert_user, Role};
        let pols = self.db.read(|c| crate::policy::list(c, Some("login")))?;
        let mut site_admin = id.site_admin;
        let mut roles: HashMap<String, Role> = HashMap::new();
        if !pols.is_empty() {
            let input = serde_json::json!({"session": {"subject": id.subject, "email": id.email, "name": id.name,
                                                       "groups": id.groups, "issuer": self.cfg.oidc_issuer}});
            let verdicts = self.policies.run(&pols, &input)?;
            if verdicts.iter().any(|v| v.flag("deny")) {
                anyhow::bail!("login denied by policy");
            }
            site_admin |= verdicts.iter().any(|v| v.flag("admin"));
            let snap = self.authz.snapshot();
            for v in &verdicts {
                if let Some(serde_json::Value::Object(m)) = v.result.get("space_role") {
                    for (space, role) in m {
                        let sid = snap.spaces.iter().find(|(sid, (name, _))| *sid == space || name == space).map(|(sid, _)| sid.clone());
                        if let (Some(sid), Some(r)) = (sid, role.as_str().and_then(Role::parse)) {
                            let e = roles.entry(sid).or_insert(r);
                            *e = (*e).max(r);
                        }
                    }
                }
            }
        }
        let (user, changed) = self.db.write(|c| upsert_user(c, &id.subject, &id.email, &id.name, &id.groups, site_admin, &roles))?;
        if changed {
            self.authz.reload(&self.db)?;
        }
        Ok(user)
    }

    /// A space and all its ancestors (nearest first).
    pub fn space_chain(&self, space_id: &str) -> Vec<String> {
        let snap = self.authz.snapshot();
        let mut out = vec![];
        let mut cur = Some(space_id.to_string());
        while let Some(id) = cur {
            if out.len() > 64 || out.contains(&id) {
                break;
            }
            cur = snap.spaces.get(&id).and_then(|(_, p)| p.clone());
            out.push(id);
        }
        out
    }

    pub fn sign(&self, what: &str) -> String {
        let mut m = self.url_key.to_vec();
        m.extend_from_slice(what.as_bytes());
        sha256_hex(&m)[..32].to_string()
    }

    /// Host (and port) OpenTofu uses for this server's registry: from
    /// `--external-url`, else the listen address.
    pub fn registry_host(&self) -> String {
        match &self.cfg.external_url {
            Some(u) => u.split("://").nth(1).unwrap_or(u).split('/').next().unwrap_or("").to_string(),
            None => self.cfg.listen.to_string(),
        }
    }

    /// Mint a token for one run: reads the registry in its stack's space and
    /// ancestors, nothing else. Revoked when the run finishes.
    pub fn mint_run_token(&self, run_id: &str, stack_space_id: &str) -> String {
        let token = format!("cairn_run_{}", crate::util::new_token());
        let p = crate::auth::Principal {
            kind: "run",
            id: stack_space_id.to_string(),
            name: run_id.to_string(),
            groups: vec![],
            site_admin: false,
            policy_roles: HashMap::new(),
        };
        let mut m = self.run_tokens.lock().unwrap();
        let now = std::time::Instant::now();
        m.retain(|_, (p, until)| *until > now && p.name != run_id);
        m.insert(sha256_hex(token.as_bytes()), (p, now + std::time::Duration::from_secs(12 * 3600)));
        token
    }

    pub fn revoke_run_tokens(&self, run_id: &str) {
        self.run_tokens.lock().unwrap().retain(|_, (p, _)| p.name != run_id);
    }

    /// URL for links that leave the request context (commit statuses, PR comments).
    pub fn public_url(&self) -> String {
        match &self.cfg.external_url {
            Some(u) => u.trim_end_matches('/').to_string(),
            None => format!("http://{}", self.cfg.listen),
        }
    }

    pub fn base_url(&self, headers: &HeaderMap) -> String {
        if let Some(u) = &self.cfg.external_url {
            return u.trim_end_matches('/').to_string();
        }
        let host = headers.get(header::HOST).and_then(|h| h.to_str().ok()).unwrap_or("localhost");
        let proto = headers.get("x-forwarded-proto").and_then(|h| h.to_str().ok()).unwrap_or("http");
        format!("{proto}://{host}")
    }

    pub fn count_request(&self) {
        self.metrics.http_requests.fetch_add(1, Ordering::Relaxed);
    }
}

/// Extract a bearer token, or the password of HTTP basic auth (what the
/// OpenTofu `http` backend sends).
pub fn token_from(headers: &HeaderMap) -> Option<String> {
    let v = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    if let Some(t) = v.strip_prefix("Bearer ") {
        return Some(t.trim().to_string());
    }
    if let Some(b) = v.strip_prefix("Basic ") {
        let raw = base64_decode(b.trim())?;
        let s = String::from_utf8(raw).ok()?;
        return s.split_once(':').map(|(_, p)| p.to_string());
    }
    None
}

fn base64_decode(s: &str) -> Option<Vec<u8>> {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut buf, mut bits) = (0u32, 0u32);
    for c in s.bytes().filter(|&c| c != b'=') {
        let v = T.iter().position(|&t| t == c)? as u32;
        buf = (buf << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    Some(out)
}

// ---------------------------------------------------------------- errors

/// `application/problem+json` error (RFC 9457).
#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub title: String,
    pub detail: Option<serde_json::Value>,
}

impl ApiError {
    pub fn new(status: StatusCode, title: impl Into<String>) -> Self {
        ApiError { status, title: title.into(), detail: None }
    }
    pub fn not_found(what: &str) -> Self {
        Self::new(StatusCode::NOT_FOUND, format!("{what} not found"))
    }
    pub fn bad_request(msg: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, msg)
    }
    pub fn conflict(msg: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, msg)
    }
    pub fn internal(msg: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, msg)
    }
    /// Human-readable message (for logs and commit statuses).
    pub fn detail(&self) -> String {
        self.title.clone()
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        let msg = format!("{e:#}");
        if msg.contains("UNIQUE constraint failed") {
            return ApiError::conflict("already exists");
        }
        tracing::error!(error = %msg, "request failed");
        ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, msg)
    }
}

impl From<rusqlite::Error> for ApiError {
    fn from(e: rusqlite::Error) -> Self {
        anyhow::Error::from(e).into()
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut body = serde_json::json!({
            "type": format!("https://cairn.dev/errors/{}", self.status.as_u16()),
            "title": self.title,
            "status": self.status.as_u16(),
            // JSON:API-style errors too, so TFE clients print something useful.
            "errors": [{ "status": self.status.as_u16().to_string(), "title": self.title }],
        });
        if let Some(d) = self.detail {
            body["detail"] = d;
        }
        (self.status, [(header::CONTENT_TYPE, "application/problem+json")], body.to_string()).into_response()
    }
}

pub type ApiResult<T> = std::result::Result<T, ApiError>;


