//! cairn: command-line client and MCP server for Cairn.

mod client;
mod identity;
mod policy;
mod registry;
mod vcs;
mod mcp;

use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use serde_json::{json, Value};

use client::Client;

/// Exit codes (stable, for scripts and agents).
const EXIT_ERROR: u8 = 1;
const EXIT_CHANGES: u8 = 4;
const EXIT_NEEDS_APPROVAL: u8 = 6;

#[derive(Parser)]
#[command(name = "cairn", version, about = "CLI for the Cairn OpenTofu automation server")]
struct Cli {
    /// Server URL [env: CAIRN_HOST, default from `cairn login`, else http://127.0.0.1:8833]
    #[arg(long, global = true, env = "CAIRN_HOST")]
    host: Option<String>,
    /// API token [env: CAIRN_TOKEN, default from `cairn login`]
    #[arg(long, global = true, env = "CAIRN_TOKEN", hide_env_values = true)]
    token: Option<String>,
    /// Output format (defaults to json when stdout is not a terminal).
    #[arg(short, long, global = true, value_enum)]
    output: Option<Format>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Clone, Copy, PartialEq, ValueEnum)]
enum Format {
    Table,
    Json,
}

#[derive(Subcommand)]
enum Cmd {
    /// Log in: with --token, save it; otherwise log in through the server's OIDC provider in a browser.
    Login {
        #[arg(long)]
        host: String,
        #[arg(long)]
        token: Option<String>,
        /// Print the login URL instead of opening a browser.
        #[arg(long)]
        no_browser: bool,
    },
    /// Show who the token belongs to.
    Whoami,
    /// Spaces (folders that scope stacks and roles).
    #[command(subcommand)]
    Space(identity::SpaceCmd),
    /// Role bindings on spaces.
    #[command(subcommand)]
    Role(identity::RoleCmd),
    /// API tokens.
    #[command(subcommand)]
    Token(identity::TokenCmd),
    /// Audit log (site admins).
    Audit {
        #[arg(long, default_value_t = 50)]
        limit: u32,
    },
    /// Policies as code (Rego): plan, approval, login, push, trigger, task, notification.
    #[command(subcommand)]
    Policy(policy::PolicyCmd),
    /// Private module registry.
    #[command(subcommand)]
    Module(registry::ModuleCmd),
    /// Private provider registry.
    #[command(subcommand)]
    Provider(registry::ProviderCmd),
    /// VCS integrations (GitHub, GitLab, Bitbucket, Azure DevOps, Gitea, Forgejo).
    #[command(subcommand)]
    Vcs(vcs::VcsCmd),
    /// Notification webhooks of a space.
    #[command(subcommand)]
    Webhook(policy::WebhookCmd),
    /// Stacks: preview, deploy, state, variables, outputs.
    #[command(subcommand)]
    Stack(StackCmd),
    /// Runs: show, logs, confirm, discard, cancel.
    #[command(subcommand)]
    Run(RunCmd),
    /// Call any API endpoint, e.g. `cairn api GET /api/v1/stacks`.
    Api {
        method: String,
        path: String,
        /// JSON request body.
        #[arg(short, long)]
        data: Option<String>,
    },
    /// MCP server for AI agents.
    #[command(subcommand)]
    Mcp(McpCmd),
}

#[derive(clap::Args, Clone)]
struct StackSel {
    /// Stack ID or name [env: CAIRN_STACK]
    #[arg(short, long, env = "CAIRN_STACK")]
    stack: String,
    /// Space of the stack when using a name.
    #[arg(long, env = "CAIRN_SPACE")]
    space: Option<String>,
}

impl StackSel {
    fn path(&self, sub: &str) -> String {
        let q = self.space.as_ref().map(|s| format!("?space={s}")).unwrap_or_default();
        format!("/api/v1/stacks/{}{sub}{q}", self.stack)
    }
}

