//! Scheduler and embedded runner.
//!
//! Event-driven: creating, confirming or finishing a run calls `kick()`, which
//! wakes the dispatcher. There are no polling loops or fixed sleeps anywhere
//! on the run path. Concurrency is a semaphore (default: number of CPUs).
//! Tracked runs hold their stack from planning until they finish, so they are
//! serialized per stack; proposed (speculative) runs never wait for a stack.
//!
//! Each phase runs OpenTofu as a child process in `<data>/work/<run>`. State
//! goes in and out through the local backend file: before `init` the current
//! state version is written to `terraform.tfstate`; after `apply` the file is
//! stored as a new version if it changed. That avoids HTTP round trips from
//! tofu back to the server on every run.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::{watch, Notify, OwnedSemaphorePermit, Semaphore};

use crate::app::AppRef;
use crate::events::Event;
use crate::store::{self, Run, Stack};
use crate::util::{now, sha256_hex};

const PLAN_FILE: &str = ".cairn.tfplan";
const OVERRIDE_FILE: &str = "zz_cairn_backend_override.tf";
const MAX_CHANGES_LISTED: usize = 500;

pub struct Scheduler {
    notify: Notify,
    sem: Arc<Semaphore>,
    cancels: Mutex<HashMap<String, watch::Sender<bool>>>,
    pub concurrency: usize,
}

impl Scheduler {
    pub fn new(concurrency: usize) -> Self {
        Scheduler {
            notify: Notify::new(),
            sem: Arc::new(Semaphore::new(concurrency)),
            cancels: Mutex::new(HashMap::new()),
            concurrency,
        }
    }

    /// Wake the dispatcher.
    pub fn kick(&self) {
        self.notify.notify_one();
    }

    pub fn active(&self) -> usize {
        self.concurrency - self.sem.available_permits()
    }

    /// Signal a running phase to stop. Returns false if the run isn't executing.
    pub fn cancel(&self, run_id: &str) -> bool {
        match self.cancels.lock().unwrap().get(run_id) {
            Some(tx) => tx.send(true).is_ok(),
            None => false,
        }
    }
}

/// Mark runs interrupted by a restart, then dispatch forever.
pub async fn scheduler_loop(app: AppRef) {
    if let Err(e) = recover(&app) {
        tracing::error!("recovery failed: {e:#}");
    }
    loop {
        if let Err(e) = dispatch(&app) {
            tracing::error!("dispatch failed: {e:#}");
        }
        app.sched.notify.notified().await;
    }
}

fn recover(app: &AppRef) -> Result<()> {
    for r in app.db.read(store::active_runs)? {
        if matches!(r.status.as_str(), "planning" | "applying") {
            app.db.write(|c| {
                store::set_run_error(c, &r.id, "interrupted by a server restart")?;
                store::transition(c, &r.id, &["planning", "applying"], "errored")?;
                store::unlock_stack(c, &r.stack_id, Some(&r.id))?;
                Ok(())
            })?;
        }
    }
    Ok(())
}

fn dispatch(app: &AppRef) -> Result<()> {
    let queued = app.db.read(store::queued_runs)?;
    if queued.is_empty() {
        return Ok(());
    }
    // Stacks held by a tracked run that has started (planning … applying).
    let mut held: std::collections::HashSet<String> = app
        .db
        .read(store::active_runs)?
        .into_iter()
        .filter(|r| r.kind != "proposed" && r.status != "plan_queued")
        .map(|r| r.stack_id)
        .collect();

    for run in queued {
        let apply = run.status == "apply_queued";
        if !apply && run.kind != "proposed"
            && held.contains(&run.stack_id) {
                continue;
            }
        let Ok(permit) = app.sched.sem.clone().try_acquire_owned() else {
            break; // a finishing phase will kick us again
        };
        let (from, to) = if apply { ("apply_queued", "applying") } else { ("plan_queued", "planning") };
        if !app.db.write(|c| store::transition(c, &run.id, &[from], to))? {
            continue; // canceled meanwhile
        }
        if run.kind != "proposed" {
            held.insert(run.stack_id.clone());
        }
        publish(app, &run.stack_id, &run.id, to);
        let app2 = app.clone();
        tokio::spawn(async move { execute(app2, run, apply, permit).await });
    }
    Ok(())
}

