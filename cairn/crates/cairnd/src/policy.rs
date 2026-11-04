//! Policy as code: Rego evaluated in-process with `regorus`.
//!
//! Policy types follow Spacelift: `plan`, `approval`, `login`, `push`,
//! `trigger`, `task` and `notification`. Every policy is a Rego module in
//! `package cairn`; which rules are read depends on the type (see
//! [`RULES`]).
//!
//! Attachment: a policy lives in a space and applies to stacks in that space
//! and below when
//! * it is attached explicitly (`/policies/{id}/attach/{stack}`),
//! * it has the label `autoattach:*`, or
//! * it has `autoattach:<label>` and the stack has `<label>`.
//!
//! `login` policies apply to every login and can only be managed by site
//! admins. Compiled policies are cached per `(id, updated_at)`.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use serde_json::{json, Value};

use crate::util::{new_id, now};

pub const KINDS: &[&str] = &["plan", "approval", "login", "push", "trigger", "task", "notification"];

/// Rules read for each policy type.
pub const RULES: &[(&str, &[&str])] = &[
    ("plan", &["deny", "warn"]),
    ("approval", &["approve", "reject"]),
    ("login", &["allow", "deny", "admin", "space_role"]),
    ("push", &["track", "propose", "ignore"]),
    ("trigger", &["trigger"]),
    ("task", &["deny"]),
    ("notification", &["webhook"]),
];

fn rules_for(kind: &str) -> &'static [&'static str] {
    RULES.iter().find(|(k, _)| *k == kind).map(|(_, r)| *r).unwrap_or(&[])
}

#[derive(Debug, Clone, Serialize)]
pub struct Policy {
    pub id: String,
    pub space_id: String,
    pub name: String,
    pub kind: String,
    pub body: String,
    pub labels: Vec<String>,
    pub description: String,
    pub created_at: String,
    pub updated_at: String,
}

/// One policy's verdict: rule name -> value (rules that are undefined are omitted).
#[derive(Debug, Clone, Serialize)]
pub struct Verdict {
    pub policy_id: String,
    pub policy: String,
    pub kind: String,
    pub result: serde_json::Map<String, Value>,
}

impl Verdict {
    pub fn set(&self, rule: &str) -> Vec<Value> {
        match self.result.get(rule) {
            Some(Value::Array(a)) => a.clone(),
            Some(Value::Object(o)) => o.keys().map(|k| Value::String(k.clone())).collect(),
            Some(v) if !v.is_null() => vec![v.clone()],
            _ => vec![],
        }
    }
    pub fn flag(&self, rule: &str) -> bool {
        matches!(self.result.get(rule), Some(Value::Bool(true)))
    }
}

/// Validate and compile a Rego module (returns a usable engine).
pub fn compile(name: &str, body: &str) -> Result<regorus::Engine> {
    let mut e = regorus::Engine::new();
    let pkg = e.add_policy(format!("{name}.rego"), body.to_string()).map_err(|e| anyhow::anyhow!("{e}"))?;
    if pkg != "data.cairn" {
        bail!("policies must be in `package cairn` (found {pkg})");
    }
    Ok(e)
}

/// Evaluate one compiled policy against `input`.
pub fn evaluate(engine: &regorus::Engine, kind: &str, input: &Value) -> Result<serde_json::Map<String, Value>> {
    let mut e = engine.clone();
    e.set_input(regorus::Value::from_json_str(&input.to_string()).map_err(|e| anyhow::anyhow!("{e}"))?);
    // One evaluation of the whole package, then pick the rules this type reads
    // (undefined rules are simply absent from the document).
    let res = e.eval_query("data.cairn".to_string(), false).map_err(|e| anyhow::anyhow!("evaluating policy: {e}"))?;
    let mut out = serde_json::Map::new();
    let Some(doc) = res.result.first().and_then(|r| r.expressions.first()).map(|x| &x.value) else { return Ok(out) };
    let j: Value = serde_json::from_str(&doc.to_json_str().map_err(|e| anyhow::anyhow!("{e}"))?)?;
    for rule in rules_for(kind) {
        if let Some(v) = j.get(*rule) {
            out.insert(rule.to_string(), v.clone());
        }
    }
    Ok(out)
}

#[derive(Default)]
pub struct PolicyCache {
    compiled: Mutex<HashMap<String, (String, Arc<regorus::Engine>)>>,
}

impl PolicyCache {
    pub fn engine(&self, p: &Policy) -> Result<Arc<regorus::Engine>> {
        if let Some((ver, e)) = self.compiled.lock().unwrap().get(&p.id) {
            if *ver == p.updated_at {
                return Ok(e.clone());
            }
        }
        let e = Arc::new(compile(&p.name, &p.body)?);
        self.compiled.lock().unwrap().insert(p.id.clone(), (p.updated_at.clone(), e.clone()));
        Ok(e)
    }