#[derive(Subcommand)]
enum StackCmd {
    List {
        #[arg(long)]
        space: Option<String>,
    },
    Show {
        #[command(flatten)]
        sel: StackSel,
    },
    Create {
        name: String,
        #[arg(long, env = "CAIRN_SPACE")]
        space: Option<String>,
        #[arg(long, default_value = "")]
        description: String,
        #[arg(long)]
        engine_version: Option<String>,
        #[arg(long)]
        auto_apply: bool,
        #[arg(long, default_value = "")]
        project_root: String,
        #[arg(long = "label")]
        labels: Vec<String>,
        /// VCS integration (name or ID) to take code from.
        #[arg(long, requires = "repo")]
        vcs: Option<String>,
        /// Repository: owner/name (Azure DevOps: project/repo).
        #[arg(long, requires = "vcs")]
        repo: Option<String>,
        /// Tracked branch.
        #[arg(long, default_value = "main")]
        branch: String,
        /// Allow `/cairn apply` comments on pull requests.
        #[arg(long)]
        pr_apply: bool,
    },
    Delete {
        #[command(flatten)]
        sel: StackSel,
    },
    /// Start a tracked run from the head of the stack's VCS branch.
    Sync {
        #[command(flatten)]
        sel: StackSel,
        /// Follow the logs.
        #[arg(short, long)]
        follow: bool,
    },
    /// Plan the local directory without changing anything (proposed run).
    Preview {
        #[command(flatten)]
        sel: StackSel,
        #[arg(long, default_value = ".")]
        dir: PathBuf,
        /// Exit 4 when the plan has changes, like `tofu plan -detailed-exitcode`.
        #[arg(long)]
        detailed_exitcode: bool,
    },
    /// Plan and apply the local directory (tracked run).
    Deploy {
        #[command(flatten)]
        sel: StackSel,
        #[arg(long, default_value = ".")]
        dir: PathBuf,
        /// Apply without asking.
        #[arg(long)]
        auto_confirm: bool,
    },
    Outputs {
        #[command(flatten)]
        sel: StackSel,
        /// Include sensitive values.
        #[arg(long)]
        sensitive: bool,
    },
    Lock {
        #[command(flatten)]
        sel: StackSel,
        #[arg(long, default_value = "")]
        note: String,
    },
    Unlock {
        #[command(flatten)]
        sel: StackSel,
        /// Release a lock held by a run or a tofu client.
        #[arg(long)]
        force: bool,
    },
    #[command(subcommand)]
    State(StateCmd),
    #[command(subcommand)]
    Var(VarCmd),
}

#[derive(Subcommand)]
enum StateCmd {
    /// Print the current state document.
    Pull {
        #[command(flatten)]
        sel: StackSel,
    },
    Versions {
        #[command(flatten)]
        sel: StackSel,
    },
}

#[derive(Subcommand)]
enum VarCmd {
    List {
        #[command(flatten)]
        sel: StackSel,
    },
    /// Set KEY=VALUE (a Terraform variable unless --env).
    Set {
        #[command(flatten)]
        sel: StackSel,
        assignment: String,
        #[arg(long)]
        env: bool,
        #[arg(long)]
        sensitive: bool,
        #[arg(long)]
        hcl: bool,
    },
    Unset {
        #[command(flatten)]
        sel: StackSel,
        key: String,
        #[arg(long)]
        env: bool,
    },
}

#[derive(Subcommand)]
enum RunCmd {
    List {
        #[command(flatten)]
        sel: StackSel,
        #[arg(long, default_value_t = 20)]
        limit: u32,
    },
    Show {
        id: String,
        /// Wait until the run finishes or needs confirmation.
        #[arg(long)]
        wait: bool,
    },
    Logs {
        id: String,
        #[arg(short, long)]
        follow: bool,
    },
    Confirm {
        id: String,
        /// Follow the apply logs.
        #[arg(short, long)]
        follow: bool,
    },
    Discard { id: String },
    Cancel { id: String },
    /// Approve or reject a run held by approval policies.
    #[command(group = clap::ArgGroup::new("decision").required(true).args(["approve", "reject"]))]
    Review {
        id: String,
        #[arg(long)]
        approve: bool,
        #[arg(long)]
        reject: bool,
        #[arg(short, long, default_value = "")]
        note: String,
    },
    /// Policy evaluations and reviews of a run.
    Policies { id: String },
}

#[derive(Subcommand)]
enum McpCmd {
    /// Serve MCP over stdio (for Claude Code and other MCP clients).
    Serve {
        /// Only expose read-only tools.
        #[arg(long)]
        read_only: bool,
    },
}

// ---------------------------------------------------------------- config

fn config_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config"));
    base.join("cairn").join("config.json")
}

fn load_config() -> Value {
    std::fs::read_to_string(config_path()).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or(json!({}))
}

