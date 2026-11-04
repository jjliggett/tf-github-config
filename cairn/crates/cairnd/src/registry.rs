//! Private module and provider registry (storage side).
//!
//! Namespaces are spaces: `<host>/<space>/<name>/<system>` for modules and
//! `<host>/<space>/<type>` for providers. Module versions are `.tar.gz`
//! archives; provider versions follow the public registry's release layout
//! (per-platform zips, `SHA256SUMS`, its detached GPG signature and the
//! public key), so a goreleaser `dist/` directory publishes as is.

use std::cmp::Ordering;

use anyhow::{bail, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use serde_json::Value;

use crate::util::{new_id, now};

/// Semantic version (`1.2.3`, `1.2.3-rc.1`; build metadata ignored).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    pub pre: String,
}

impl Version {
    pub fn parse(s: &str) -> Result<Version> {
        let s = s.strip_prefix('v').unwrap_or(s);
        let core_pre = s.split('+').next().unwrap_or("");
        let (core, pre) = core_pre.split_once('-').unwrap_or((core_pre, ""));
        let n: Vec<u64> = core.split('.').map(|p| p.parse::<u64>()).collect::<Result<_, _>>().map_err(|_| anyhow::anyhow!("invalid version {s:?}"))?;
        if n.len() != 3 || (!pre.is_empty() && !pre.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')) {
            bail!("invalid version {s:?}: use MAJOR.MINOR.PATCH[-PRERELEASE]");
        }
        Ok(Version { major: n[0], minor: n[1], patch: n[2], pre: pre.to_string() })
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        if !self.pre.is_empty() {
            write!(f, "-{}", self.pre)?;
        }
        Ok(())
    }
}