    /// Evaluate every policy in `policies` (all of the same kind).
    pub fn run(&self, policies: &[Policy], input: &Value) -> Result<Vec<Verdict>> {
        let mut out = Vec::with_capacity(policies.len());
        for p in policies {
            let e = self.engine(p).with_context(|| format!("policy {}", p.name))?;
            let result = evaluate(&e, &p.kind, input).with_context(|| format!("policy {}", p.name))?;
            out.push(Verdict { policy_id: p.id.clone(), policy: p.name.clone(), kind: p.kind.clone(), result });
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------- storage

const COLS: &str = "id, space_id, name, kind, body, labels, description, created_at, updated_at";

fn row(r: &rusqlite::Row) -> rusqlite::Result<Policy> {
    let labels: String = r.get(5)?;
    Ok(Policy {
        id: r.get(0)?,
        space_id: r.get(1)?,
        name: r.get(2)?,
        kind: r.get(3)?,
        body: r.get(4)?,
        labels: serde_json::from_str(&labels).unwrap_or_default(),
        description: r.get(6)?,
        created_at: r.get(7)?,
        updated_at: r.get(8)?,
    })
}

pub struct NewPolicy<'a> {
    pub space_id: &'a str,
    pub name: &'a str,
    pub kind: &'a str,
    pub body: &'a str,
    pub labels: &'a [String],
    pub description: &'a str,
}

pub fn create(c: &Connection, n: &NewPolicy) -> Result<Policy> {
    let id = new_id("pol");
    let t = now();
    c.execute(
        &format!("INSERT INTO policies ({COLS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8)"),
        params![id, n.space_id, n.name, n.kind, n.body, serde_json::to_string(n.labels)?, n.description, t],
    )?;
    Ok(get(c, &id)?.expect("inserted"))
}

pub fn get(c: &Connection, id: &str) -> Result<Option<Policy>> {
    Ok(c.query_row(&format!("SELECT {COLS} FROM policies WHERE id = ?1 OR name = ?1"), [id], row).optional()?)
}

pub fn update(c: &Connection, id: &str, body: Option<&str>, labels: Option<&[String]>, description: Option<&str>) -> Result<()> {
    c.execute(
        "UPDATE policies SET body = COALESCE(?2, body), labels = COALESCE(?3, labels),
         description = COALESCE(?4, description), updated_at = ?5 WHERE id = ?1",
        params![id, body, labels.map(|l| serde_json::to_string(l).unwrap_or_default()), description, now()],
    )?;
    Ok(())
}

pub fn delete(c: &Connection, id: &str) -> Result<bool> {
    Ok(c.execute("DELETE FROM policies WHERE id = ?1", [id])? > 0)
}

pub fn list(c: &Connection, kind: Option<&str>) -> Result<Vec<Policy>> {
    let mut st = c.prepare_cached(&format!("SELECT {COLS} FROM policies WHERE (?1 IS NULL OR kind = ?1) ORDER BY name"))?;
    let rows = st.query_map(params![kind], row)?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub fn attach(c: &Connection, policy_id: &str, stack_id: &str) -> Result<()> {
    c.execute("INSERT OR IGNORE INTO policy_attachments (policy_id, stack_id) VALUES (?1, ?2)", params![policy_id, stack_id])?;
    Ok(())
}

pub fn detach(c: &Connection, policy_id: &str, stack_id: &str) -> Result<bool> {
    Ok(c.execute("DELETE FROM policy_attachments WHERE policy_id = ?1 AND stack_id = ?2", params![policy_id, stack_id])? > 0)
}

/// Policies of `kind` that apply to a stack in `space_chain` (its space and
/// all ancestors) with `labels`.
pub fn for_stack(c: &Connection, kind: &str, stack_id: &str, space_chain: &[String], labels: &[String]) -> Result<Vec<Policy>> {
    let chain = serde_json::to_string(space_chain)?;
    let mut st = c.prepare_cached(&format!(
        "SELECT {COLS} FROM policies WHERE kind = ?1 AND space_id IN (SELECT value FROM json_each(?2))"
    ))?;
    let candidates = st.query_map(params![kind, chain], row)?.collect::<rusqlite::Result<Vec<_>>>()?;
    if candidates.is_empty() {
        return Ok(vec![]);
    }
    let attached: HashSet<String> = c
        .prepare_cached("SELECT policy_id FROM policy_attachments WHERE stack_id = ?1")?
        .query_map([stack_id], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(candidates
        .into_iter()
        .filter(|p| {
            attached.contains(&p.id)
                || p.labels.iter().any(|l| match l.strip_prefix("autoattach:") {
                    Some("*") => true,
                    Some(want) => labels.iter().any(|sl| sl == want),
                    None => false,
                })
        })
        .collect())
}

pub fn record(c: &Connection, run_id: &str, verdicts: &[Verdict]) -> Result<()> {
    for v in verdicts {
        c.execute(
            "INSERT INTO policy_evaluations (id, run_id, policy_id, policy, kind, result, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![new_id("pev"), run_id, v.policy_id, v.policy, v.kind, Value::Object(v.result.clone()).to_string(), now()],
        )?;
    }
    Ok(())
}

pub fn evaluations(c: &Connection, run_id: &str) -> Result<Vec<Value>> {
    let mut st = c.prepare("SELECT policy_id, policy, kind, result, created_at FROM policy_evaluations WHERE run_id = ?1 ORDER BY created_at")?;
    let rows = st
        .query_map([run_id], |r| {
            let res: String = r.get(3)?;
            Ok(json!({"policy_id": r.get::<_, String>(0)?, "policy": r.get::<_, String>(1)?, "kind": r.get::<_, String>(2)?,
                      "result": serde_json::from_str::<Value>(&res).unwrap_or(Value::Null), "at": r.get::<_, String>(4)?}))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

#[derive(Debug, Clone, Serialize)]
pub struct Review {
    pub id: String,
    pub run_id: String,
    pub principal: String,
    pub decision: String,
    pub note: String,
    pub created_at: String,
}

pub fn add_review(c: &Connection, run_id: &str, principal: &str, decision: &str, note: &str) -> Result<()> {
    c.execute(
        "INSERT INTO run_reviews (id, run_id, principal, decision, note, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![new_id("rev"), run_id, principal, decision, note, now()],
    )?;
    Ok(())
}

pub fn reviews(c: &Connection, run_id: &str) -> Result<Vec<Review>> {
    let mut st = c.prepare("SELECT id, run_id, principal, decision, note, created_at FROM run_reviews WHERE run_id = ?1 ORDER BY created_at")?;
    let rows = st
        .query_map([run_id], |r| {
            Ok(Review { id: r.get(0)?, run_id: r.get(1)?, principal: r.get(2)?, decision: r.get(3)?, note: r.get(4)?, created_at: r.get(5)? })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Combine approval verdicts: any reject wins; otherwise every policy must approve.
pub fn approval_outcome(verdicts: &[Verdict]) -> &'static str {
    if verdicts.iter().any(|v| v.flag("reject")) {
        "rejected"
    } else if !verdicts.is_empty() && verdicts.iter().all(|v| v.flag("approve")) {
        "approved"
    } else {
        "pending"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn verdicts(kind: &str, body: &str, input: Value) -> serde_json::Map<String, Value> {
        let e = compile("t", body).unwrap();
        evaluate(&e, kind, &input).unwrap()
    }

    #[test]
    fn plan_deny_and_warn() {
        let body = r#"package cairn
import rego.v1
deny contains sprintf("no deletes: %s", [rc.address]) if { some rc in input.plan.resource_changes; "delete" in rc.change.actions }
warn contains "large change" if count(input.plan.resource_changes) > 1
"#;
        let input = json!({"plan": {"resource_changes": [
            {"address": "a.b", "change": {"actions": ["delete"]}}, {"address": "c.d", "change": {"actions": ["create"]}}]}});
        let r = verdicts("plan", body, input);
        assert_eq!(r["deny"], json!(["no deletes: a.b"]));
        assert_eq!(r["warn"], json!(["large change"]));
    }

    #[test]
    fn undefined_rules_are_omitted_and_package_is_enforced() {
        let r = verdicts("approval", "package cairn\nimport rego.v1\napprove if count(input.reviews) > 1\n", json!({"reviews": []}));
        assert!(r.is_empty());
        assert!(compile("x", "package other\n").is_err());
        assert!(compile("x", "package cairn\nthis is not rego {").is_err());
    }

    #[test]
    fn approval_combination() {
        let v = |a: bool, r: bool| Verdict {
            policy_id: "p".into(),
            policy: "p".into(),
            kind: "approval".into(),
            result: {
                let mut m = serde_json::Map::new();
                m.insert("approve".into(), json!(a));
                m.insert("reject".into(), json!(r));
                m
            },
        };
        assert_eq!(approval_outcome(&[v(true, false), v(true, false)]), "approved");
        assert_eq!(approval_outcome(&[v(true, false), v(false, false)]), "pending");
        assert_eq!(approval_outcome(&[v(true, false), v(false, true)]), "rejected");
    }
}