pub fn publish(app: &AppRef, stack_id: &str, run_id: &str, status: &str) {
    app.hub.publish(Event {
        kind: "run.status",
        stack_id: stack_id.to_string(),
        run_id: Some(run_id.to_string()),
        status: Some(status.to_string()),
        at: now(),
    });
}

/// Context for one run's execution.
struct Exec {
    app: AppRef,
    run: Run,
    stack: Stack,
    work: PathBuf,
    cwd: PathBuf,
    cancel: watch::Receiver<bool>,
}

async fn execute(app: AppRef, run: Run, apply_only: bool, permit: OwnedSemaphorePermit) {
    let (tx, rx) = watch::channel(false);
    app.sched.cancels.lock().unwrap().insert(run.id.clone(), tx);
    let run_id = run.id.clone();
    let stack_id = run.stack_id.clone();

    let result = async {
        let stack = app.db.read(|c| store::get_stack(c, &run.stack_id))?.ok_or_else(|| anyhow!("stack deleted"))?;
        let work = app.dir("work").join(&run.id);
        let cwd = work.join(stack.project_root.trim_matches('/'));
        app.hub.open_log(&run.stack_id, &run.id)?;
        let mut ex = Exec { app: app.clone(), run, stack, work, cwd, cancel: rx };
        if apply_only {
            ex.apply().await
        } else {
            ex.plan_and_maybe_apply().await
        }
    }
    .await;

    app.sched.cancels.lock().unwrap().remove(&run_id);
    if let Err(e) = result {
        let canceled = e.to_string() == "canceled";
        let status = if canceled { "canceled" } else { "errored" };
        let msg = format!("{e:#}");
        app.hub.write_line(&run_id, &json!({"@level": "error", "@message": msg, "type": "cairn"}).to_string());
        let _ = app.db.write(|c| {
            if !canceled {
                store::set_run_error(c, &run_id, &msg)?;
            }
            store::transition(c, &run_id, &["planning", "applying", "planned", "apply_queued"], status)?;
            store::unlock_stack(c, &stack_id, Some(&run_id))?;
            Ok(())
        });
        finish(&app, &stack_id, &run_id, status);
    }
    app.hub.close_log(&run_id);
    drop(permit);
    app.sched.kick();
}

fn finish(app: &AppRef, stack_id: &str, run_id: &str, status: &str) {
    app.metrics.finished(status);
    publish(app, stack_id, run_id, status);
    if !app.cfg.keep_workspaces {
        let _ = std::fs::remove_dir_all(app.dir("work").join(run_id));
    }
}

/// Remove a planned run's workspace after discard.
pub fn cleanup(app: &AppRef, run_id: &str) {
    if !app.cfg.keep_workspaces {
        let _ = std::fs::remove_dir_all(app.dir("work").join(run_id));
    }
}

impl Exec {
    fn log(&self, level: &str, msg: &str) {
        self.app
            .hub
            .write_line(&self.run.id, &json!({"@level": level, "@message": msg, "type": "cairn"}).to_string());
    }

