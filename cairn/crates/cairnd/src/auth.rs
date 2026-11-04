//! Identity and access: principals, roles, role bindings, tokens, audit.
//!
//! Model (TFC's read/plan/write/admin crossed with Spacelift's spaces):
//!
//! * Spaces form a tree. A role granted on a space applies to every space
//!   below it.
//! * Roles: `reader` < `planner` < `writer` < `admin`.
//! * Principals: users (from OIDC, or the user behind a user token), teams
//!   (IdP groups, matched by name) and service tokens. Site admins (site
//!   token, OIDC admin group, admin service tokens) can do everything.
//!
//! Everything authorization needs is held in an in-memory snapshot rebuilt
//! when bindings, spaces, users or tokens change, so a permission check is a
//! few hash lookups and never a database query.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};

use anyhow::Result;
use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::http::StatusCode;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::app::{ApiError, ApiResult};
use crate::db::Db;
use crate::util::{new_id, now};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Reader,
    Planner,
    Writer,
    Admin,
}

impl Role {
    pub fn parse(s: &str) -> Option<Role> {
        Some(match s {
            "reader" | "read" => Role::Reader,
            "planner" | "plan" => Role::Planner,
            "writer" | "write" => Role::Writer,
            "admin" => Role::Admin,
            _ => return None,
        })
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Reader => "reader",
            Role::Planner => "planner",
            Role::Writer => "writer",
            Role::Admin => "admin",
        }
    }
}

/// Who is making a request.
#[derive(Debug, Clone, Serialize)]
pub struct Principal {
    /// `site`, `user` or `token`.
    pub kind: &'static str,
    /// User ID, token ID, or `site-token`.
    pub id: String,
    pub name: String,
    pub groups: Vec<String>,
    pub site_admin: bool,
    /// Extra roles granted at login time by a `login` policy: space ID -> role.
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    pub policy_roles: HashMap<String, Role>,
}

impl Principal {
    pub fn site() -> Self {
        Principal {
            kind: "site",
            id: "site-token".into(),
            name: "site token".into(),
            groups: vec![],
            site_admin: true,
            policy_roles: HashMap::new(),
        }
    }
    /// Stable label for audit logs and run attribution.
    pub fn label(&self) -> String {
        format!("{}:{}", self.kind, if self.kind == "user" { &self.name } else { &self.id })
    }
}