fn client(cli: &Cli) -> Result<Client> {
    let cfg = load_config();
    let host = cli
        .host
        .clone()
        .or_else(|| cfg["host"].as_str().map(String::from))
        .unwrap_or_else(|| "http://127.0.0.1:8833".into());
    let token = cli
        .token
        .clone()
        .or_else(|| cfg["token"].as_str().map(String::from))
        .context("no token: run `cairn login --host URL --token TOKEN` or set CAIRN_TOKEN")?;
    Ok(Client::new(&host, &token))
}

// ---------------------------------------------------------------- output

pub struct Out {
    pub json: bool,
}

impl Out {
    pub fn value(&self, v: &Value) {
        println!("{}", serde_json::to_string_pretty(v).unwrap_or_default());
    }

    pub fn table(&self, v: &Value, items: &Value, cols: &[(&str, &str)]) {
        if self.json {
            return self.value(v);
        }
        let rows: Vec<Vec<String>> = items
            .as_array()
            .map(|a| a.iter().map(|it| cols.iter().map(|(_, k)| cell(&it[*k])).collect()).collect())
            .unwrap_or_default();
        let widths: Vec<usize> = cols
            .iter()
            .enumerate()
            .map(|(i, (h, _))| rows.iter().map(|r| r[i].len()).max().unwrap_or(0).max(h.len()))
            .collect();
        let line = |cells: Vec<String>| {
            cells.iter().zip(&widths).map(|(c, w)| format!("{c:<w$}")).collect::<Vec<_>>().join("  ").trim_end().to_string()
        };
        println!("{}", line(cols.iter().map(|(h, _)| h.to_string()).collect()));
        for r in rows {
            println!("{}", line(r));
        }
    }

    pub fn object(&self, v: &Value, keys: &[&str]) {
        if self.json {
            return self.value(v);
        }
        for k in keys {
            println!("{:<16} {}", format!("{k}:"), cell(&v[*k]));
        }
    }
}

fn cell(v: &Value) -> String {
    match v {
        Value::Null => "-".into(),
        Value::String(s) => s.clone(),
        Value::Bool(b) => if *b { "yes" } else { "no" }.into(),
        other => other.to_string(),
    }
}

/// Render one JSON log record from a run the way `tofu` would print it.
pub fn render_log(v: &Value) -> Option<String> {
    let msg = v["@message"].as_str()?;
    if v["type"] == "cairn" {
        return Some(format!("» {}", msg.trim_start_matches("cairn: ")));
    }
    if msg.is_empty() && v["type"] != "init" {
        return None;
    }
    let lvl = v["@level"].as_str().unwrap_or("info");
    Some(if lvl == "error" { format!("Error: {msg}") } else { msg.to_string() })
}

fn summary(r: &Value) -> String {
    let counts = if r["has_changes"].is_boolean() {
        format!(" (+{} ~{} -{})", r["adds"].as_i64().unwrap_or(0), r["changes"].as_i64().unwrap_or(0), r["destroys"].as_i64().unwrap_or(0))
    } else {
        String::new()
    };
    let mut s = format!("run {} {}{counts}", r["id"].as_str().unwrap_or("?"), r["status"].as_str().unwrap_or("?"));
    if let Some(e) = r["error"].as_str() {
        s.push_str(&format!("\nerror: {e}"));
    }
    s
}

// ---------------------------------------------------------------- commands

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::from(EXIT_ERROR)
        }
    }
}

fn run(cli: Cli) -> Result<u8> {
    let out = Out { json: cli.output.map(|f| f == Format::Json).unwrap_or(!std::io::stdout().is_terminal()) };
    match &cli.cmd {
        Cmd::Login { host, token, no_browser } => {
            let token = match token {
                Some(t) => t.clone(),
                None => identity::oidc_login(host, !no_browser)?,
            };
            let c = Client::new(host, &token);
            c.get("/api/v1/whoami").context("checking the token")?;
            let path = config_path();
            std::fs::create_dir_all(path.parent().unwrap())?;
            write_private(&path, &serde_json::to_vec_pretty(&json!({"host": host, "token": token}))?)?;
            eprintln!("logged in to {host} (saved to {})", path.display());
            Ok(0)
        }
        Cmd::Mcp(McpCmd::Serve { read_only }) => {
            mcp::serve(client(&cli)?, *read_only)?;
            Ok(0)
        }
        _ => {
            let c = client(&cli)?;
            dispatch(&c, &out, &cli.cmd)
        }
    }
}

fn write_private(path: &std::path::Path, data: &[u8]) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(path)?;
    f.write_all(data)?;
    Ok(())
}