    async fn plan_and_maybe_apply(&mut self) -> Result<()> {
        let started = Instant::now();
        let tracked = self.run.kind != "proposed";
        self.log("info", &format!("cairn: {} run {} on stack {}", self.run.kind, self.run.id, self.stack.name));
        if tracked {
            let info = json!({"ID": self.run.id, "Operation": "OperationTypePlan", "Who": "cairn run",
                              "Info": format!("run {}", self.run.id), "Created": now()});
            let id = self.run.id.clone();
            let sid = self.stack.id.clone();
            if let Err(holder) = self.app.db.write(|c| store::try_lock_stack(c, &sid, &id, &info))? {
                bail!("stack is locked: {holder}");
            }
        }
        self.prepare().await?;
        let bin = self.app.engines.resolve(self.stack.engine_version.as_deref()).await?;
        let env = self.env()?;

        let code = self.tofu(&bin, &["init", "-input=false", "-no-color"], &env, |_| {}).await?;
        if code != 0 {
            bail!("tofu init failed (exit {code})");
        }
        if tracked && self.run.auto_apply {
            return self.plan_and_apply_in_one(&bin, &env, started).await;
        }
        let mut counts = (0i64, 0i64, 0i64);
        let mut changed: Vec<Value> = Vec::new();
        let mut args = vec!["plan", "-input=false", "-json", "-detailed-exitcode"];
        args.push(if tracked { "-out=.cairn.tfplan" } else { "-lock=false" });
        let code = self
            .tofu(&bin, &args, &env, |v| match v["type"].as_str() {
                Some("change_summary") => {
                    let c = &v["changes"];
                    counts = (c["add"].as_i64().unwrap_or(0), c["change"].as_i64().unwrap_or(0), c["remove"].as_i64().unwrap_or(0));
                }
                Some("planned_change") if changed.len() < MAX_CHANGES_LISTED => {
                    let c = &v["change"];
                    changed.push(json!({"address": c["resource"]["addr"], "action": c["action"]}));
                }
                _ => {}
            })
            .await?;
        let has_changes = match code {
            0 => false,
            2 => true,
            c => bail!("tofu plan failed (exit {c})"),
        };
        let summary = json!({"add": counts.0, "change": counts.1, "destroy": counts.2, "resources": changed});
        let run_id = self.run.id.clone();
        self.app.db.write(|c| store::set_plan_result(c, &run_id, has_changes, counts, &summary))?;
        self.app.metrics.phase("plan", started.elapsed().as_secs_f64());

        if !tracked || !has_changes {
            let sid = self.stack.id.clone();
            self.app.db.write(|c| {
                store::transition(c, &run_id, &["planning"], "planned_and_finished")?;
                store::unlock_stack(c, &sid, Some(&run_id))?;
                Ok(())
            })?;
            finish(&self.app, &self.stack.id, &run_id, "planned_and_finished");
            return Ok(());
        }
        if self.run.auto_apply {
            self.app.db.write(|c| {
                store::transition(c, &run_id, &["planning"], "planned")?;
                store::transition(c, &run_id, &["planned"], "applying")
            })?;
            publish(&self.app, &self.stack.id, &run_id, "applying");
            return self.apply().await;
        }
        self.app.db.write(|c| store::transition(c, &run_id, &["planning"], "planned"))?;
        publish(&self.app, &self.stack.id, &run_id, "planned");
        Ok(())
    }