impl<S: Send + Sync> FromRequestParts<S> for Principal {
    type Rejection = ApiError;
    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        parts
            .extensions
            .get::<Principal>()
            .cloned()
            .ok_or_else(|| ApiError::new(StatusCode::UNAUTHORIZED, "not authenticated"))
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct User {
    pub id: String,
    pub subject: String,
    pub email: String,
    pub name: String,
    pub groups: Vec<String>,
    pub site_admin: bool,
    pub created_at: String,
    pub last_seen_at: Option<String>,
    /// Roles granted by `login` policies at the last login: space ID -> role.
    pub policy_roles: HashMap<String, Role>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TokenInfo {
    pub id: String,
    /// `user` (acts as its owner) or `service` (has its own bindings).
    pub kind: String,
    pub owner: Option<String>,
    pub description: String,
    pub site_admin: bool,
    pub expires_at: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Binding {
    pub id: String,
    pub space_id: String,
    /// `user`, `team` or `token`.
    pub principal_kind: String,
    pub principal: String,
    pub role: Role,
    pub created_at: String,
}

/// Read-optimized copy of everything authorization needs.
#[derive(Default)]
pub struct Snapshot {
    /// space id -> (name, parent id)
    pub spaces: HashMap<String, (String, Option<String>)>,
    /// space id -> bindings on that space
    pub bindings: HashMap<String, Vec<Binding>>,
    pub users: HashMap<String, User>,
    /// token sha256 -> token
    pub tokens: HashMap<String, TokenInfo>,
}

pub struct Authz {
    snap: RwLock<Arc<Snapshot>>,
}

impl Authz {
    pub fn load(db: &Db) -> Result<Self> {
        Ok(Authz { snap: RwLock::new(Arc::new(db.read(load_snapshot)?)) })
    }

    pub fn reload(&self, db: &Db) -> Result<()> {
        let s = db.read(load_snapshot)?;
        *self.snap.write().unwrap() = Arc::new(s);
        Ok(())
    }

    pub fn snapshot(&self) -> Arc<Snapshot> {
        self.snap.read().unwrap().clone()
    }

    /// [`Authz::role`] against a snapshot the caller already holds (no lock per call).
    pub fn role_in_snapshot(&self, s: &Snapshot, p: &Principal, space_id: &str) -> Option<Role> {
        if p.site_admin {
            return Some(Role::Admin);
        }
        role_in(s, p, space_id)
    }

    /// The role `p` has on `space_id` (inherited from ancestors), if any.
    pub fn role(&self, p: &Principal, space_id: &str) -> Option<Role> {
        if p.site_admin {
            return Some(Role::Admin);
        }
        let s = self.snapshot();
        role_in(&s, p, space_id)
    }

    /// IDs of every space `p` can at least read; `None` means all of them.
    pub fn readable_spaces(&self, p: &Principal) -> Option<HashSet<String>> {
        if p.site_admin {
            return None;
        }
        let s = self.snapshot();
        Some(s.spaces.keys().filter(|id| role_in(&s, p, id).is_some()).cloned().collect())
    }

    pub fn require(&self, p: &Principal, space_id: &str, need: Role) -> ApiResult<()> {
        match self.role(p, space_id) {
            Some(r) if r >= need => Ok(()),
            Some(r) => Err(ApiError::new(
                StatusCode::FORBIDDEN,
                format!("needs the {} role on this space (you have {})", need.as_str(), r.as_str()),
            )),
            // Don't reveal that the resource exists.
            None => Err(ApiError::not_found("resource")),
        }
    }

    pub fn require_site_admin(&self, p: &Principal) -> ApiResult<()> {
        if p.site_admin {
            Ok(())
        } else {
            Err(ApiError::new(StatusCode::FORBIDDEN, "needs site admin"))
        }
    }

    /// Look up an API token by its SHA-256. Expired tokens are rejected.
    pub fn token(&self, sha: &str) -> Option<(TokenInfo, Option<User>)> {
        let s = self.snapshot();
        let t = s.tokens.get(sha)?.clone();
        if let Some(exp) = &t.expires_at {
            if exp.as_str() < now().as_str() {
                return None;
            }
        }
        let owner = t.owner.as_ref().and_then(|o| s.users.get(o)).cloned();
        Some((t, owner))
    }
}

fn role_in(s: &Snapshot, p: &Principal, space_id: &str) -> Option<Role> {
    let mut best: Option<Role> = None;
    let mut cur = Some(space_id.to_string());
    let mut hops = 0;
    while let Some(id) = cur {
        if let Some(r) = p.policy_roles.get(&id) {
            best = best.max(Some(*r));
        }
        for b in s.bindings.get(&id).map(Vec::as_slice).unwrap_or(&[]) {
            let hit = match b.principal_kind.as_str() {
                "user" => p.kind == "user" && b.principal == p.id,
                "team" => p.groups.iter().any(|g| g == &b.principal),
                "token" => p.kind == "token" && b.principal == p.id,
                _ => false,
            };
            if hit {
                best = best.max(Some(b.role));
            }
        }
        cur = s.spaces.get(&id).and_then(|(_, parent)| parent.clone());
        hops += 1;
        if hops > 64 {
            break; // defensive: a corrupted tree must not loop forever
        }
    }
    best
}

pub fn principal_for_user(u: &User) -> Principal {
    Principal {
        kind: "user",
        id: u.id.clone(),
        name: if u.email.is_empty() { u.name.clone() } else { u.email.clone() },
        groups: u.groups.clone(),
        site_admin: u.site_admin,
        policy_roles: u.policy_roles.clone(),
    }
}

pub fn principal_for_token(t: &TokenInfo, owner: Option<User>) -> Principal {
    match (t.kind.as_str(), owner) {
        ("user", Some(u)) => {
            let mut p = principal_for_user(&u);
            p.site_admin |= t.site_admin;
            p
        }
        _ => Principal {
            kind: "token",
            id: t.id.clone(),
            name: t.description.clone(),
            groups: vec![],
            site_admin: t.site_admin,
            policy_roles: HashMap::new(),
        },
    }
}

// ---------------------------------------------------------------- storage

fn load_snapshot(c: &Connection) -> Result<Snapshot> {
    let mut s = Snapshot::default();
    let mut st = c.prepare("SELECT id, name, parent_id FROM spaces")?;
    for r in st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<String>>(2)?)))? {
        let (id, name, parent) = r?;
        s.spaces.insert(id, (name, parent));
    }
    for b in list_bindings(c, None)? {
        s.bindings.entry(b.space_id.clone()).or_default().push(b);
    }
    for u in list_users(c)? {
        s.users.insert(u.id.clone(), u);
    }
    let mut st = c.prepare(
        "SELECT sha256, id, kind, owner, description, site_admin, expires_at, created_at FROM tokens",
    )?;
    for r in st.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            TokenInfo {
                id: r.get(1)?,
                kind: r.get(2)?,
                owner: r.get(3)?,
                description: r.get(4)?,
                site_admin: r.get(5)?,
                expires_at: r.get(6)?,
                created_at: r.get(7)?,
            },
        ))
    })? {
        let (sha, t) = r?;
        s.tokens.insert(sha, t);
    }
    Ok(s)
}