fn dispatch(c: &Client, out: &Out, cmd: &Cmd) -> Result<u8> {
    match cmd {
        Cmd::Login { .. } | Cmd::Mcp(_) => unreachable!(),
        Cmd::Whoami => out.value(&c.get("/api/v1/whoami")?),
        Cmd::Api { method, path, data } => {
            let v = match (method.to_uppercase().as_str(), data) {
                ("GET", _) => c.get(path)?,
                ("DELETE", _) => c.delete(path)?,
                (m, d) => c.send(m, path, &d.as_deref().map(serde_json::from_str).transpose()?.unwrap_or(json!({})))?,
            };
            out.value(&v);
        }
        Cmd::Space(cmd) => identity::space(c, out, cmd)?,
        Cmd::Role(cmd) => identity::role(c, out, cmd)?,
        Cmd::Token(cmd) => identity::token(c, out, cmd)?,
        Cmd::Audit { limit } => identity::audit(c, out, *limit)?,
        Cmd::Policy(cmd) => policy::policy(c, out, cmd)?,
        Cmd::Webhook(cmd) => policy::webhook(c, out, cmd)?,
        Cmd::Vcs(cmd) => vcs::vcs(c, out, cmd)?,
        Cmd::Module(cmd) => registry::module(c, out, cmd)?,
        Cmd::Provider(cmd) => registry::provider(c, out, cmd)?,
        Cmd::Stack(s) => return stack(c, out, s),
        Cmd::Run(r) => return run_cmd(c, out, r),
    }
    Ok(0)
}

const STACK_COLS: &[(&str, &str)] =
    &[("NAME", "name"), ("SPACE", "space"), ("ID", "id"), ("AUTO-APPLY", "auto_apply"), ("LOCKED", "locked"), ("LATEST RUN", "latest_run_id")];

