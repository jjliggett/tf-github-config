//! `cairn policy …`, `cairn webhook …` and run reviews.

use anyhow::{Context, Result};
use clap::Subcommand;
use serde_json::{json, Value};

use crate::client::Client;
use crate::Out;

#[derive(Subcommand)]
pub enum PolicyCmd {
    /// Policies you can see.
    List {
        /// plan, approval, login, push, trigger, task or notification
        #[arg(long)]
        kind: Option<String>,
    },
    Show { id: String },
    /// Create a policy from a Rego file (`package cairn`).
    Create {
        name: String,
        #[arg(long)]
        kind: String,
        /// Rego file (`-` for stdin).
        #[arg(short, long)]
        file: String,
        #[arg(long, env = "CAIRN_SPACE")]
        space: Option<String>,
        /// Labels, e.g. `autoattach:prod` or `autoattach:*`. Repeatable.
        #[arg(long = "label")]
        labels: Vec<String>,
        #[arg(long, default_value = "")]
        description: String,
    },
    /// Replace a policy's body and/or labels.
    Update {
        id: String,
        #[arg(short, long)]
        file: Option<String>,
        #[arg(long = "label")]
        labels: Option<Vec<String>>,
    },
    Delete { id: String },
    /// Attach a policy to a stack explicitly.
    Attach {
        id: String,
        stack: String,
        #[arg(long, env = "CAIRN_SPACE")]
        space: Option<String>,
    },
    Detach {
        id: String,
        stack: String,
        #[arg(long, env = "CAIRN_SPACE")]
        space: Option<String>,
    },
    /// Dry-run a policy: a stored one (by ID) or a local file with --kind.
    Eval {
        /// Stored policy ID (omit to evaluate --file).
        id: Option<String>,
        #[arg(short, long)]
        file: Option<String>,
        #[arg(long)]
        kind: Option<String>,
        /// Input document (JSON file, `-` for stdin).
        #[arg(short, long)]
        input: Option<String>,
    },
}

#[derive(Subcommand)]
pub enum WebhookCmd {
    /// Notification endpoints of a space (admin on the space).
    List { space: String },
    /// Add an endpoint; deliveries are signed with --secret (`X-Cairn-Signature: sha256=…`).
    Create {
        space: String,
        url: String,
        #[arg(long, default_value = "")]
        secret: String,
    },
    Delete { space: String, id: String },
}

fn read_arg(path: &str) -> Result<String> {
    if path == "-" {
        let mut s = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut s)?;
        Ok(s)
    } else {
        std::fs::read_to_string(path).with_context(|| format!("reading {path}"))
    }
}

fn space_q(space: &Option<String>) -> String {
    space.as_ref().map(|s| format!("?space={s}")).unwrap_or_default()
}

pub fn policy(c: &Client, out: &Out, cmd: &PolicyCmd) -> Result<()> {
    let cols: &[(&str, &str)] = &[("ID", "id"), ("NAME", "name"), ("KIND", "kind"), ("SPACE", "space_id"), ("LABELS", "labels")];
    match cmd {
        PolicyCmd::List { kind } => {
            let q = kind.as_ref().map(|k| format!("?kind={k}")).unwrap_or_default();
            let v = c.get(&format!("/api/v1/policies{q}"))?;
            out.table(&v, &v["items"], cols);
        }
        PolicyCmd::Show { id } => out.value(&c.get(&format!("/api/v1/policies/{id}"))?),
        PolicyCmd::Create { name, kind, file, space, labels, description } => {
            let v = c.send("POST", "/api/v1/policies", &json!({"name": name, "kind": kind, "body": read_arg(file)?,
                "space": space, "labels": labels, "description": description}))?;
            out.object(&v, &["id", "name", "kind", "space_id", "labels"]);
        }
        PolicyCmd::Update { id, file, labels } => {
            let body = file.as_deref().map(read_arg).transpose()?;
            let v = c.send("PATCH", &format!("/api/v1/policies/{id}"), &json!({"body": body, "labels": labels}))?;
            out.object(&v, &["id", "name", "kind", "labels", "updated_at"]);
        }
        PolicyCmd::Delete { id } => {
            c.delete(&format!("/api/v1/policies/{id}"))?;
        }
        PolicyCmd::Attach { id, stack, space } => {
            c.send("POST", &format!("/api/v1/policies/{id}/attach/{stack}{}", space_q(space)), &json!({}))?;
        }
        PolicyCmd::Detach { id, stack, space } => {
            c.delete(&format!("/api/v1/policies/{id}/attach/{stack}{}", space_q(space)))?;
        }
        PolicyCmd::Eval { id, file, kind, input } => {
            let input: Value = match input {
                Some(p) => serde_json::from_str(&read_arg(p)?).context("input is not JSON")?,
                None => json!({}),
            };
            let v = match id {
                Some(id) => c.send("POST", &format!("/api/v1/policies/{id}/evaluate"), &json!({"input": input}))?,
                None => {
                    let file = file.as_deref().context("give a policy ID or --file with --kind")?;
                    c.send("POST", "/api/v1/policies/evaluate", &json!({"kind": kind, "body": read_arg(file)?, "input": input}))?
                }
            };
            out.value(&v["result"]);
        }
    }
    Ok(())
}

pub fn webhook(c: &Client, out: &Out, cmd: &WebhookCmd) -> Result<()> {
    match cmd {
        WebhookCmd::List { space } => {
            let v = c.get(&format!("/api/v1/spaces/{space}/webhooks"))?;
            out.table(&v, &v["items"], &[("ID", "id"), ("URL", "url"), ("CREATED", "created_at")]);
        }
        WebhookCmd::Create { space, url, secret } => {
            let v = c.send("POST", &format!("/api/v1/spaces/{space}/webhooks"), &json!({"url": url, "secret": secret}))?;
            out.object(&v, &["id", "space_id", "url"]);
        }
        WebhookCmd::Delete { space, id } => {
            c.delete(&format!("/api/v1/spaces/{space}/webhooks/{id}"))?;
        }
    }
    Ok(())
}
