//! Registry endpoints: the `modules.v1` and `providers.v1` protocols that
//! OpenTofu speaks, the publishing API, and pre-signed downloads.
//!
//! Reading needs `reader` on the namespace's space (runs get a run-scoped
//! token that reads their stack's space and its ancestors). Publishing needs
//! `writer`, deleting `admin`. Archive URLs handed to OpenTofu are
//! pre-signed and expire after an hour, because OpenTofu does not send
//! credentials when it downloads them.

use axum::body::{Body, Bytes};
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use base64::Engine as _;
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::Digest;

use crate::api::identity::find_space;
use crate::app::{ApiError, ApiResult, AppRef};
use crate::auth::{Principal, Role};
use crate::registry::{self, valid_ident, Version};
use crate::store::Space;
use crate::util::{ct_eq, sha256_hex};

const URL_TTL_SECS: u64 = 3600;

fn unix_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Read access to a namespace (space).
fn can_read(app: &AppRef, p: &Principal, space_id: &str) -> bool {
    if p.kind == "run" {
        // Run tokens: the stack's space (p.id) and its ancestors.
        return app.space_chain(&p.id).iter().any(|s| s == space_id);
    }
    app.authz.role(p, space_id).is_some()
}

fn namespace(app: &AppRef, p: &Principal, ns: &str) -> ApiResult<Space> {
    let s = find_space(app, ns)?;
    if !can_read(app, p, &s.id) {
        return Err(ApiError::not_found("namespace"));
    }
    Ok(s)
}

fn presign(app: &AppRef, base: &str, kind: &str, id: &str, file: &str) -> String {
    let exp = unix_now() + URL_TTL_SECS;
    let sig = app.sign(&format!("{exp}/{kind}/{id}"));
    format!("{base}/registry/dl/{exp}/{sig}/{kind}/{id}/{file}")
}

// ---------------------------------------------------------------- protocol: modules.v1

pub async fn module_versions(State(app): State<AppRef>, p: Principal, Path((ns, name, system)): Path<(String, String, String)>) -> ApiResult<Json<Value>> {
    let s = namespace(&app, &p, &ns)?;
    let m = app.db.read(|c| registry::find_module(c, &s.id, &name, &system))?.ok_or_else(|| ApiError::not_found("module"))?;
    let versions: Vec<Value> = m.versions.iter().map(|v| json!({"version": v.version})).collect();
    Ok(Json(json!({"modules": [{"versions": versions}]})))
}

pub async fn module_download(
    State(app): State<AppRef>, p: Principal, headers: HeaderMap,
    Path((ns, name, system, version)): Path<(String, String, String, String)>,
) -> ApiResult<Response> {
    let s = namespace(&app, &p, &ns)?;
    let m = app.db.read(|c| registry::find_module(c, &s.id, &name, &system))?.ok_or_else(|| ApiError::not_found("module"))?;
    let v = m.versions.iter().find(|v| v.version == version).ok_or_else(|| ApiError::not_found("module version"))?;
    let url = presign(&app, &app.base_url(&headers), "module", &v.id, &format!("{name}-{system}-{version}.tar.gz"));
    Ok((StatusCode::NO_CONTENT, [("X-Terraform-Get", url)]).into_response())
}

// ---------------------------------------------------------------- protocol: providers.v1

pub async fn provider_versions(State(app): State<AppRef>, p: Principal, Path((ns, kind)): Path<(String, String)>) -> ApiResult<Json<Value>> {
    let s = namespace(&app, &p, &ns)?;
    let pr = app.db.read(|c| registry::find_provider(c, &s.id, &kind))?.ok_or_else(|| ApiError::not_found("provider"))?;
    let versions: Vec<Value> = pr
        .versions
        .iter()
        .filter(|v| !v.platforms.is_empty())
        .map(|v| {
            json!({"version": v.version, "protocols": v.protocols,
                   "platforms": v.platforms.iter().map(|p| json!({"os": p.os, "arch": p.arch})).collect::<Vec<_>>()})
        })
        .collect();
    Ok(Json(json!({"id": format!("{ns}/{kind}"), "versions": versions, "warnings": null})))
}