    /// Auto-apply fast path: one `tofu apply -auto-approve` process instead of
    /// `plan -out` + `apply <planfile>`. Nothing sits between plan and apply in
    /// this case (no confirmation), so the saved plan would only cost a second
    /// process start and a plan-file round trip. Status still moves through
    /// planned -> applying when the plan summary appears in the JSON stream.
    async fn plan_and_apply_in_one(&mut self, bin: &Path, env: &[(String, String)], started: Instant) -> Result<()> {
        let (app, run_id, stack_id) = (self.app.clone(), self.run.id.clone(), self.stack.id.clone());
        let mut counts = (0i64, 0i64, 0i64);
        let mut changed: Vec<Value> = Vec::new();
        let mut planned = false;
        let mut plan_secs = 0.0;
        let code = self
            .tofu(bin, &["apply", "-input=false", "-json", "-auto-approve"], env, |v| match v["type"].as_str() {
                Some("planned_change") if changed.len() < MAX_CHANGES_LISTED => {
                    let c = &v["change"];
                    changed.push(json!({"address": c["resource"]["addr"], "action": c["action"]}));
                }
                Some("change_summary") if !planned && v["changes"]["operation"] == "plan" => {
                    let c = &v["changes"];
                    counts = (c["add"].as_i64().unwrap_or(0), c["change"].as_i64().unwrap_or(0), c["remove"].as_i64().unwrap_or(0));
                    planned = true;
                    plan_secs = started.elapsed().as_secs_f64();
                    let has = counts.0 + counts.1 + counts.2 > 0;
                    let summary = json!({"add": counts.0, "change": counts.1, "destroy": counts.2, "resources": changed});
                    let _ = app.db.write(|c| {
                        store::set_plan_result(c, &run_id, has, counts, &summary)?;
                        if has {
                            store::transition(c, &run_id, &["planning"], "planned")?;
                            store::transition(c, &run_id, &["planned"], "applying")?;
                        }
                        Ok(())
                    });
                    if has {
                        publish(&app, &stack_id, &run_id, "applying");
                    }
                }
                _ => {}
            })
            .await?;
        let stored = self.store_state().await?;
        if code != 0 {
            bail!("tofu apply failed (exit {code})");
        }
        self.app.metrics.phase("plan", plan_secs);
        self.app.metrics.phase("apply", started.elapsed().as_secs_f64() - plan_secs);
        let resources_changed = counts.0 + counts.1 + counts.2 > 0;
        // Output-only changes still write state: that counts as applied.
        let status = if resources_changed || stored.is_some() { "applied" } else { "planned_and_finished" };
        if !planned || (!resources_changed && stored.is_some()) {
            let summary = json!({"add": counts.0, "change": counts.1, "destroy": counts.2, "resources": changed});
            self.app.db.write(|c| store::set_plan_result(c, &run_id, stored.is_some(), counts, &summary))?;
        }
        self.app.db.write(|c| {
            store::transition(c, &run_id, &["planning", "applying"], status)?;
            store::unlock_stack(c, &stack_id, Some(&run_id))?;
            Ok(())
        })?;
        if let Some(sv) = stored {
            self.log("info", &format!("cairn: saved state version {} (serial {})", sv.id, sv.serial));
        }
        finish(&self.app, &stack_id, &run_id, status);
        Ok(())
    }

    async fn apply(&mut self) -> Result<()> {
        let started = Instant::now();
        if !self.cwd.join(PLAN_FILE).exists() {
            bail!("saved plan is missing (the workspace was removed); start a new run");
        }
        let bin = self.app.engines.resolve(self.stack.engine_version.as_deref()).await?;
        let env = self.env()?;
        self.log("info", "cairn: applying saved plan");
        let code = self.tofu(&bin, &["apply", "-input=false", "-json", PLAN_FILE], &env, |_| {}).await?;
        // Store whatever state tofu wrote, even after a failed apply (partial changes are real).
        let stored = self.store_state().await?;
        if code != 0 {
            bail!("tofu apply failed (exit {code})");
        }
        if let Some(sv) = stored {
            self.log("info", &format!("cairn: saved state version {} (serial {})", sv.id, sv.serial));
        }
        let (run_id, sid) = (self.run.id.clone(), self.stack.id.clone());
        self.app.db.write(|c| {
            store::transition(c, &run_id, &["applying"], "applied")?;
            store::unlock_stack(c, &sid, Some(&run_id))?;
            Ok(())
        })?;
        self.app.metrics.phase("apply", started.elapsed().as_secs_f64());
        finish(&self.app, &sid, &run_id, "applied");
        Ok(())
    }