impl Ord for Version {
    fn cmp(&self, o: &Self) -> Ordering {
        (self.major, self.minor, self.patch)
            .cmp(&(o.major, o.minor, o.patch))
            .then_with(|| match (self.pre.is_empty(), o.pre.is_empty()) {
                (true, true) => Ordering::Equal,
                (true, false) => Ordering::Greater,
                (false, true) => Ordering::Less,
                (false, false) => cmp_pre(&self.pre, &o.pre),
            })
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}

fn cmp_pre(a: &str, b: &str) -> Ordering {
    for (x, y) in a.split('.').zip(b.split('.')) {
        let o = match (x.parse::<u64>(), y.parse::<u64>()) {
            (Ok(i), Ok(j)) => i.cmp(&j),
            (Ok(_), Err(_)) => Ordering::Less,
            (Err(_), Ok(_)) => Ordering::Greater,
            _ => x.cmp(y),
        };
        if o != Ordering::Equal {
            return o;
        }
    }
    a.split('.').count().cmp(&b.split('.').count())
}

pub fn valid_ident(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

fn sort_versions<T>(items: &mut [T], key: impl Fn(&T) -> &str) {
    items.sort_by(|a, b| match (Version::parse(key(a)), Version::parse(key(b))) {
        (Ok(x), Ok(y)) => y.cmp(&x),
        _ => key(b).cmp(key(a)),
    });
}

// ---------------------------------------------------------------- modules

#[derive(Debug, Clone, Serialize)]
pub struct ModuleVersion {
    pub id: String,
    pub version: String,
    pub sha256: String,
    pub size: i64,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Module {
    pub id: String,
    pub space_id: String,
    pub namespace: String,
    pub name: String,
    pub system: String,
    pub description: String,
    pub created_at: String,
    pub versions: Vec<ModuleVersion>,
}

pub fn find_module(c: &Connection, space_id: &str, name: &str, system: &str) -> Result<Option<Module>> {
    let m = c
        .prepare_cached(
            "SELECT m.id, m.space_id, sp.name, m.name, m.system, m.description, m.created_at FROM registry_modules m
             JOIN spaces sp ON sp.id = m.space_id WHERE m.space_id = ?1 AND m.name = ?2 AND m.system = ?3",
        )?
        .query_row(params![space_id, name, system], |r| {
            Ok(Module {
                id: r.get(0)?,
                space_id: r.get(1)?,
                namespace: r.get(2)?,
                name: r.get(3)?,
                system: r.get(4)?,
                description: r.get(5)?,
                created_at: r.get(6)?,
                versions: vec![],
            })
        })
        .optional()?;
    let Some(mut m) = m else { return Ok(None) };
    let mut st = c.prepare_cached("SELECT id, version, sha256, size, created_at FROM registry_module_versions WHERE module_id = ?1")?;
    m.versions = st
        .query_map([&m.id], |r| Ok(ModuleVersion { id: r.get(0)?, version: r.get(1)?, sha256: r.get(2)?, size: r.get(3)?, created_at: r.get(4)? }))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    sort_versions(&mut m.versions, |v| &v.version);
    Ok(Some(m))
}

pub fn list_modules(c: &Connection) -> Result<Vec<Module>> {
    let keys: Vec<(String, String, String)> = c
        .prepare_cached("SELECT space_id, name, system FROM registry_modules ORDER BY name, system")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let mut out = vec![];
    for (s, n, sys) in keys {
        out.extend(find_module(c, &s, &n, &sys)?);
    }
    Ok(out)
}

/// Record a module version (creating the module); returns the version ID.
pub fn add_module_version(c: &Connection, space_id: &str, name: &str, system: &str, version: &str, sha256: &str, size: i64) -> Result<String> {
    let t = now();
    c.execute(
        "INSERT INTO registry_modules (id, space_id, name, system, created_at) VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT (space_id, name, system) DO NOTHING",
        params![new_id("mod"), space_id, name, system, t],
    )?;
    let module_id: String = c.query_row(
        "SELECT id FROM registry_modules WHERE space_id = ?1 AND name = ?2 AND system = ?3",
        params![space_id, name, system],
        |r| r.get(0),
    )?;
    let id = new_id("modv");
    c.execute(
        "INSERT INTO registry_module_versions (id, module_id, version, sha256, size, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![id, module_id, version, sha256, size, t],
    )?;
    Ok(id)
}

pub fn delete_module_version(c: &Connection, module_id: &str, version: &str) -> Result<Option<String>> {
    let id: Option<String> = c
        .query_row("SELECT id FROM registry_module_versions WHERE module_id = ?1 AND version = ?2", params![module_id, version], |r| r.get(0))
        .optional()?;
    if let Some(id) = &id {
        c.execute("DELETE FROM registry_module_versions WHERE id = ?1", [id])?;
    }
    Ok(id)
}

pub fn delete_module(c: &Connection, module_id: &str) -> Result<Vec<String>> {
    let ids: Vec<String> = c
        .prepare("SELECT id FROM registry_module_versions WHERE module_id = ?1")?
        .query_map([module_id], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    c.execute("DELETE FROM registry_modules WHERE id = ?1", [module_id])?;
    Ok(ids)
}

// ---------------------------------------------------------------- providers

#[derive(Debug, Clone, Serialize)]
pub struct Platform {
    pub id: String,
    pub os: String,
    pub arch: String,
    pub filename: String,
    pub sha256: String,
    pub size: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProviderVersion {
    pub id: String,
    pub version: String,
    pub protocols: Vec<String>,
    pub key_id: String,
    #[serde(skip)]
    pub ascii_armor: String,
    #[serde(skip)]
    pub shasums: String,
    pub platforms: Vec<Platform>,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Provider {
    pub id: String,
    pub space_id: String,
    pub namespace: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub created_at: String,
    pub versions: Vec<ProviderVersion>,
}

pub fn find_provider(c: &Connection, space_id: &str, kind: &str) -> Result<Option<Provider>> {
    let p = c
        .prepare_cached(
            "SELECT p.id, p.space_id, sp.name, p.type, p.created_at FROM registry_providers p
             JOIN spaces sp ON sp.id = p.space_id WHERE p.space_id = ?1 AND p.type = ?2",
        )?
        .query_row(params![space_id, kind], |r| {
            Ok(Provider { id: r.get(0)?, space_id: r.get(1)?, namespace: r.get(2)?, kind: r.get(3)?, created_at: r.get(4)?, versions: vec![] })
        })
        .optional()?;
    let Some(mut p) = p else { return Ok(None) };
    let mut st = c.prepare_cached(
        "SELECT id, version, protocols, key_id, ascii_armor, shasums, created_at FROM registry_provider_versions WHERE provider_id = ?1",
    )?;
    p.versions = st
        .query_map([&p.id], |r| {
            Ok(ProviderVersion {
                id: r.get(0)?,
                version: r.get(1)?,
                protocols: serde_json::from_str(&r.get::<_, String>(2)?).unwrap_or_default(),
                key_id: r.get(3)?,
                ascii_armor: r.get(4)?,
                shasums: r.get(5)?,
                platforms: vec![],
                created_at: r.get(6)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut pst = c.prepare_cached("SELECT id, os, arch, filename, sha256, size FROM registry_provider_platforms WHERE version_id = ?1 ORDER BY os, arch")?;
    for v in &mut p.versions {
        v.platforms = pst
            .query_map([&v.id], |r| Ok(Platform { id: r.get(0)?, os: r.get(1)?, arch: r.get(2)?, filename: r.get(3)?, sha256: r.get(4)?, size: r.get(5)? }))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
    }
    sort_versions(&mut p.versions, |v| &v.version);
    Ok(Some(p))
}

pub fn list_providers(c: &Connection) -> Result<Vec<Provider>> {
    let keys: Vec<(String, String)> = c
        .prepare_cached("SELECT space_id, type FROM registry_providers ORDER BY type")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let mut out = vec![];
    for (s, t) in keys {
        out.extend(find_provider(c, &s, &t)?);
    }
    Ok(out)
}

pub struct NewProviderVersion<'a> {
    pub space_id: &'a str,
    pub kind: &'a str,
    pub version: &'a str,
    pub protocols: &'a [String],
    pub key_id: &'a str,
    pub ascii_armor: &'a str,
    pub shasums: &'a str,
    pub shasums_sig: &'a [u8],
}

pub fn add_provider_version(c: &Connection, n: &NewProviderVersion) -> Result<String> {
    let t = now();
    c.execute(
        "INSERT INTO registry_providers (id, space_id, type, created_at) VALUES (?1, ?2, ?3, ?4) ON CONFLICT (space_id, type) DO NOTHING",
        params![new_id("prv"), n.space_id, n.kind, t],
    )?;
    let provider_id: String =
        c.query_row("SELECT id FROM registry_providers WHERE space_id = ?1 AND type = ?2", params![n.space_id, n.kind], |r| r.get(0))?;
    let id = new_id("prvv");
    c.execute(
        "INSERT INTO registry_provider_versions (id, provider_id, version, protocols, key_id, ascii_armor, shasums, shasums_sig, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![id, provider_id, n.version, serde_json::to_string(n.protocols)?, n.key_id, n.ascii_armor, n.shasums, n.shasums_sig, t],
    )?;
    Ok(id)
}

pub fn add_platform(c: &Connection, version_id: &str, os: &str, arch: &str, filename: &str, sha256: &str, size: i64) -> Result<String> {
    let id = new_id("plat");
    c.execute(
        "INSERT INTO registry_provider_platforms (id, version_id, os, arch, filename, sha256, size) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT (version_id, os, arch) DO UPDATE SET filename = excluded.filename, sha256 = excluded.sha256, size = excluded.size",
        params![id, version_id, os, arch, filename, sha256, size],
    )?;
    Ok(c.query_row("SELECT id FROM registry_provider_platforms WHERE version_id = ?1 AND os = ?2 AND arch = ?3", params![version_id, os, arch], |r| r.get(0))?)
}

pub fn delete_provider_version(c: &Connection, provider_id: &str, version: &str) -> Result<Vec<String>> {
    let ids: Vec<String> = c
        .prepare(
            "SELECT pl.id FROM registry_provider_platforms pl JOIN registry_provider_versions v ON v.id = pl.version_id
             WHERE v.provider_id = ?1 AND v.version = ?2",
        )?
        .query_map(params![provider_id, version], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    c.execute("DELETE FROM registry_provider_versions WHERE provider_id = ?1 AND version = ?2", params![provider_id, version])?;
    Ok(ids)
}

/// `SHA256SUMS` and its signature for a provider version.
pub fn shasums(c: &Connection, version_id: &str) -> Result<Option<(String, Vec<u8>)>> {
    Ok(c
        .prepare_cached("SELECT shasums, shasums_sig FROM registry_provider_versions WHERE id = ?1")?
        .query_row([version_id], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?)
}

/// The hash `SHA256SUMS` lists for `filename`.
pub fn sum_for(shasums: &str, filename: &str) -> Option<String> {
    shasums.lines().find_map(|l| {
        let mut it = l.split_whitespace();
        let (h, f) = (it.next()?, it.next()?);
        (f.trim_start_matches('*') == filename).then(|| h.to_lowercase())
    })
}

pub fn module_json(m: &Module) -> Value {
    serde_json::to_value(m).unwrap_or(Value::Null)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_order() {
        let mut v = vec!["1.0.0", "1.10.0", "1.2.0", "1.2.0-rc.1", "1.2.0-beta", "0.9.9"];
        sort_versions(&mut v, |s| s);
        assert_eq!(v, vec!["1.10.0", "1.2.0", "1.2.0-rc.1", "1.2.0-beta", "1.0.0", "0.9.9"]);
        assert!(Version::parse("1.2").is_err());
        assert!(Version::parse("1.2.x").is_err());
        assert_eq!(Version::parse("v2.0.1+build").unwrap().to_string(), "2.0.1");
    }

    #[test]
    fn shasum_lookup() {
        let s = "abc123  terraform-provider-x_1.0.0_linux_amd64.zip\nDEF  terraform-provider-x_1.0.0_darwin_arm64.zip\n";
        assert_eq!(sum_for(s, "terraform-provider-x_1.0.0_darwin_arm64.zip").as_deref(), Some("def"));
        assert_eq!(sum_for(s, "nope.zip"), None);
    }
}