pub async fn provider_download(
    State(app): State<AppRef>, p: Principal, headers: HeaderMap,
    Path((ns, kind, version, os, arch)): Path<(String, String, String, String, String)>,
) -> ApiResult<Json<Value>> {
    let s = namespace(&app, &p, &ns)?;
    let pr = app.db.read(|c| registry::find_provider(c, &s.id, &kind))?.ok_or_else(|| ApiError::not_found("provider"))?;
    let v = pr.versions.iter().find(|v| v.version == version).ok_or_else(|| ApiError::not_found("provider version"))?;
    let pl = v.platforms.iter().find(|x| x.os == os && x.arch == arch).ok_or_else(|| ApiError::not_found("platform"))?;
    let base = app.base_url(&headers);
    let prefix = format!("terraform-provider-{kind}_{version}");
    Ok(Json(json!({
        "protocols": v.protocols,
        "os": pl.os,
        "arch": pl.arch,
        "filename": pl.filename,
        "download_url": presign(&app, &base, "provider", &pl.id, &pl.filename),
        "shasums_url": presign(&app, &base, "shasums", &v.id, &format!("{prefix}_SHA256SUMS")),
        "shasums_signature_url": presign(&app, &base, "shasums-sig", &v.id, &format!("{prefix}_SHA256SUMS.sig")),
        "shasum": pl.sha256,
        "signing_keys": {"gpg_public_keys": [{"key_id": v.key_id, "ascii_armor": v.ascii_armor,
                                              "trust_signature": "", "source": "Cairn", "source_url": null}]},
    })))
}

/// `GET /registry/dl/{exp}/{sig}/{kind}/{id}/{file}` (public; pre-signed).
pub async fn download(State(app): State<AppRef>, Path((exp, sig, kind, id, _file)): Path<(u64, String, String, String, String)>) -> ApiResult<Response> {
    if exp < unix_now() || !ct_eq(sig.as_bytes(), app.sign(&format!("{exp}/{kind}/{id}")).as_bytes()) {
        return Err(ApiError::new(StatusCode::FORBIDDEN, "invalid or expired download URL"));
    }
    let (bytes, ctype) = match kind.as_str() {
        "module" => (app.blobs.get(&format!("registry/modules/{id}.tar.gz")).await?, "application/gzip"),
        "provider" => (app.blobs.get(&format!("registry/providers/{id}.zip")).await?, "application/zip"),
        "shasums" => (app.db.read(|c| registry::shasums(c, &id))?.map(|(s, _)| s.into_bytes()), "text/plain"),
        "shasums-sig" => (app.db.read(|c| registry::shasums(c, &id))?.map(|(_, sig)| sig), "application/octet-stream"),
        _ => (None, ""),
    };
    let bytes = bytes.ok_or_else(|| ApiError::not_found("file"))?;
    Ok(Response::builder().header(header::CONTENT_TYPE, ctype).body(Body::from(bytes)).expect("valid response"))
}

// ---------------------------------------------------------------- publishing: modules

fn check_ids(ids: &[&str]) -> ApiResult<()> {
    for i in ids {
        if !valid_ident(i) {
            return Err(ApiError::bad_request(format!("{i:?}: use lowercase letters, digits, - and _")));
        }
    }
    Ok(())
}

fn version(v: &str) -> ApiResult<String> {
    Version::parse(v).map(|v| v.to_string()).map_err(|e| ApiError::bad_request(e.to_string()))
}

fn readable(app: &AppRef, p: &Principal) -> impl Fn(&str) -> bool {
    let allowed = app.authz.readable_spaces(p);
    move |space_id: &str| allowed.as_ref().map(|a| a.contains(space_id)).unwrap_or(true)
}

pub async fn list_modules(State(app): State<AppRef>, p: Principal) -> ApiResult<Json<Value>> {
    let ok = readable(&app, &p);
    let items: Vec<_> = app.db.read(registry::list_modules)?.into_iter().filter(|m| ok(&m.space_id)).collect();
    Ok(Json(json!({"items": items})))
}

pub async fn get_module(State(app): State<AppRef>, p: Principal, Path((ns, name, system)): Path<(String, String, String)>) -> ApiResult<Json<Value>> {
    let s = namespace(&app, &p, &ns)?;
    let m = app.db.read(|c| registry::find_module(c, &s.id, &name, &system))?.ok_or_else(|| ApiError::not_found("module"))?;
    let mut v = registry::module_json(&m);
    v["source"] = json!(format!("{}/{}/{}/{}", app.registry_host(), m.namespace, m.name, m.system));
    Ok(Json(v))
}