fn user_row(r: &rusqlite::Row) -> rusqlite::Result<User> {
    let groups: String = r.get(4)?;
    Ok(User {
        id: r.get(0)?,
        subject: r.get(1)?,
        email: r.get(2)?,
        name: r.get(3)?,
        groups: serde_json::from_str(&groups).unwrap_or_default(),
        site_admin: r.get(5)?,
        created_at: r.get(6)?,
        last_seen_at: r.get(7)?,
        policy_roles: serde_json::from_str(&r.get::<_, String>(8)?).unwrap_or_default(),
    })
}

const USER_COLS: &str = "id, subject, email, name, groups, site_admin, created_at, last_seen_at, policy_roles";

pub fn list_users(c: &Connection) -> Result<Vec<User>> {
    let mut st = c.prepare(&format!("SELECT {USER_COLS} FROM users ORDER BY email, name"))?;
    let rows = st.query_map([], user_row)?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Create or refresh a user from IdP claims. Returns (user, changed).
pub fn upsert_user(
    c: &Connection,
    subject: &str,
    email: &str,
    name: &str,
    groups: &[String],
    site_admin: bool,
    policy_roles: &HashMap<String, Role>,
) -> Result<(User, bool)> {
    let existing = c
        .query_row(&format!("SELECT {USER_COLS} FROM users WHERE subject = ?1"), [subject], user_row)
        .optional()?;
    let groups_json = serde_json::to_string(groups)?;
    let roles_json = serde_json::to_string(policy_roles)?;
    let t = now();
    match existing {
        Some(u) => {
            let changed = u.email != email || u.name != name || u.groups != groups || u.site_admin != site_admin || u.policy_roles != *policy_roles;
            c.execute(
                "UPDATE users SET email = ?2, name = ?3, groups = ?4, site_admin = ?5, last_seen_at = ?6, policy_roles = ?7 WHERE id = ?1",
                params![u.id, email, name, groups_json, site_admin, t, roles_json],
            )?;
            let u = User { email: email.into(), name: name.into(), groups: groups.to_vec(), site_admin, last_seen_at: Some(t), policy_roles: policy_roles.clone(), ..u };
            Ok((u, changed))
        }
        None => {
            let id = new_id("usr");
            c.execute(
                "INSERT INTO users (id, subject, email, name, groups, site_admin, created_at, last_seen_at, policy_roles)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7, ?8)",
                params![id, subject, email, name, groups_json, site_admin, t, roles_json],
            )?;
            let u = User {
                id,
                subject: subject.into(),
                email: email.into(),
                name: name.into(),
                groups: groups.to_vec(),
                site_admin,
                created_at: t.clone(),
                last_seen_at: Some(t),
                policy_roles: policy_roles.clone(),
            };
            Ok((u, true))
        }
    }
}

pub fn list_bindings(c: &Connection, space_id: Option<&str>) -> Result<Vec<Binding>> {
    let mut st = c.prepare(
        "SELECT id, space_id, principal_kind, principal, role, created_at FROM role_bindings
         WHERE (?1 IS NULL OR space_id = ?1) ORDER BY principal_kind, principal",
    )?;
    let rows = st
        .query_map(params![space_id], |r| {
            let role: String = r.get(4)?;
            Ok(Binding {
                id: r.get(0)?,
                space_id: r.get(1)?,
                principal_kind: r.get(2)?,
                principal: r.get(3)?,
                role: Role::parse(&role).unwrap_or(Role::Reader),
                created_at: r.get(5)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub fn put_binding(c: &Connection, space_id: &str, kind: &str, principal: &str, role: Role) -> Result<String> {
    let id = new_id("rb");
    c.execute(
        "INSERT INTO role_bindings (id, space_id, principal_kind, principal, role, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT (space_id, principal_kind, principal) DO UPDATE SET role = excluded.role",
        params![id, space_id, kind, principal, role.as_str(), now()],
    )?;
    Ok(c.query_row(
        "SELECT id FROM role_bindings WHERE space_id = ?1 AND principal_kind = ?2 AND principal = ?3",
        params![space_id, kind, principal],
        |r| r.get(0),
    )?)
}

pub fn delete_binding(c: &Connection, space_id: &str, id: &str) -> Result<bool> {
    Ok(c.execute("DELETE FROM role_bindings WHERE space_id = ?1 AND id = ?2", params![space_id, id])? > 0)
}

pub struct NewToken<'a> {
    pub sha: &'a str,
    pub kind: &'a str,
    pub owner: Option<&'a str>,
    pub description: &'a str,
    pub site_admin: bool,
    pub expires_at: Option<&'a str>,
}

pub fn insert_token(c: &Connection, t: &NewToken) -> Result<String> {
    let id = new_id("tok");
    c.execute(
        "INSERT INTO tokens (id, sha256, description, created_at, kind, owner, site_admin, expires_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![id, t.sha, t.description, now(), t.kind, t.owner, t.site_admin, t.expires_at],
    )?;
    Ok(id)
}

pub fn list_tokens(c: &Connection, owner: Option<&str>) -> Result<Vec<TokenInfo>> {
    let mut st = c.prepare(
        "SELECT id, kind, owner, description, site_admin, expires_at, created_at FROM tokens
         WHERE (?1 IS NULL OR owner = ?1) ORDER BY created_at",
    )?;
    let rows = st
        .query_map(params![owner], |r| {
            Ok(TokenInfo {
                id: r.get(0)?,
                kind: r.get(1)?,
                owner: r.get(2)?,
                description: r.get(3)?,
                site_admin: r.get(4)?,
                expires_at: r.get(5)?,
                created_at: r.get(6)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub fn delete_token(c: &Connection, id: &str, owner: Option<&str>) -> Result<bool> {
    Ok(c.execute("DELETE FROM tokens WHERE id = ?1 AND (?2 IS NULL OR owner = ?2)", params![id, owner])? > 0)
}

pub fn audit(c: &Connection, principal: &str, method: &str, path: &str, status: u16) -> Result<()> {
    c.execute(
        "INSERT INTO audit_log (at, principal, method, path, status) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![now(), principal, method, path, status],
    )?;
    Ok(())
}

#[derive(Serialize)]
pub struct AuditEntry {
    pub seq: i64,
    pub at: String,
    pub principal: String,
    pub method: String,
    pub path: String,
    pub status: i64,
}

pub fn list_audit(c: &Connection, before: i64, limit: i64) -> Result<Vec<AuditEntry>> {
    let mut st = c.prepare(
        "SELECT seq, at, principal, method, path, status FROM audit_log WHERE seq < ?1 ORDER BY seq DESC LIMIT ?2",
    )?;
    let rows = st
        .query_map(params![before, limit], |r| {
            Ok(AuditEntry {
                seq: r.get(0)?,
                at: r.get(1)?,
                principal: r.get(2)?,
                method: r.get(3)?,
                path: r.get(4)?,
                status: r.get(5)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap() -> Snapshot {
        let mut s = Snapshot::default();
        s.spaces.insert("root".into(), ("root".into(), None));
        s.spaces.insert("team".into(), ("team".into(), Some("root".into())));
        s.spaces.insert("prod".into(), ("prod".into(), Some("team".into())));
        s.spaces.insert("other".into(), ("other".into(), None));
        let b = |space: &str, kind: &str, who: &str, role| Binding {
            id: new_id("rb"),
            space_id: space.into(),
            principal_kind: kind.into(),
            principal: who.into(),
            role,
            created_at: now(),
        };
        s.bindings.insert("team".into(), vec![b("team", "team", "devs", Role::Planner)]);
        s.bindings.insert("prod".into(), vec![b("prod", "user", "usr-1", Role::Writer)]);
        s
    }

    fn user(id: &str, groups: &[&str]) -> Principal {
        Principal {
            kind: "user",
            id: id.into(),
            name: id.into(),
            groups: groups.iter().map(|g| g.to_string()).collect(),
            site_admin: false,
            policy_roles: HashMap::new(),
        }
    }

    #[test]
    fn roles_inherit_down_the_tree_and_take_the_max() {
        let s = snap();
        let dev = user("usr-2", &["devs"]);
        assert_eq!(role_in(&s, &dev, "team"), Some(Role::Planner));
        assert_eq!(role_in(&s, &dev, "prod"), Some(Role::Planner), "inherited");
        assert_eq!(role_in(&s, &dev, "root"), None, "not upwards");
        assert_eq!(role_in(&s, &dev, "other"), None);
        let lead = user("usr-1", &["devs"]);
        assert_eq!(role_in(&s, &lead, "prod"), Some(Role::Writer), "max of direct and inherited");
        assert_eq!(role_in(&s, &lead, "team"), Some(Role::Planner));
    }

    #[test]
    fn policy_roles_count_too() {
        let s = snap();
        let mut p = user("usr-9", &[]);
        assert_eq!(role_in(&s, &p, "prod"), None);
        p.policy_roles.insert("team".into(), Role::Admin);
        assert_eq!(role_in(&s, &p, "prod"), Some(Role::Admin));
    }

    #[test]
    fn role_order() {
        assert!(Role::Admin > Role::Writer && Role::Writer > Role::Planner && Role::Planner > Role::Reader);
        assert_eq!(Role::parse("write"), Some(Role::Writer));
    }
}
