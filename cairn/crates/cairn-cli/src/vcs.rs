//! `cairn vcs …`: VCS integrations.

use anyhow::Result;
use clap::Subcommand;
use serde_json::json;

use crate::client::Client;
use crate::Out;

#[derive(Subcommand)]
pub enum VcsCmd {
    /// Integrations you can see, with their webhook URLs.
    List,
    /// Connect a provider. Prints the webhook URL and secret to configure on the repository.
    Create {
        name: String,
        /// github, gitlab, bitbucket, azure_devops, gitea or forgejo
        #[arg(long)]
        provider: String,
        /// API token (repo read, commit statuses, PR comments).
        #[arg(long, env = "CAIRN_VCS_TOKEN")]
        access_token: String,
        /// Server or API URL (required for gitea, forgejo, azure_devops; self-hosted GitHub/GitLab).
        #[arg(long)]
        url: Option<String>,
        #[arg(long, env = "CAIRN_SPACE")]
        space: Option<String>,
        /// Bitbucket: account for app passwords / API tokens.
        #[arg(long, default_value = "")]
        username: String,
        /// Webhook secret (generated if omitted).
        #[arg(long)]
        webhook_secret: Option<String>,
    },
    Delete { id: String },
}

pub fn vcs(c: &Client, out: &Out, cmd: &VcsCmd) -> Result<()> {
    match cmd {
        VcsCmd::List => {
            let v = c.get("/api/v1/vcs")?;
            out.table(&v, &v["items"], &[("NAME", "name"), ("PROVIDER", "provider"), ("API", "api_url"), ("WEBHOOK", "webhook_url"), ("ID", "id")]);
        }
        VcsCmd::Create { name, provider, access_token, url, space, username, webhook_secret } => {
            let v = c.send("POST", "/api/v1/vcs", &json!({"name": name, "provider": provider, "token": access_token, "url": url,
                "space": space, "username": username, "webhook_secret": webhook_secret}))?;
            if out.json {
                out.value(&v);
            } else {
                println!("integration {} ({})", v["name"].as_str().unwrap_or(""), v["id"].as_str().unwrap_or(""));
                println!("Add a webhook to the repository (push, pull request and comment events):");
                println!("  URL:    {}", v["webhook_url"].as_str().unwrap_or(""));
                println!("  Secret: {}   (shown once)", v["webhook_secret"].as_str().unwrap_or(""));
                if provider == "azure_devops" {
                    println!("  Azure DevOps service hooks: use basic auth with any username and the secret as password.");
                }
            }
        }
        VcsCmd::Delete { id } => {
            c.delete(&format!("/api/v1/vcs/{id}"))?;
        }
    }
    Ok(())
}