    /// Unpack the configuration, seed state and neutralise any configured backend.
    async fn prepare(&self) -> Result<()> {
        let cv = self.run.config_id.as_deref().ok_or_else(|| anyhow!("run has no configuration version"))?;
        let tarball = self.app.blobs.get(&format!("config/{cv}.tar.gz")).await?.context("configuration not uploaded")?;
        let work = self.work.clone();
        let cwd = self.cwd.clone();
        let state = self.app.db.read(|c| store::current_state(c, &self.stack.id))?;
        let state_bytes = match state {
            Some(s) => Some(self.app.blobs.get(&format!("state/{}.json", s.sha256)).await?.context("state blob missing")?),
            None => None,
        };
        tokio::task::spawn_blocking(move || -> Result<()> {
            let _ = std::fs::remove_dir_all(&work);
            std::fs::create_dir_all(&work)?;
            let mut ar = tar::Archive::new(flate2::read::GzDecoder::new(std::io::Cursor::new(tarball)));
            ar.set_preserve_permissions(false);
            ar.unpack(&work).context("unpacking configuration")?;
            std::fs::create_dir_all(&cwd)?;
            std::fs::create_dir_all(work.join(".home"))?;
            if let Some(b) = state_bytes {
                std::fs::write(cwd.join("terraform.tfstate"), b).context("seeding state")?;
            }
            if declares_backend(&cwd)? {
                std::fs::write(cwd.join(OVERRIDE_FILE), "terraform {\n  backend \"local\" {}\n}\n")?;
            }
            Ok(())
        })
        .await??;
        Ok(())
    }

    fn env(&self) -> Result<Vec<(String, String)>> {
        let mut env: Vec<(String, String)> = Vec::new();
        for k in ["PATH", "SSL_CERT_FILE", "SSL_CERT_DIR", "HTTPS_PROXY", "https_proxy", "HTTP_PROXY", "http_proxy",
                  "NO_PROXY", "no_proxy", "TZ", "LANG"] {
            if let Ok(v) = std::env::var(k) {
                env.push((k.into(), v));
            }
        }
        if !env.iter().any(|(k, _)| k == "PATH") {
            env.push(("PATH".into(), "/usr/local/bin:/usr/bin:/bin".into()));
        }
        env.push(("HOME".into(), self.work.join(".home").display().to_string()));
        env.push(("TF_IN_AUTOMATION".into(), "1".into()));
        env.push(("TF_INPUT".into(), "0".into()));
        env.push(("CHECKPOINT_DISABLE".into(), "1".into()));
        env.push(("TF_PLUGIN_CACHE_DIR".into(), self.app.dir("cache/plugins").display().to_string()));
        if self.app.cfg.engine_gogc > 0 {
            env.push(("GOGC".into(), self.app.cfg.engine_gogc.to_string()));
        }
        env.push(("CAIRN_RUN_ID".into(), self.run.id.clone()));
        env.push(("CAIRN_STACK_ID".into(), self.stack.id.clone()));
        let vars = self.app.db.read(|c| store::list_variables(c, &self.stack.id))?;
        for v in vars {
            let value = if v.sensitive { self.app.secrets.open(&v.value)? } else { v.value };
            match v.category.as_str() {
                "env" => env.push((v.key, value)),
                _ => env.push((format!("TF_VAR_{}", v.key), value)),
            }
        }
        Ok(env)
    }