pub async fn publish_module(
    State(app): State<AppRef>, p: Principal,
    Path((ns, name, system, ver)): Path<(String, String, String, String)>,
    body: Bytes,
) -> ApiResult<impl IntoResponse> {
    check_ids(&[&name, &system])?;
    let ver = version(&ver)?;
    let s = find_space(&app, &ns)?;
    app.authz.require(&p, &s.id, Role::Writer)?;
    if body.len() < 2 || body[0] != 0x1f || body[1] != 0x8b {
        return Err(ApiError::bad_request("module versions are uploaded as .tar.gz archives"));
    }
    let sha = sha256_hex(&body);
    let id = app.db.write(|c| registry::add_module_version(c, &s.id, &name, &system, &ver, &sha, body.len() as i64))?;
    if let Err(e) = app.blobs.put(&format!("registry/modules/{id}.tar.gz"), body.to_vec(), true).await {
        let _ = app.db.write(|c| {
            let m = registry::find_module(c, &s.id, &name, &system)?;
            if let Some(m) = m {
                registry::delete_module_version(c, &m.id, &ver)?;
            }
            Ok(())
        });
        return Err(e.into());
    }
    Ok((StatusCode::CREATED, Json(json!({"id": id, "namespace": s.name, "name": name, "system": system, "version": ver, "sha256": sha,
                                         "source": format!("{}/{}/{name}/{system}", app.registry_host(), s.name)}))))
}

pub async fn delete_module(State(app): State<AppRef>, p: Principal, Path((ns, name, system)): Path<(String, String, String)>) -> ApiResult<StatusCode> {
    let s = find_space(&app, &ns)?;
    app.authz.require(&p, &s.id, Role::Admin)?;
    let m = app.db.read(|c| registry::find_module(c, &s.id, &name, &system))?.ok_or_else(|| ApiError::not_found("module"))?;
    for id in app.db.write(|c| registry::delete_module(c, &m.id))? {
        let _ = app.blobs.delete(&format!("registry/modules/{id}.tar.gz")).await;
    }
    Ok(StatusCode::NO_CONTENT)
}