fn stack(c: &Client, out: &Out, cmd: &StackCmd) -> Result<u8> {
    match cmd {
        StackCmd::List { space } => {
            let mut all = vec![];
            let mut after = String::new();
            loop {
                let q = space.as_ref().map(|s| format!("&space={s}")).unwrap_or_default();
                let v = c.get(&format!("/api/v1/stacks?limit=200&after={}{q}", urlenc(&after)))?;
                all.extend(v["items"].as_array().cloned().unwrap_or_default());
                match v["next"].as_str() {
                    Some(n) => after = n.to_string(),
                    None => break,
                }
            }
            let v = json!({"items": all});
            out.table(&v, &v["items"], STACK_COLS);
        }
        StackCmd::Show { sel } => {
            let v = c.get(&sel.path(""))?;
            out.object(&v, &["id", "name", "space", "description", "engine_version", "auto_apply", "project_root",
                             "labels", "locked", "latest_run_id", "current_state_id", "created_at"]);
        }
        StackCmd::Create { name, space, description, engine_version, auto_apply, project_root, labels, vcs, repo, branch, pr_apply } => {
            let vcs = vcs.as_ref().map(|i| json!({"integration": i, "repo": repo, "branch": branch, "pr_apply": pr_apply}));
            let v = c.send("POST", "/api/v1/stacks", &json!({
                "name": name, "space": space, "description": description, "engine_version": engine_version,
                "auto_apply": auto_apply, "project_root": project_root, "labels": labels, "vcs": vcs}))?;
            out.object(&v, &["id", "name", "space", "auto_apply"]);
        }
        StackCmd::Sync { sel, follow: f } => {
            let r = c.send("POST", &sel.path("/sync"), &json!({}))?;
            if *f {
                let r = follow(c, out, &r, 0)?.1;
                report(out, &r);
                return Ok(exit_for(&r, false));
            }
            report(out, &r);
        }
        StackCmd::Delete { sel } => {
            c.delete(&sel.path(""))?;
            eprintln!("deleted {}", sel.stack);
        }
        StackCmd::Preview { sel, dir, detailed_exitcode } => {
            let r = start_run(c, sel, dir, "proposed", None)?;
            let r = follow(c, out, &r, 0)?.1;
            report(out, &r);
            return Ok(exit_for(&r, *detailed_exitcode));
        }
        StackCmd::Deploy { sel, dir, auto_confirm } => {
            let r = start_run(c, sel, dir, "tracked", auto_confirm.then_some(true))?;
            let (seen, mut r) = follow(c, out, &r, 0)?;
            if r["status"] == "planned" {
                let id = r["id"].as_str().unwrap_or_default().to_string();
                if !std::io::stdin().is_terminal() || out.json {
                    report(out, &r);
                    eprintln!("run {id} needs confirmation: cairn run confirm {id} --follow");
                    return Ok(EXIT_NEEDS_APPROVAL);
                }
                eprint!("\nApply these changes? Only 'yes' will be accepted: ");
                let mut answer = String::new();
                std::io::stdin().read_line(&mut answer)?;
                if answer.trim() != "yes" {
                    c.send("POST", &format!("/api/v1/runs/{id}/discard"), &json!({}))?;
                    eprintln!("discarded");
                    return Ok(0);
                }
                c.send("POST", &format!("/api/v1/runs/{id}/confirm"), &json!({}))?;
                r = follow(c, out, &r, seen)?.1;
            }
            report(out, &r);
            return Ok(exit_for(&r, false));
        }
        StackCmd::Outputs { sel, sensitive } => {
            let v = c.get(&format!("{}{}sensitive={sensitive}", sel.path("/outputs"), if sel.space.is_some() { "&" } else { "?" }))?;
            if out.json {
                out.value(&v);
            } else if let Some(m) = v.as_object() {
                for (k, o) in m {
                    let val = if o["value"].is_null() && o["sensitive"] == true { "<sensitive>".into() } else { o["value"].to_string() };
                    println!("{k} = {val}");
                }
            }
        }
        StackCmd::Lock { sel, note } => {
            let v = c.send("POST", &sel.path("/lock"), &json!({"note": note}))?;
            out.object(&v, &["name", "locked"]);
        }
        StackCmd::Unlock { sel, force } => {
            let p = sel.path("/lock");
            let p = if *force { format!("{p}{}force=true", if p.contains('?') { "&" } else { "?" }) } else { p };
            let v = c.delete(&p)?;
            out.object(&v, &["name", "locked"]);
        }
        StackCmd::State(StateCmd::Pull { sel }) => {
            let raw = c.get_raw(&sel.path("/state"))?;
            std::io::stdout().write_all(&raw)?;
        }
        StackCmd::State(StateCmd::Versions { sel }) => {
            let v = c.get(&sel.path("/state/versions"))?;
            out.table(&v, &v["items"], &[("SERIAL", "serial"), ("ID", "id"), ("RESOURCES", "resource_count"), ("RUN", "run_id"), ("CREATED", "created_at")]);
        }
        StackCmd::Var(VarCmd::List { sel }) => {
            let v = c.get(&sel.path("/variables"))?;
            out.table(&v, &v["items"], &[("KEY", "key"), ("CATEGORY", "category"), ("SENSITIVE", "sensitive"), ("VALUE", "value")]);
        }
        StackCmd::Var(VarCmd::Set { sel, assignment, env, sensitive, hcl }) => {
            let (k, v) = assignment.split_once('=').context("expected KEY=VALUE")?;
            let r = c.send("POST", &sel.path("/variables"), &json!({
                "key": k, "value": v, "category": if *env { "env" } else { "terraform" }, "sensitive": sensitive, "hcl": hcl}))?;
            out.object(&r, &["key", "category", "sensitive"]);
        }
        StackCmd::Var(VarCmd::Unset { sel, key, env }) => {
            let p = sel.path(&format!("/variables/{key}"));
            let cat = if *env { "env" } else { "terraform" };
            c.delete(&format!("{p}{}category={cat}", if p.contains('?') { "&" } else { "?" }))?;
        }
    }
    Ok(0)
}