    /// Run tofu, streaming every output line into the run log. JSON lines are
    /// passed to `on_json`; plain lines are wrapped as JSON log records.
    async fn tofu(
        &mut self,
        bin: &Path,
        args: &[&str],
        env: &[(String, String)],
        mut on_json: impl FnMut(&Value),
    ) -> Result<i32> {
        if *self.cancel.borrow() {
            bail!("canceled");
        }
        let mut child = Command::new(bin)
            .args(args)
            .current_dir(&self.cwd)
            .env_clear()
            .envs(env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("starting {}", bin.display()))?;
        let mut out = BufReader::new(child.stdout.take().unwrap()).lines();
        let mut err = BufReader::new(child.stderr.take().unwrap()).lines();
        let (mut out_done, mut err_done) = (false, false);
        let hub = &self.app.hub;
        let run_id = self.run.id.clone();
        let phase = args[0];
        while !(out_done && err_done) {
            tokio::select! {
                l = out.next_line(), if !out_done => match l? {
                    Some(line) => match serde_json::from_str::<Value>(&line) {
                        Ok(v) if v.is_object() => { on_json(&v); hub.write_line(&run_id, &line); }
                        _ => hub.write_line(&run_id, &json!({"@level": "info", "@message": line, "type": phase}).to_string()),
                    },
                    None => out_done = true,
                },
                l = err.next_line(), if !err_done => match l? {
                    Some(line) => hub.write_line(&run_id, &json!({"@level": "error", "@message": line, "type": phase}).to_string()),
                    None => err_done = true,
                },
                _ = self.cancel.changed() => {
                    if *self.cancel.borrow() {
                        // SIGINT first so tofu can release locks and write partial state.
                        if let Some(pid) = child.id() {
                            interrupt(pid);
                        }
                        let _ = tokio::time::timeout(std::time::Duration::from_secs(10), child.wait()).await;
                        let _ = child.kill().await;
                        let _ = self.store_state().await;
                        bail!("canceled");
                    }
                }
            }
        }
        let status = child.wait().await?;
        Ok(status.code().unwrap_or(-1))
    }

    /// Store `terraform.tfstate` as a new state version if it changed.
    async fn store_state(&self) -> Result<Option<store::StateVersion>> {
        let path = self.cwd.join("terraform.tfstate");
        let Ok(bytes) = tokio::fs::read(&path).await else { return Ok(None) };
        let sha = sha256_hex(&bytes);
        let current = self.app.db.read(|c| store::current_state(c, &self.stack.id))?;
        if current.as_ref().map(|s| s.sha256 == sha).unwrap_or(false) {
            return Ok(None);
        }
        let doc: Value = serde_json::from_slice(&bytes).context("state is not valid JSON")?;
        let key = format!("state/{sha}.json");
        if !self.app.blobs.exists(&key).await? {
            self.app.blobs.put(&key, bytes.clone(), true).await?;
        }
        let outputs = doc.get("outputs").cloned().unwrap_or_else(|| json!({}));
        let resources = crate::store::count_instances(&doc);
        let sv = self.app.db.write(|c| {
            store::insert_state(
                c,
                &store::NewState {
                    stack_id: &self.stack.id,
                    serial: doc["serial"].as_i64().unwrap_or(0),
                    lineage: doc["lineage"].as_str().unwrap_or(""),
                    sha256: &sha,
                    size: bytes.len() as i64,
                    run_id: Some(&self.run.id),
                    outputs: &outputs,
                    resource_count: resources,
                },
            )
        })?;
        self.app.hub.publish(Event {
            kind: "state.version",
            stack_id: self.stack.id.clone(),
            run_id: Some(self.run.id.clone()),
            status: None,
            at: now(),
        });
        Ok(Some(sv))
    }
}

/// Send SIGINT (lets tofu release locks and write partial state).
fn interrupt(pid: u32) {
    extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    const SIGINT: i32 = 2;
    // SAFETY: plain syscall wrapper; pid is our own child.
    unsafe {
        kill(pid as i32, SIGINT);
    }
}

/// Does any .tf file declare a `backend` or `cloud` block?
fn declares_backend(dir: &Path) -> Result<bool> {
    for entry in std::fs::read_dir(dir)? {
        let p = entry?.path();
        if p.extension().and_then(|e| e.to_str()) != Some("tf") {
            continue;
        }
        let text = std::fs::read_to_string(&p).unwrap_or_default();
        if text.lines().any(|l| {
            let t = l.trim_start();
            t.starts_with("backend \"") || t.starts_with("cloud {") || t.starts_with("cloud{")
        }) {
            return Ok(true);
        }
    }
    Ok(false)
}
