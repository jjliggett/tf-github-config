//! Stack dependencies: a DAG between stacks, outputs passed downstream as
//! input variables, and triggering after applies.
//!
//! * `B depends on A` means A's tracked runs go first: a tracked run of B
//!   waits while A has a tracked run queued or in progress.
//! * References map an output of A to an input of B; B's runs get
//!   `TF_VAR_<input>` from A's current state.
//! * When A applies, B is triggered — if B references A's outputs, only when
//!   one of those outputs changed. `trigger` policies on A can add stacks.
//! * Trigger chains stop after [`MAX_DEPTH`] hops, so a cycle created through
//!   trigger policies can't run forever.

use std::collections::{HashMap, HashSet};

use anyhow::{bail, Result};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::app::AppRef;
use crate::store::{self, Stack};
use crate::util::now;

pub const MAX_DEPTH: i64 = 10;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Reference {
    /// Output of the upstream stack.
    pub output: String,
    /// Input variable of the downstream stack (`TF_VAR_<input>`).
    pub input: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Dependency {
    pub stack_id: String,
    pub depends_on: String,
    pub references: Vec<Reference>,
    pub created_at: String,
}

fn row(r: &rusqlite::Row) -> rusqlite::Result<Dependency> {
    Ok(Dependency {
        stack_id: r.get(0)?,
        depends_on: r.get(1)?,
        references: serde_json::from_str(&r.get::<_, String>(2)?).unwrap_or_default(),
        created_at: r.get(3)?,
    })
}

pub fn all(c: &Connection) -> Result<Vec<Dependency>> {
    let mut st = c.prepare_cached("SELECT stack_id, depends_on, refs, created_at FROM stack_dependencies")?;
    let rows = st.query_map([], row)?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub fn upstream(c: &Connection, stack_id: &str) -> Result<Vec<Dependency>> {
    let mut st = c.prepare_cached("SELECT stack_id, depends_on, refs, created_at FROM stack_dependencies WHERE stack_id = ?1")?;
    let rows = st.query_map([stack_id], row)?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub fn downstream(c: &Connection, stack_id: &str) -> Result<Vec<Dependency>> {
    let mut st = c.prepare_cached("SELECT stack_id, depends_on, refs, created_at FROM stack_dependencies WHERE depends_on = ?1")?;
    let rows = st.query_map([stack_id], row)?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Would `stack_id -> depends_on` close a cycle?
fn creates_cycle(c: &Connection, stack_id: &str, depends_on: &str) -> Result<bool> {
    if stack_id == depends_on {
        return Ok(true);
    }
    let mut edges: HashMap<String, Vec<String>> = HashMap::new();
    for d in all(c)? {
        edges.entry(d.stack_id).or_default().push(d.depends_on);
    }
    // Can we get from depends_on back to stack_id?
    let mut stack = vec![depends_on.to_string()];
    let mut seen = HashSet::new();
    while let Some(n) = stack.pop() {
        if n == stack_id {
            return Ok(true);
        }
        if seen.insert(n.clone()) {
            stack.extend(edges.get(&n).cloned().unwrap_or_default());
        }
    }
    Ok(false)
}

pub fn set(c: &Connection, stack_id: &str, depends_on: &str, refs: &[Reference]) -> Result<()> {
    if creates_cycle(c, stack_id, depends_on)? {
        bail!("dependency cycle: the upstream stack already depends on this stack");
    }
    let mut inputs = HashSet::new();
    for r in refs {
        if !valid_var(&r.input) || r.output.is_empty() {
            bail!("invalid reference {} -> {}", r.output, r.input);
        }
        if !inputs.insert(&r.input) {
            bail!("input {} is referenced twice", r.input);
        }
    }
    // An input may only be fed by one upstream stack.
    for d in upstream(c, stack_id)?.iter().filter(|d| d.depends_on != depends_on) {
        if let Some(r) = d.references.iter().find(|r| inputs.contains(&r.input)) {
            bail!("input {} is already fed by stack {}", r.input, d.depends_on);
        }
    }
    c.execute(
        "INSERT INTO stack_dependencies (stack_id, depends_on, refs, created_at) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT (stack_id, depends_on) DO UPDATE SET refs = excluded.refs",
        params![stack_id, depends_on, serde_json::to_string(refs)?, now()],
    )?;
    Ok(())
}

pub fn remove(c: &Connection, stack_id: &str, depends_on: &str) -> Result<bool> {
    Ok(c.execute("DELETE FROM stack_dependencies WHERE stack_id = ?1 AND depends_on = ?2", params![stack_id, depends_on])? > 0)
}

fn valid_var(s: &str) -> bool {
    let mut ch = s.chars();
    matches!(ch.next(), Some(c) if c.is_ascii_alphabetic() || c == '_') && ch.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// `TF_VAR_*` values for a run of `stack_id`, from its upstream stacks' current outputs.
pub fn input_env(c: &Connection, stack_id: &str) -> Result<Vec<(String, String, bool)>> {
    let mut out = vec![];
    for d in upstream(c, stack_id)? {
        if d.references.is_empty() {
            continue;
        }
        let up = store::get_stack(c, &d.depends_on)?.map(|s| format!("{}/{}", s.space, s.name)).unwrap_or(d.depends_on.clone());
        let outputs = store::current_state(c, &d.depends_on)?.map(|s| s.outputs).unwrap_or(Value::Null);
        for r in &d.references {
            let Some(o) = outputs.get(&r.output) else {
                bail!("stack {up} has no output {:?} (needed for input {}); apply it first", r.output, r.input);
            };
            let value = match &o["value"] {
                Value::String(s) => s.clone(),
                v => v.to_string(), // numbers, bools, lists and maps: JSON is valid HCL
            };
            out.push((format!("TF_VAR_{}", r.input), value, o["sensitive"] == json!(true)));
        }
    }
    Ok(out)
}

/// Upstream stacks with a tracked run queued or in progress (they go first).
pub fn blocked_by(deps: &[Dependency], active_tracked: &HashSet<String>, stack_id: &str) -> bool {
    deps.iter().any(|d| d.stack_id == stack_id && active_tracked.contains(&d.depends_on))
}

fn changed(prev: &Value, new: &Value, names: &[&str]) -> bool {
    names.iter().any(|n| prev.get(*n).map(|o| &o["value"]) != new.get(*n).map(|o| &o["value"]))
}

/// After a tracked run applied on `stack`: trigger dependents and whatever
/// `trigger` policies ask for.
pub async fn on_applied(app: &AppRef, stack: &Stack, run: &store::Run) -> Result<Vec<String>> {
    let depth = run.params.as_ref().and_then(|p| p["trigger_depth"].as_i64()).unwrap_or(0);
    if depth >= MAX_DEPTH {
        tracing::warn!(run = %run.id, "dependency trigger depth {MAX_DEPTH} reached; not triggering further stacks");
        return Ok(vec![]);
    }
    let (deps, states) = app.db.read(|c| Ok((downstream(c, &stack.id)?, store::list_states(c, &stack.id, 2)?)))?;
    let new_out = states.first().filter(|s| s.run_id.as_deref() == Some(run.id.as_str())).map(|s| s.outputs.clone());
    let prev_out = states.get(1).map(|s| s.outputs.clone()).unwrap_or(Value::Null);
    let mut targets: Vec<String> = vec![];
    for d in &deps {
        let names: Vec<&str> = d.references.iter().map(|r| r.output.as_str()).collect();
        let go = match &new_out {
            None => false, // state didn't change
            Some(_) if names.is_empty() => true,
            Some(n) => changed(&prev_out, n, &names),
        };
        if go {
            targets.push(d.stack_id.clone());
        }
    }
    // Trigger policies on the upstream stack can add more.
    let chain = app.space_chain(&stack.space_id);
    let pols = app.db.read(|c| crate::policy::for_stack(c, "trigger", &stack.id, &chain, &stack.labels))?;
    if !pols.is_empty() {
        let candidates: Vec<Stack> = app.db.read(|c| store::list_stacks(c, None, "", 10_000, None))?
            .into_iter()
            .filter(|s| s.id != stack.id && app.space_chain(&s.space_id).contains(&stack.space_id))
            .collect();
        let input = json!({
            "run": {"id": run.id, "kind": run.kind, "status": run.status, "triggered_by": run.triggered_by,
                    "changes": {"add": run.adds, "change": run.changes, "destroy": run.destroys}},
            "stack": {"id": stack.id, "name": stack.name, "space": stack.space, "labels": stack.labels},
            "outputs_changed": new_out.as_ref().map(|n| n.as_object().map(|m| m.keys().filter(|k| changed(&prev_out, n, &[k.as_str()])).cloned().collect::<Vec<_>>()).unwrap_or_default()),
            "stacks": candidates.iter().map(|s| json!({"id": s.id, "name": s.name, "space": s.space, "labels": s.labels})).collect::<Vec<_>>(),
        });
        let verdicts = app.policies.run(&pols, &input)?;
        app.db.write(|c| crate::policy::record(c, &run.id, &verdicts))?;
        for v in verdicts {
            for t in v.set("trigger") {
                let Some(t) = t.as_str() else { continue };
                if let Some(s) = candidates.iter().find(|s| s.id == t || format!("{}/{}", s.space, s.name) == t) {
                    targets.push(s.id.clone());
                }
            }
        }
    }
    let mut started = vec![];
    let mut seen = HashSet::new();
    for id in targets {
        if !seen.insert(id.clone()) {
            continue;
        }
        let Some(s) = app.db.read(|c| store::get_stack(c, &id))? else { continue };
        let cv = app.db.read(|c| store::last_tracked_config(c, &s.id))?;
        let cv = match cv {
            Some(cv) => cv,
            None => match crate::api::native::latest_config(app, &s) {
                Ok(cv) => cv,
                Err(_) => {
                    tracing::info!(stack = %s.name, "dependent stack has no configuration yet; not triggered");
                    continue;
                }
            },
        };
        let msg = format!("Triggered by {}/{} (run {})", stack.space, stack.name, run.id);
        let by = format!("dependency:{}", run.id);
        let params = json!({"trigger_depth": depth + 1});
        let r = crate::api::native::enqueue(app, &s, &store::NewRun {
            stack_id: &s.id, kind: "tracked", config_id: Some(&cv), auto_apply: s.auto_apply, message: &msg,
            triggered_by: &by, source: None, params: Some(&params),
        })
        .map_err(|e| anyhow::anyhow!(e.detail()))?;
        started.push(r.id);
    }
    Ok(started)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_change_detection() {
        let a = json!({"x": {"value": 1}, "y": {"value": "a"}});
        let b = json!({"x": {"value": 1}, "y": {"value": "b"}});
        assert!(!changed(&a, &b, &["x"]));
        assert!(changed(&a, &b, &["y"]));
        assert!(changed(&Value::Null, &b, &["x"]));
    }

    #[test]
    fn variable_names() {
        assert!(valid_var("vpc_id"));
        assert!(!valid_var("1x"));
        assert!(!valid_var("a b"));
    }
}
