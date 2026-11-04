//! `cairn login` (OIDC loopback flow), spaces, roles, tokens, audit.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::Subcommand;
use serde_json::{json, Value};

use crate::client::Client;
use crate::Out;

#[derive(Subcommand)]
pub enum SpaceCmd {
    /// Spaces you can see, with your role in each.
    List,
    Create {
        name: String,
        /// Parent space (omit for a top-level space; site admins only).
        #[arg(long)]
        parent: Option<String>,
    },
    Delete { name: String },
}

#[derive(Subcommand)]
pub enum RoleCmd {
    /// Role bindings on a space (admin on the space).
    List { space: String },
    /// Grant a role: `cairn role grant prod writer --team platform`.
    Grant {
        space: String,
        /// reader, planner, writer or admin
        role: String,
        #[arg(long, group = "who")]
        team: Option<String>,
        #[arg(long, group = "who")]
        user: Option<String>,
        #[arg(long, group = "who")]
        token: Option<String>,
    },
    Revoke { space: String, id: String },
}

#[derive(Subcommand)]
pub enum TokenCmd {
    List {
        /// Every token (site admins).
        #[arg(long)]
        all: bool,
    },
    /// Create a token. Without --service it acts as you.
    Create {
        #[arg(long, default_value = "")]
        description: String,
        #[arg(long)]
        service: bool,
        /// For service tokens: SPACE=ROLE, repeatable.
        #[arg(long = "role")]
        roles: Vec<String>,
        #[arg(long)]
        expires_hours: Option<u64>,
    },
    Revoke { id: String },
}

pub fn space(c: &Client, out: &Out, cmd: &SpaceCmd) -> Result<()> {
    match cmd {
        SpaceCmd::List => {
            let v = c.get("/api/v1/spaces")?;
            out.table(&v, &v["items"], &[("NAME", "name"), ("ROLE", "your_role"), ("PARENT", "parent_id"), ("ID", "id")]);
        }
        SpaceCmd::Create { name, parent } => out.value(&c.send("POST", "/api/v1/spaces", &json!({"name": name, "parent": parent}))?),
        SpaceCmd::Delete { name } => {
            c.delete(&format!("/api/v1/spaces/{name}"))?;
            eprintln!("deleted space {name}");
        }
    }
    Ok(())
}

pub fn role(c: &Client, out: &Out, cmd: &RoleCmd) -> Result<()> {
    match cmd {
        RoleCmd::List { space } => {
            let v = c.get(&format!("/api/v1/spaces/{space}/roles"))?;
            out.table(&v, &v["items"], &[("ID", "id"), ("KIND", "principal_kind"), ("PRINCIPAL", "principal"), ("ROLE", "role")]);
        }
        RoleCmd::Grant { space, role, team, user, token } => {
            let (kind, who) = match (team, user, token) {
                (Some(t), _, _) => ("team", t),
                (_, Some(u), _) => ("user", u),
                (_, _, Some(t)) => ("token", t),
                _ => bail!("one of --team, --user or --token is required"),
            };
            let v = c.send("POST", &format!("/api/v1/spaces/{space}/roles"), &json!({"principal_kind": kind, "principal": who, "role": role}))?;
            out.object(&v, &["id", "space_id", "principal_kind", "principal", "role"]);
        }
        RoleCmd::Revoke { space, id } => {
            c.delete(&format!("/api/v1/spaces/{space}/roles/{id}"))?;
        }
    }
    Ok(())
}

pub fn token(c: &Client, out: &Out, cmd: &TokenCmd) -> Result<()> {
    match cmd {
        TokenCmd::List { all } => {
            let v = c.get(&format!("/api/v1/tokens?all={all}"))?;
            out.table(&v, &v["items"], &[("ID", "id"), ("KIND", "kind"), ("DESCRIPTION", "description"), ("EXPIRES", "expires_at"), ("CREATED", "created_at")]);
        }
        TokenCmd::Create { description, service, roles, expires_hours } => {
            let roles: Vec<Value> = roles
                .iter()
                .map(|r| r.split_once('=').map(|(s, ro)| json!({"space": s, "role": ro})).context("--role takes SPACE=ROLE"))
                .collect::<Result<_>>()?;
            let v = c.send("POST", "/api/v1/tokens", &json!({
                "description": description, "kind": if *service { "service" } else { "user" },
                "roles": roles, "expires_in_hours": expires_hours}))?;
            if out.json {
                out.value(&v);
            } else {
                eprintln!("token {} created; it is shown only once:", v["id"].as_str().unwrap_or("?"));
                println!("{}", v["token"].as_str().unwrap_or(""));
            }
        }
        TokenCmd::Revoke { id } => {
            c.delete(&format!("/api/v1/tokens/{id}"))?;
        }
    }
    Ok(())
}

pub fn audit(c: &Client, out: &Out, limit: u32) -> Result<()> {
    let v = c.get(&format!("/api/v1/audit?limit={limit}"))?;
    out.table(&v, &v["items"], &[("AT", "at"), ("PRINCIPAL", "principal"), ("METHOD", "method"), ("PATH", "path"), ("STATUS", "status")]);
    Ok(())
}

/// Browser login: Cairn redirects to the IdP, then back to a loopback port here.
pub fn oidc_login(host: &str, open_browser: bool) -> Result<String> {
    let listener = TcpListener::bind("127.0.0.1:0").context("opening a loopback port")?;
    let port = listener.local_addr()?.port();
    let redirect = format!("http://127.0.0.1:{port}/callback");
    let url = format!("{}/auth/login?redirect={}", host.trim_end_matches('/'), enc(&redirect));
    eprintln!("Opening your browser to log in. If it doesn't open, visit:\n  {url}");
    if open_browser {
        for opener in ["xdg-open", "open", "wslview"] {
            if std::process::Command::new(opener).arg(&url).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn().is_ok() {
                break;
            }
        }
    }
    listener.set_nonblocking(false)?;
    let (mut stream, _) = listener.accept()?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut line = String::new();
    BufReader::new(&stream).read_line(&mut line)?;
    let path = line.split_whitespace().nth(1).unwrap_or("");
    let token = path
        .split_once("token=")
        .map(|(_, t)| t.split('&').next().unwrap_or("").to_string())
        .filter(|t| !t.is_empty());
    let body = if token.is_some() { "Logged in to Cairn. You can close this tab." } else { "Login failed." };
    let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
    token.context("the login did not return a token")
}

fn enc(s: &str) -> String {
    s.bytes()
        .map(|b| if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") })
        .collect()
}