fn urlenc(s: &str) -> String {
    s.bytes()
        .map(|b| if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") })
        .collect()
}

fn start_run(c: &Client, sel: &StackSel, dir: &std::path::Path, kind: &str, auto_apply: Option<bool>) -> Result<Value> {
    if !dir.is_dir() {
        bail!("{} is not a directory", dir.display());
    }
    let tarball = client::pack_dir(dir)?;
    let mut q = format!("kind={kind}");
    if let Some(a) = auto_apply {
        q.push_str(&format!("&auto_apply={a}"));
    }
    if let Some(s) = &sel.space {
        q.push_str(&format!("&space={s}"));
    }
    let r = c.post_bytes(&format!("/api/v1/stacks/{}/runs?{q}", sel.stack), &tarball)?;
    eprintln!("» started {kind} run {} ({} KiB uploaded)", r["id"].as_str().unwrap_or("?"), tarball.len() / 1024 + 1);
    Ok(r)
}

/// Print logs until the run rests, then return (lines seen, final run).
fn follow(c: &Client, out: &Out, r: &Value, offset: u64) -> Result<(u64, Value)> {
    let id = r["id"].as_str().unwrap_or_default().to_string();
    let seen = c.follow_logs(&id, offset, |line| {
        if out.json {
            return;
        }
        if let Some(s) = render_log(line) {
            println!("{s}");
        }
    })?;
    let r = c.wait_resting(&id, Duration::from_secs(24 * 3600))?;
    Ok((seen, r))
}

fn report(out: &Out, r: &Value) {
    if out.json {
        out.value(r);
    } else {
        println!("\n{}", summary(r));
    }
}

fn exit_for(r: &Value, detailed: bool) -> u8 {
    match r["status"].as_str() {
        Some("errored" | "canceled") => EXIT_ERROR,
        _ if detailed && r["has_changes"] == true => EXIT_CHANGES,
        _ => 0,
    }
}

fn run_cmd(c: &Client, out: &Out, cmd: &RunCmd) -> Result<u8> {
    match cmd {
        RunCmd::List { sel, limit } => {
            let v = c.get(&format!("{}{}limit={limit}", sel.path("/runs"), if sel.space.is_some() { "&" } else { "?" }))?;
            out.table(&v, &v["items"], &[("ID", "id"), ("KIND", "kind"), ("STATUS", "status"), ("+", "adds"), ("~", "changes"), ("-", "destroys"), ("CREATED", "created_at")]);
        }
        RunCmd::Show { id, wait } => {
            let v = if *wait { c.wait_resting(id, Duration::from_secs(24 * 3600))? } else { c.get(&format!("/api/v1/runs/{id}"))? };
            out.object(&v, &["id", "stack_id", "kind", "status", "has_changes", "adds", "changes", "destroys", "error", "created_at"]);
            return Ok(exit_for(&v, false));
        }
        RunCmd::Logs { id, follow: f } => {
            if *f {
                c.follow_logs(id, 0, |l| if let Some(s) = render_log(l) { println!("{s}") })?;
            } else {
                let raw = c.get_raw(&format!("/api/v1/runs/{id}/logs"))?;
                for line in raw.split(|&b| b == b'\n') {
                    if let Ok(v) = serde_json::from_slice::<Value>(line) {
                        if out.json {
                            println!("{v}");
                        } else if let Some(s) = render_log(&v) {
                            println!("{s}");
                        }
                    }
                }
            }
        }
        RunCmd::Confirm { id, follow: f } => {
            let r = c.send("POST", &format!("/api/v1/runs/{id}/confirm"), &json!({}))?;
            if *f {
                let lines = c.get_raw(&format!("/api/v1/runs/{id}/logs"))?.split(|&b| b == b'\n').filter(|l| !l.is_empty()).count();
                let r = follow(c, out, &r, lines as u64)?.1;
                report(out, &r);
                return Ok(exit_for(&r, false));
            }
            report(out, &r);
        }
        RunCmd::Discard { id } => report(out, &c.send("POST", &format!("/api/v1/runs/{id}/discard"), &json!({}))?),
        RunCmd::Cancel { id } => report(out, &c.send("POST", &format!("/api/v1/runs/{id}/cancel"), &json!({}))?),
        RunCmd::Review { id, approve, reject: _, note } => {
            let decision = if *approve { "approve" } else { "reject" };
            report(out, &c.send("POST", &format!("/api/v1/runs/{id}/reviews"), &json!({"decision": decision, "note": note}))?)
        }
        RunCmd::Policies { id } => {
            let v = c.get(&format!("/api/v1/runs/{id}/policies"))?;
            if out.json {
                out.value(&v);
            } else {
                println!("approval: {}", v["approval"].as_str().unwrap_or("-"));
                for e in v["evaluations"].as_array().into_iter().flatten() {
                    println!("{:<12} {:<24} {}", e["kind"].as_str().unwrap_or(""), e["policy"].as_str().unwrap_or(""), e["result"]);
                }
                for r in v["reviews"].as_array().into_iter().flatten() {
                    println!("review       {:<24} {} {}", r["principal"].as_str().unwrap_or(""), r["decision"].as_str().unwrap_or(""), r["note"].as_str().unwrap_or(""));
                }
            }
        }
    }
    Ok(0)
}
