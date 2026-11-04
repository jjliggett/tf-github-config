//! Domain types and every SQL query, in one place.
//!
//! List queries are a single statement per page (keyset pagination); nothing
//! here does per-row follow-up queries.

use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension, Row};
use serde::Serialize;
use serde_json::Value;

use crate::util::{new_id, now};

#[derive(Debug, Clone, Serialize)]
pub struct Space {
    pub id: String,
    pub name: String,
    pub email: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Stack {
    pub id: String,
    pub space_id: String,
    pub space: String,
    pub name: String,
    pub description: String,
    pub engine_version: Option<String>,
    pub auto_apply: bool,
    pub project_root: String,
    pub labels: Vec<String>,
    pub locked: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lock: Option<Value>,
    #[serde(skip)]
    pub lock_id: Option<String>,
    pub current_state_id: Option<String>,
    pub latest_run_id: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Run {
    pub id: String,
    pub stack_id: String,
    pub kind: String,
    pub status: String,
    pub config_id: Option<String>,
    pub auto_apply: bool,
    pub message: String,
    pub triggered_by: String,
    pub has_changes: Option<bool>,
    pub adds: Option<i64>,
    pub changes: Option<i64>,
    pub destroys: Option<i64>,
    #[serde(skip)]
    pub plan_summary: Option<String>,
    pub error: Option<String>,
    pub created_at: String,
    pub status_times: Value,
    #[serde(skip)]
    pub seq: i64,
}

impl Run {
    pub fn is_terminal(&self) -> bool {
        is_terminal(&self.status)
    }
    /// Terminal, or waiting for a human (nothing more happens on its own).
    pub fn is_resting(&self) -> bool {
        self.is_terminal() || self.status == "planned"
    }
}

/// Resource instances in a state document (a `count = 100` resource is 100).
pub fn count_instances(state: &Value) -> i64 {
    state["resources"]
        .as_array()
        .map(|rs| rs.iter().map(|r| r["instances"].as_array().map(|i| i.len()).unwrap_or(0) as i64).sum())
        .unwrap_or(0)
}

pub fn is_terminal(status: &str) -> bool {
    matches!(status, "applied" | "planned_and_finished" | "discarded" | "errored" | "canceled")
}

#[derive(Debug, Clone, Serialize)]
pub struct ConfigVersion {
    pub id: String,
    pub stack_id: String,
    pub status: String,
    pub auto_queue: bool,
    pub speculative: bool,
    pub sha256: Option<String>,
    pub size: Option<i64>,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct StateVersion {
    pub id: String,
    pub stack_id: String,
    pub serial: i64,
    pub lineage: String,
    pub sha256: String,
    pub size: i64,
    pub run_id: Option<String>,
    pub outputs: Value,
    pub resource_count: i64,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Variable {
    pub id: String,
    pub key: String,
    /// Empty for sensitive variables in API responses.
    pub value: String,
    pub category: String,
    pub sensitive: bool,
    pub hcl: bool,
}

// ---------------------------------------------------------------- spaces

fn space_row(r: &Row) -> rusqlite::Result<Space> {
    Ok(Space { id: r.get(0)?, name: r.get(1)?, email: r.get(2)?, created_at: r.get(3)? })
}

pub fn create_space(c: &Connection, name: &str, email: &str) -> Result<Space> {
    let s = Space { id: new_id("spc"), name: name.into(), email: email.into(), created_at: now() };
    c.execute(
        "INSERT INTO spaces (id, name, email, created_at) VALUES (?1, ?2, ?3, ?4)",
        params![s.id, s.name, s.email, s.created_at],
    )?;
    Ok(s)
}

pub fn space_by_name(c: &Connection, name: &str) -> Result<Option<Space>> {
    Ok(c.query_row("SELECT id, name, email, created_at FROM spaces WHERE name = ?1", [name], space_row)
        .optional()?)
}

pub fn list_spaces(c: &Connection) -> Result<Vec<Space>> {
    let mut st = c.prepare_cached("SELECT id, name, email, created_at FROM spaces ORDER BY name")?;
    let rows = st.query_map([], space_row)?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

// ---------------------------------------------------------------- stacks

const STACK_COLS: &str = "s.id, s.space_id, sp.name, s.name, s.description, s.engine_version, s.auto_apply,
    s.project_root, s.labels, s.lock_id, s.lock_info, s.current_state_id, s.latest_run_id,
    s.created_at, s.updated_at";

fn stack_row(r: &Row) -> rusqlite::Result<Stack> {
    let labels: String = r.get(8)?;
    let lock_id: Option<String> = r.get(9)?;
    let lock_info: Option<String> = r.get(10)?;
    Ok(Stack {
        id: r.get(0)?,
        space_id: r.get(1)?,
        space: r.get(2)?,
        name: r.get(3)?,
        description: r.get(4)?,
        engine_version: r.get(5)?,
        auto_apply: r.get(6)?,
        project_root: r.get(7)?,
        labels: serde_json::from_str(&labels).unwrap_or_default(),
        locked: lock_id.is_some(),
        lock: lock_info.and_then(|s| serde_json::from_str(&s).ok()),
        lock_id,
        current_state_id: r.get(11)?,
        latest_run_id: r.get(12)?,
        created_at: r.get(13)?,
        updated_at: r.get(14)?,
    })
}

pub struct NewStack<'a> {
    pub space_id: &'a str,
    pub name: &'a str,
    pub description: &'a str,
    pub engine_version: Option<&'a str>,
    pub auto_apply: bool,
    pub project_root: &'a str,
    pub labels: &'a [String],
}

pub fn create_stack(c: &Connection, n: &NewStack) -> Result<String> {
    let id = new_id("stk");
    let t = now();
    c.execute(
        "INSERT INTO stacks (id, space_id, name, description, engine_version, auto_apply, project_root, labels, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9)",
        params![id, n.space_id, n.name, n.description, n.engine_version, n.auto_apply, n.project_root,
                serde_json::to_string(n.labels)?, t],
    )?;
    Ok(id)
}

pub fn get_stack(c: &Connection, id: &str) -> Result<Option<Stack>> {
    let sql = format!("SELECT {STACK_COLS} FROM stacks s JOIN spaces sp ON sp.id = s.space_id WHERE s.id = ?1");
    Ok(c.prepare_cached(&sql)?.query_row([id], stack_row).optional()?)
}

pub fn stack_by_name(c: &Connection, space: &str, name: &str) -> Result<Option<Stack>> {
    let sql = format!(
        "SELECT {STACK_COLS} FROM stacks s JOIN spaces sp ON sp.id = s.space_id WHERE sp.name = ?1 AND s.name = ?2"
    );
    Ok(c.prepare_cached(&sql)?.query_row([space, name], stack_row).optional()?)
}

/// One page of stacks ordered by name. `after` is the last name of the previous page.
pub fn list_stacks(c: &Connection, space: Option<&str>, after: &str, limit: i64) -> Result<Vec<Stack>> {
    let sql = format!(
        "SELECT {STACK_COLS} FROM stacks s JOIN spaces sp ON sp.id = s.space_id
         WHERE (?1 IS NULL OR sp.name = ?1) AND (sp.name || '/' || s.name) > ?2
         ORDER BY sp.name, s.name LIMIT ?3"
    );
    let mut st = c.prepare_cached(&sql)?;
    let rows = st.query_map(params![space, after, limit], stack_row)?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub struct StackPatch {
    pub name: Option<String>,
    pub description: Option<String>,
    pub engine_version: Option<Option<String>>,
    pub auto_apply: Option<bool>,
    pub project_root: Option<String>,
    pub labels: Option<Vec<String>>,
}

pub fn update_stack(c: &Connection, id: &str, p: &StackPatch) -> Result<bool> {
    let n = c.execute(
        "UPDATE stacks SET
            name = COALESCE(?2, name),
            description = COALESCE(?3, description),
            engine_version = CASE WHEN ?4 THEN ?5 ELSE engine_version END,
            auto_apply = COALESCE(?6, auto_apply),
            project_root = COALESCE(?7, project_root),
            labels = COALESCE(?8, labels),
            updated_at = ?9
         WHERE id = ?1",
        params![
            id,
            p.name,
            p.description,
            p.engine_version.is_some(),
            p.engine_version.clone().flatten(),
            p.auto_apply,
            p.project_root,
            p.labels.as_ref().map(|l| serde_json::to_string(l).unwrap_or_default()),
            now()
        ],
    )?;
    Ok(n > 0)
}

pub fn delete_stack(c: &Connection, id: &str) -> Result<bool> {
    Ok(c.execute("DELETE FROM stacks WHERE id = ?1", [id])? > 0)
}

/// Take the stack lock if it is free (or already held by `lock_id`).
pub fn try_lock_stack(c: &Connection, id: &str, lock_id: &str, info: &Value) -> Result<Result<(), Value>> {
    let n = c.execute(
        "UPDATE stacks SET lock_id = ?2, lock_info = ?3 WHERE id = ?1 AND (lock_id IS NULL OR lock_id = ?2)",
        params![id, lock_id, info.to_string()],
    )?;
    if n > 0 {
        return Ok(Ok(()));
    }
    let held: Option<String> =
        c.query_row("SELECT lock_info FROM stacks WHERE id = ?1", [id], |r| r.get(0)).optional()?.flatten();
    Ok(Err(held.and_then(|s| serde_json::from_str(&s).ok()).unwrap_or(Value::Null)))
}

/// Release the lock. With `lock_id = None` it is a force-unlock.
pub fn unlock_stack(c: &Connection, id: &str, lock_id: Option<&str>) -> Result<bool> {
    let n = c.execute(
        "UPDATE stacks SET lock_id = NULL, lock_info = NULL WHERE id = ?1 AND (?2 IS NULL OR lock_id = ?2)",
        params![id, lock_id],
    )?;
    Ok(n > 0)
}

// ---------------------------------------------------------------- config versions

fn cv_row(r: &Row) -> rusqlite::Result<ConfigVersion> {
    Ok(ConfigVersion {
        id: r.get(0)?,
        stack_id: r.get(1)?,
        status: r.get(2)?,
        auto_queue: r.get(3)?,
        speculative: r.get(4)?,
        sha256: r.get(5)?,
        size: r.get(6)?,
        created_at: r.get(7)?,
    })
}

pub fn create_config(c: &Connection, stack_id: &str, auto_queue: bool, speculative: bool) -> Result<ConfigVersion> {
    let cv = ConfigVersion {
        id: new_id("cv"),
        stack_id: stack_id.into(),
        status: "pending".into(),
        auto_queue,
        speculative,
        sha256: None,
        size: None,
        created_at: now(),
    };
    c.execute(
        "INSERT INTO config_versions (id, stack_id, status, auto_queue, speculative, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![cv.id, cv.stack_id, cv.status, cv.auto_queue, cv.speculative, cv.created_at],
    )?;
    Ok(cv)
}

pub fn get_config(c: &Connection, id: &str) -> Result<Option<ConfigVersion>> {
    Ok(c.prepare_cached(
        "SELECT id, stack_id, status, auto_queue, speculative, sha256, size, created_at FROM config_versions WHERE id = ?1",
    )?
    .query_row([id], cv_row)
    .optional()?)
}

pub fn mark_config_uploaded(c: &Connection, id: &str, sha: &str, size: i64) -> Result<()> {
    c.execute(
        "UPDATE config_versions SET status = 'uploaded', sha256 = ?2, size = ?3 WHERE id = ?1",
        params![id, sha, size],
    )?;
    Ok(())
}

// ---------------------------------------------------------------- runs

const RUN_COLS: &str = "id, stack_id, kind, status, config_id, auto_apply, message, triggered_by, has_changes,
    adds, changes, destroys, plan_summary, error, created_at, status_times, seq";

fn run_row(r: &Row) -> rusqlite::Result<Run> {
    let st: String = r.get(15)?;
    Ok(Run {
        id: r.get(0)?,
        stack_id: r.get(1)?,
        kind: r.get(2)?,
        status: r.get(3)?,
        config_id: r.get(4)?,
        auto_apply: r.get(5)?,
        message: r.get(6)?,
        triggered_by: r.get(7)?,
        has_changes: r.get(8)?,
        adds: r.get(9)?,
        changes: r.get(10)?,
        destroys: r.get(11)?,
        plan_summary: r.get(12)?,
        error: r.get(13)?,
        created_at: r.get(14)?,
        status_times: serde_json::from_str(&st).unwrap_or(Value::Null),
        seq: r.get(16)?,
    })
}

pub struct NewRun<'a> {
    pub stack_id: &'a str,
    pub kind: &'a str,
    pub config_id: Option<&'a str>,
    pub auto_apply: bool,
    pub message: &'a str,
    pub triggered_by: &'a str,
}

pub fn create_run(c: &Connection, n: &NewRun) -> Result<Run> {
    let id = new_id("run");
    let t = now();
    let times = serde_json::json!({ "plan_queued": t }).to_string();
    c.execute(
        "INSERT INTO runs (id, stack_id, kind, status, config_id, auto_apply, message, triggered_by, created_at, status_times)
         VALUES (?1, ?2, ?3, 'plan_queued', ?4, ?5, ?6, ?7, ?8, ?9)",
        params![id, n.stack_id, n.kind, n.config_id, n.auto_apply, n.message, n.triggered_by, t, times],
    )?;
    c.execute("UPDATE stacks SET latest_run_id = ?2 WHERE id = ?1", params![n.stack_id, id])?;
    Ok(get_run(c, &id)?.expect("just inserted"))
}

pub fn get_run(c: &Connection, id: &str) -> Result<Option<Run>> {
    let sql = format!("SELECT {RUN_COLS} FROM runs WHERE id = ?1");
    Ok(c.prepare_cached(&sql)?.query_row([id], run_row).optional()?)
}

pub fn list_runs(c: &Connection, stack_id: &str, before_seq: i64, limit: i64) -> Result<Vec<Run>> {
    let sql = format!("SELECT {RUN_COLS} FROM runs WHERE stack_id = ?1 AND seq < ?2 ORDER BY seq DESC LIMIT ?3");
    let mut st = c.prepare_cached(&sql)?;
    let rows = st.query_map(params![stack_id, before_seq, limit], run_row)?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Runs waiting for a runner, oldest first.
pub fn queued_runs(c: &Connection) -> Result<Vec<Run>> {
    let sql = format!("SELECT {RUN_COLS} FROM runs WHERE status IN ('plan_queued', 'apply_queued') ORDER BY seq");
    let mut st = c.prepare_cached(&sql)?;
    let rows = st.query_map([], run_row)?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Non-terminal runs (used at startup for recovery and to rebuild stack holds).
pub fn active_runs(c: &Connection) -> Result<Vec<Run>> {
    let sql = format!(
        "SELECT {RUN_COLS} FROM runs WHERE status NOT IN ('applied','planned_and_finished','discarded','errored','canceled') ORDER BY seq"
    );
    let mut st = c.prepare_cached(&sql)?;
    let rows = st.query_map([], run_row)?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Move a run to `status` if it is currently in one of `from`. Returns false if it was not.
pub fn transition(c: &Connection, id: &str, from: &[&str], status: &str) -> Result<bool> {
    let from_json = serde_json::to_string(from)?;
    let n = c.execute(
        "UPDATE runs SET status = ?2, status_times = json_set(status_times, '$.' || ?2, ?3)
         WHERE id = ?1 AND status IN (SELECT value FROM json_each(?4))",
        params![id, status, now(), from_json],
    )?;
    Ok(n > 0)
}

pub fn set_plan_result(
    c: &Connection,
    id: &str,
    has_changes: bool,
    counts: (i64, i64, i64),
    summary: &Value,
) -> Result<()> {
    c.execute(
        "UPDATE runs SET has_changes = ?2, adds = ?3, changes = ?4, destroys = ?5, plan_summary = ?6 WHERE id = ?1",
        params![id, has_changes, counts.0, counts.1, counts.2, summary.to_string()],
    )?;
    Ok(())
}

pub fn set_run_error(c: &Connection, id: &str, err: &str) -> Result<()> {
    c.execute("UPDATE runs SET error = ?2 WHERE id = ?1", params![id, err])?;
    Ok(())
}

// ---------------------------------------------------------------- state versions

fn sv_row(r: &Row) -> rusqlite::Result<StateVersion> {
    let outputs: String = r.get(7)?;
    Ok(StateVersion {
        id: r.get(0)?,
        stack_id: r.get(1)?,
        serial: r.get(2)?,
        lineage: r.get(3)?,
        sha256: r.get(4)?,
        size: r.get(5)?,
        run_id: r.get(6)?,
        outputs: serde_json::from_str(&outputs).unwrap_or(Value::Null),
        resource_count: r.get(8)?,
        created_at: r.get(9)?,
    })
}

const SV_COLS: &str = "id, stack_id, serial, lineage, sha256, size, run_id, outputs, resource_count, created_at";

pub struct NewState<'a> {
    pub stack_id: &'a str,
    pub serial: i64,
    pub lineage: &'a str,
    pub sha256: &'a str,
    pub size: i64,
    pub run_id: Option<&'a str>,
    pub outputs: &'a Value,
    pub resource_count: i64,
}

pub fn insert_state(c: &Connection, n: &NewState) -> Result<StateVersion> {
    let id = new_id("sv");
    c.execute(
        &format!("INSERT INTO state_versions ({SV_COLS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)"),
        params![id, n.stack_id, n.serial, n.lineage, n.sha256, n.size, n.run_id, n.outputs.to_string(),
                n.resource_count, now()],
    )?;
    c.execute("UPDATE stacks SET current_state_id = ?2 WHERE id = ?1", params![n.stack_id, id])?;
    Ok(get_state(c, &id)?.expect("just inserted"))
}

pub fn get_state(c: &Connection, id: &str) -> Result<Option<StateVersion>> {
    Ok(c.prepare_cached(&format!("SELECT {SV_COLS} FROM state_versions WHERE id = ?1"))?
        .query_row([id], sv_row)
        .optional()?)
}

pub fn current_state(c: &Connection, stack_id: &str) -> Result<Option<StateVersion>> {
    Ok(c.prepare_cached(&format!(
        "SELECT {SV_COLS} FROM state_versions WHERE id = (SELECT current_state_id FROM stacks WHERE id = ?1)"
    ))?
    .query_row([stack_id], sv_row)
    .optional()?)
}

pub fn list_states(c: &Connection, stack_id: &str, limit: i64) -> Result<Vec<StateVersion>> {
    let mut st = c.prepare_cached(&format!(
        "SELECT {SV_COLS} FROM state_versions WHERE stack_id = ?1 ORDER BY serial DESC, created_at DESC LIMIT ?2"
    ))?;
    let rows = st.query_map(params![stack_id, limit], sv_row)?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

// ---------------------------------------------------------------- variables

pub fn list_variables(c: &Connection, stack_id: &str) -> Result<Vec<Variable>> {
    let mut st = c.prepare_cached(
        "SELECT id, key, value, category, sensitive, hcl FROM variables WHERE stack_id = ?1 ORDER BY category, key",
    )?;
    let rows = st
        .query_map([stack_id], |r| {
            Ok(Variable {
                id: r.get(0)?,
                key: r.get(1)?,
                value: r.get(2)?,
                category: r.get(3)?,
                sensitive: r.get(4)?,
                hcl: r.get(5)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub fn upsert_variable(c: &Connection, stack_id: &str, v: &Variable) -> Result<String> {
    let id = new_id("var");
    c.execute(
        "INSERT INTO variables (id, stack_id, key, value, category, sensitive, hcl) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT (stack_id, key, category) DO UPDATE SET value = excluded.value, sensitive = excluded.sensitive, hcl = excluded.hcl",
        params![id, stack_id, v.key, v.value, v.category, v.sensitive, v.hcl],
    )?;
    Ok(c.query_row(
        "SELECT id FROM variables WHERE stack_id = ?1 AND key = ?2 AND category = ?3",
        params![stack_id, v.key, v.category],
        |r| r.get(0),
    )?)
}

pub fn delete_variable(c: &Connection, stack_id: &str, key: &str, category: Option<&str>) -> Result<bool> {
    Ok(c.execute(
        "DELETE FROM variables WHERE stack_id = ?1 AND (key = ?2 OR id = ?2) AND (?3 IS NULL OR category = ?3)",
        params![stack_id, key, category],
    )? > 0)
}

// ---------------------------------------------------------------- tokens

pub fn insert_token(c: &Connection, sha: &str, description: &str) -> Result<String> {
    let id = new_id("tok");
    c.execute(
        "INSERT INTO tokens (id, sha256, description, created_at) VALUES (?1, ?2, ?3, ?4)",
        params![id, sha, description, now()],
    )?;
    Ok(id)
}

pub fn token_hashes(c: &Connection) -> Result<Vec<String>> {
    let mut st = c.prepare("SELECT sha256 FROM tokens")?;
    let rows = st.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Page-number pagination for the TFE API (`page[number]`, `page[size]`, `search[name]`).
pub fn list_stacks_page(c: &Connection, space: &str, search: Option<&str>, limit: i64, offset: i64) -> Result<(Vec<Stack>, i64)> {
    let pattern = search.map(|s| format!("%{}%", s.replace('%', "")));
    let sql = format!(
        "SELECT {STACK_COLS} FROM stacks s JOIN spaces sp ON sp.id = s.space_id
         WHERE sp.name = ?1 AND (?2 IS NULL OR s.name LIKE ?2) ORDER BY s.name LIMIT ?3 OFFSET ?4"
    );
    let mut st = c.prepare_cached(&sql)?;
    let rows = st.query_map(params![space, pattern, limit, offset], stack_row)?.collect::<rusqlite::Result<Vec<_>>>()?;
    let total: i64 = c
        .prepare_cached(
            "SELECT COUNT(*) FROM stacks s JOIN spaces sp ON sp.id = s.space_id WHERE sp.name = ?1 AND (?2 IS NULL OR s.name LIKE ?2)",
        )?
        .query_row(params![space, pattern], |r| r.get(0))?;
    Ok((rows, total))
}

pub fn latest_uploaded_config(c: &Connection, stack_id: &str) -> Result<Option<String>> {
    Ok(c.prepare_cached(
        "SELECT id FROM config_versions WHERE stack_id = ?1 AND status = 'uploaded' ORDER BY rowid DESC LIMIT 1",
    )?
    .query_row([stack_id], |r| r.get(0))
    .optional()?)
}