pub async fn delete_module_version(
    State(app): State<AppRef>, p: Principal, Path((ns, name, system, ver)): Path<(String, String, String, String)>,
) -> ApiResult<StatusCode> {
    let s = find_space(&app, &ns)?;
    app.authz.require(&p, &s.id, Role::Admin)?;
    let m = app.db.read(|c| registry::find_module(c, &s.id, &name, &system))?.ok_or_else(|| ApiError::not_found("module"))?;
    let id = app.db.write(|c| registry::delete_module_version(c, &m.id, &ver))?.ok_or_else(|| ApiError::not_found("module version"))?;
    let _ = app.blobs.delete(&format!("registry/modules/{id}.tar.gz")).await;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------- publishing: providers

pub async fn list_providers(State(app): State<AppRef>, p: Principal) -> ApiResult<Json<Value>> {
    let ok = readable(&app, &p);
    let items: Vec<_> = app.db.read(registry::list_providers)?.into_iter().filter(|x| ok(&x.space_id)).collect();
    Ok(Json(json!({"items": items})))
}

pub async fn get_provider(State(app): State<AppRef>, p: Principal, Path((ns, kind)): Path<(String, String)>) -> ApiResult<Json<Value>> {
    let s = namespace(&app, &p, &ns)?;
    let pr = app.db.read(|c| registry::find_provider(c, &s.id, &kind))?.ok_or_else(|| ApiError::not_found("provider"))?;
    let mut v = serde_json::to_value(&pr).unwrap_or(Value::Null);
    v["source"] = json!(format!("{}/{}/{}", app.registry_host(), pr.namespace, pr.kind));
    Ok(Json(v))
}

#[derive(Deserialize)]
pub struct NewProviderVersionReq {
    #[serde(default = "default_protocols")]
    protocols: Vec<String>,
    /// ASCII-armored GPG public key that signed SHA256SUMS.
    gpg_public_key: String,
    /// Key ID (derived from the key when omitted).
    #[serde(default)]
    key_id: Option<String>,
    /// Contents of `SHA256SUMS`.
    shasums: String,
    /// `SHA256SUMS.sig` (binary detached signature), base64.
    shasums_sig: String,
}

fn default_protocols() -> Vec<String> {
    vec!["5.0".into()]
}

pub async fn create_provider_version(
    State(app): State<AppRef>, p: Principal, Path((ns, kind, ver)): Path<(String, String, String)>, Json(r): Json<NewProviderVersionReq>,
) -> ApiResult<impl IntoResponse> {
    check_ids(&[&kind])?;
    let ver = version(&ver)?;
    let s = find_space(&app, &ns)?;
    app.authz.require(&p, &s.id, Role::Writer)?;
    let sig = base64::engine::general_purpose::STANDARD
        .decode(r.shasums_sig.trim())
        .map_err(|_| ApiError::bad_request("shasums_sig must be base64"))?;
    let key_id = match r.key_id.filter(|k| !k.is_empty()) {
        Some(k) => k.to_uppercase(),
        None => crate::registry_gpg::key_id(&r.gpg_public_key).map_err(|e| ApiError::bad_request(format!("gpg_public_key: {e:#}")))?,
    };
    if r.shasums.lines().filter(|l| !l.trim().is_empty()).count() == 0 {
        return Err(ApiError::bad_request("shasums is empty"));
    }
    let id = app.db.write(|c| {
        registry::add_provider_version(
            c,
            &registry::NewProviderVersion {
                space_id: &s.id,
                kind: &kind,
                version: &ver,
                protocols: &r.protocols,
                key_id: &key_id,
                ascii_armor: &r.gpg_public_key,
                shasums: &r.shasums,
                shasums_sig: &sig,
            },
        )
    })?;
    Ok((StatusCode::CREATED, Json(json!({"id": id, "namespace": s.name, "type": kind, "version": ver, "key_id": key_id}))))
}

pub async fn upload_platform(
    State(app): State<AppRef>, p: Principal,
    Path((ns, kind, ver, os, arch)): Path<(String, String, String, String, String)>,
    body: Bytes,
) -> ApiResult<impl IntoResponse> {
    check_ids(&[&os, &arch])?;
    let s = find_space(&app, &ns)?;
    app.authz.require(&p, &s.id, Role::Writer)?;
    let pr = app.db.read(|c| registry::find_provider(c, &s.id, &kind))?.ok_or_else(|| ApiError::not_found("provider"))?;
    let v = pr.versions.iter().find(|v| v.version == ver).ok_or_else(|| ApiError::not_found("provider version"))?;
    let filename = format!("terraform-provider-{kind}_{ver}_{os}_{arch}.zip");
    let want = registry::sum_for(&v.shasums, &filename)
        .ok_or_else(|| ApiError::bad_request(format!("{filename} is not listed in SHA256SUMS")))?;
    let got = hex::encode(sha2::Sha256::digest(&body));
    if got != want {
        return Err(ApiError::bad_request(format!("{filename}: sha256 {got} does not match SHA256SUMS ({want})")));
    }
    let id = app.db.write(|c| registry::add_platform(c, &v.id, &os, &arch, &filename, &got, body.len() as i64))?;
    app.blobs.put(&format!("registry/providers/{id}.zip"), body.to_vec(), true).await?;
    Ok((StatusCode::CREATED, Json(json!({"id": id, "filename": filename, "sha256": got}))))
}

pub async fn delete_provider_version(
    State(app): State<AppRef>, p: Principal, Path((ns, kind, ver)): Path<(String, String, String)>,
) -> ApiResult<StatusCode> {
    let s = find_space(&app, &ns)?;
    app.authz.require(&p, &s.id, Role::Admin)?;
    let pr = app.db.read(|c| registry::find_provider(c, &s.id, &kind))?.ok_or_else(|| ApiError::not_found("provider"))?;
    for id in app.db.write(|c| registry::delete_provider_version(c, &pr.id, &ver))? {
        let _ = app.blobs.delete(&format!("registry/providers/{id}.zip")).await;
    }
    Ok(StatusCode::NO_CONTENT)
}
