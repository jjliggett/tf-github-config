//! VCS providers: GitHub, GitLab, Bitbucket Cloud, Azure DevOps, Gitea and
//! Forgejo.
//!
//! Everything provider-specific lives here and nowhere else:
//! * [`verify`]: webhook authenticity (HMAC signatures, GitLab's token,
//!   Azure DevOps basic auth);
//! * [`parse`]: payloads normalized into [`Event`]s (push, pull request,
//!   comment);
//! * [`Client`]: source archives, commit statuses and pull request comments
//!   over each provider's REST API.
//!
//! Code is fetched as an archive of one commit (no git, no clone, no
//! working copy kept around), normalized into the same `.tar.gz` layout a
//! CLI upload produces.

use std::io::Read;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use axum::http::HeaderMap;
use base64::Engine as _;
use hmac::{Hmac, Mac};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use serde_json::{json, Value};
use sha2::Sha256;

use crate::util::{ct_eq, new_id, now};

pub const PROVIDERS: &[&str] = &["github", "gitlab", "bitbucket", "azure_devops", "gitea", "forgejo"];
const MAX_ARCHIVE: u64 = 100 << 20;

#[derive(Debug, Clone, Serialize)]
pub struct Integration {
    pub id: String,
    pub space_id: String,
    pub provider: String,
    pub name: String,
    pub api_url: String,
    pub web_url: String,
    pub username: String,
    #[serde(skip)]
    pub token: String,
    #[serde(skip)]
    pub secret: String,
    pub created_at: String,
}

/// Default API base for a provider (None: the URL is required).
pub fn default_api_url(provider: &str) -> Option<&'static str> {
    match provider {
        "github" => Some("https://api.github.com"),
        "gitlab" => Some("https://gitlab.com/api/v4"),
        "bitbucket" => Some("https://api.bitbucket.org/2.0"),
        _ => None,
    }
}

/// Normalize the user's URL into an API base (Gitea/Forgejo: append `/api/v1`).
pub fn api_base(provider: &str, url: Option<&str>) -> Result<String> {
    let u = match url {
        Some(u) if !u.is_empty() => u.trim_end_matches('/').to_string(),
        _ => default_api_url(provider)
            .ok_or_else(|| anyhow!("url is required for {provider} (e.g. https://git.example.com or https://dev.azure.com/org)"))?
            .to_string(),
    };
    if !u.starts_with("http://") && !u.starts_with("https://") {
        bail!("url must be http(s)");
    }
    Ok(match provider {
        "gitea" | "forgejo" if !u.ends_with("/api/v1") => format!("{u}/api/v1"),
        _ => u,
    })
}

const COLS: &str = "id, space_id, provider, name, api_url, web_url, username, token, secret, created_at";

fn row(r: &rusqlite::Row) -> rusqlite::Result<Integration> {
    Ok(Integration {
        id: r.get(0)?,
        space_id: r.get(1)?,
        provider: r.get(2)?,
        name: r.get(3)?,
        api_url: r.get(4)?,
        web_url: r.get(5)?,
        username: r.get(6)?,
        token: r.get(7)?,
        secret: r.get(8)?,
        created_at: r.get(9)?,
    })
}

pub struct NewIntegration<'a> {
    pub space_id: &'a str,
    pub provider: &'a str,
    pub name: &'a str,
    pub api_url: &'a str,
    pub web_url: &'a str,
    pub username: &'a str,
    /// Sealed.
    pub token: &'a str,
    /// Sealed.
    pub secret: &'a str,
}

pub fn create(c: &Connection, n: &NewIntegration) -> Result<Integration> {
    let id = new_id("vcs");
    c.execute(
        &format!("INSERT INTO vcs_integrations ({COLS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)"),
        params![id, n.space_id, n.provider, n.name, n.api_url, n.web_url, n.username, n.token, n.secret, now()],
    )?;
    Ok(get(c, &id)?.expect("just inserted"))
}

pub fn get(c: &Connection, key: &str) -> Result<Option<Integration>> {
    Ok(c
        .prepare_cached(&format!("SELECT {COLS} FROM vcs_integrations WHERE id = ?1 OR name = ?1"))?
        .query_row([key], row)
        .optional()?)
}

pub fn list(c: &Connection) -> Result<Vec<Integration>> {
    let mut st = c.prepare_cached(&format!("SELECT {COLS} FROM vcs_integrations ORDER BY name"))?;
    let rows = st.query_map([], row)?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub fn delete(c: &Connection, id: &str) -> Result<bool> {
    Ok(c.execute("DELETE FROM vcs_integrations WHERE id = ?1", [id])? > 0)
}

pub fn comment_id(c: &Connection, vcs_id: &str, repo: &str, pr: i64) -> Result<Option<String>> {
    Ok(c
        .prepare_cached("SELECT comment_id FROM vcs_comments WHERE vcs_id = ?1 AND repo = ?2 AND pr = ?3")?
        .query_row(params![vcs_id, repo, pr], |r| r.get(0))
        .optional()?)
}

pub fn set_comment_id(c: &Connection, vcs_id: &str, repo: &str, pr: i64, id: &str) -> Result<()> {
    c.execute(
        "INSERT INTO vcs_comments (vcs_id, repo, pr, comment_id) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT (vcs_id, repo, pr) DO UPDATE SET comment_id = excluded.comment_id",
        params![vcs_id, repo, pr, id],
    )?;
    Ok(())
}

// ---------------------------------------------------------------- webhooks

fn header<'a>(h: &'a HeaderMap, name: &str) -> Option<&'a str> {
    h.get(name).and_then(|v| v.to_str().ok())
}

fn hmac_hex(secret: &str, body: &[u8]) -> String {
    let mut m = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("any key");
    m.update(body);
    hex::encode(m.finalize().into_bytes())
}

/// Check that a webhook delivery really comes from the provider.
pub fn verify(provider: &str, secret: &str, h: &HeaderMap, body: &[u8]) -> Result<()> {
    let ok = match provider {
        "github" => header(h, "x-hub-signature-256").map(|s| ct_eq(s.as_bytes(), format!("sha256={}", hmac_hex(secret, body)).as_bytes())),
        "bitbucket" => header(h, "x-hub-signature").map(|s| ct_eq(s.as_bytes(), format!("sha256={}", hmac_hex(secret, body)).as_bytes())),
        "gitea" | "forgejo" => {
            let want = hmac_hex(secret, body);
            header(h, "x-forgejo-signature")
                .or_else(|| header(h, "x-gitea-signature"))
                .map(|s| ct_eq(s.as_bytes(), want.as_bytes()))
                .or_else(|| header(h, "x-hub-signature-256").map(|s| ct_eq(s.as_bytes(), format!("sha256={want}").as_bytes())))
        }
        "gitlab" => header(h, "x-gitlab-token").map(|t| ct_eq(t.as_bytes(), secret.as_bytes())),
        // Service hooks send HTTP basic auth; the password is the secret.
        "azure_devops" => header(h, "authorization").and_then(|a| a.strip_prefix("Basic ")).map(|b| {
            base64::engine::general_purpose::STANDARD
                .decode(b.trim())
                .ok()
                .and_then(|d| String::from_utf8(d).ok())
                .and_then(|up| up.split_once(':').map(|(_, p)| ct_eq(p.as_bytes(), secret.as_bytes())))
                .unwrap_or(false)
        }),
        _ => None,
    };
    match ok {
        Some(true) => Ok(()),
        Some(false) => bail!("signature mismatch"),
        None => bail!("missing signature header"),
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PrAction {
    Opened,
    Updated,
    Closed,
    Merged,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    Push {
        repo: String,
        branch: String,
        sha: String,
        message: String,
        author: String,
        /// Changed paths, when the provider includes them.
        files: Option<Vec<String>>,
    },
    PullRequest {
        repo: String,
        number: i64,
        action: PrAction,
        head_sha: String,
        head_branch: String,
        base_branch: String,
        title: String,
        author: String,
    },
    Comment {
        repo: String,
        number: i64,
        body: String,
        author: String,
    },
}

impl Event {
    pub fn repo(&self) -> &str {
        match self {
            Event::Push { repo, .. } | Event::PullRequest { repo, .. } | Event::Comment { repo, .. } => repo,
        }
    }
}

fn s(v: &Value) -> String {
    v.as_str().unwrap_or_default().to_string()
}

fn branch_of(r: &str) -> Option<String> {
    r.strip_prefix("refs/heads/").map(str::to_string)
}

fn commit_files(commits: &Value) -> Option<Vec<String>> {
    let commits = commits.as_array()?;
    let mut out = vec![];
    for c in commits {
        for k in ["added", "modified", "removed"] {
            for f in c[k].as_array()? {
                out.push(s(f));
            }
        }
    }
    out.sort();
    out.dedup();
    Some(out)
}

/// Turn a webhook delivery into events (empty: nothing to do, e.g. pings).
pub fn parse(provider: &str, h: &HeaderMap, body: &[u8]) -> Result<Vec<Event>> {
    let v: Value = serde_json::from_slice(body).context("payload is not JSON")?;
    Ok(match provider {
        "github" | "gitea" | "forgejo" => {
            let kind = if provider == "github" {
                header(h, "x-github-event")
            } else {
                header(h, "x-forgejo-event").or_else(|| header(h, "x-gitea-event"))
            }
            .unwrap_or_default();
            parse_github_like(kind, &v).into_iter().collect()
        }
        "gitlab" => parse_gitlab(header(h, "x-gitlab-event").unwrap_or_default(), &v).into_iter().collect(),
        "bitbucket" => parse_bitbucket(header(h, "x-event-key").unwrap_or_default(), &v),
        "azure_devops" => parse_azure(&v).into_iter().collect(),
        _ => vec![],
    })
}

fn parse_github_like(kind: &str, v: &Value) -> Option<Event> {
    let repo = s(&v["repository"]["full_name"]);
    match kind {
        "push" => {
            let sha = s(&v["after"]);
            if sha.is_empty() || sha.chars().all(|c| c == '0') {
                return None; // branch deleted
            }
            Some(Event::Push {
                repo,
                branch: branch_of(v["ref"].as_str()?)?,
                message: s(&v["head_commit"]["message"]),
                author: v["pusher"]["name"].as_str().or(v["pusher"]["login"].as_str()).or(v["pusher"]["username"].as_str()).unwrap_or_default().into(),
                files: commit_files(&v["commits"]),
                sha,
            })
        }
        "pull_request" => {
            let pr = &v["pull_request"];
            let action = match v["action"].as_str()? {
                "opened" | "reopened" => PrAction::Opened,
                "synchronize" | "synchronized" => PrAction::Updated,
                "closed" if pr["merged"].as_bool() == Some(true) => PrAction::Merged,
                "closed" => PrAction::Closed,
                _ => return None,
            };
            Some(Event::PullRequest {
                repo,
                number: pr["number"].as_i64().or(v["number"].as_i64())?,
                action,
                head_sha: s(&pr["head"]["sha"]),
                head_branch: s(&pr["head"]["ref"]),
                base_branch: s(&pr["base"]["ref"]),
                title: s(&pr["title"]),
                author: s(&pr["user"]["login"]),
            })
        }
        "issue_comment" | "pull_request_comment" => {
            let is_pr = !v["issue"]["pull_request"].is_null() || v["is_pull"].as_bool() == Some(true);
            if !is_pr || v["action"].as_str() != Some("created") {
                return None;
            }
            Some(Event::Comment { repo, number: v["issue"]["number"].as_i64()?, body: s(&v["comment"]["body"]), author: s(&v["comment"]["user"]["login"]) })
        }
        _ => None,
    }
}

fn parse_gitlab(kind: &str, v: &Value) -> Option<Event> {
    let repo = s(&v["project"]["path_with_namespace"]);
    match kind {
        "Push Hook" => {
            let sha = s(&v["checkout_sha"]);
            if sha.is_empty() {
                return None;
            }
            let commits = &v["commits"];
            Some(Event::Push {
                repo,
                branch: branch_of(v["ref"].as_str()?)?,
                message: commits.as_array().and_then(|c| c.last()).map(|c| s(&c["message"])).unwrap_or_default(),
                author: s(&v["user_username"]),
                files: commit_files(commits),
                sha,
            })
        }
        "Merge Request Hook" => {
            let a = &v["object_attributes"];
            let action = match a["action"].as_str()? {
                "open" | "reopen" => PrAction::Opened,
                "update" if !a["oldrev"].is_null() => PrAction::Updated,
                "merge" => PrAction::Merged,
                "close" => PrAction::Closed,
                _ => return None,
            };
            Some(Event::PullRequest {
                repo,
                number: a["iid"].as_i64()?,
                action,
                head_sha: s(&a["last_commit"]["id"]),
                head_branch: s(&a["source_branch"]),
                base_branch: s(&a["target_branch"]),
                title: s(&a["title"]),
                author: s(&v["user"]["username"]),
            })
        }
        "Note Hook" if v["object_attributes"]["noteable_type"] == "MergeRequest" => Some(Event::Comment {
            repo,
            number: v["merge_request"]["iid"].as_i64()?,
            body: s(&v["object_attributes"]["note"]),
            author: s(&v["user"]["username"]),
        }),
        _ => None,
    }
}

fn parse_bitbucket(kind: &str, v: &Value) -> Vec<Event> {
    let repo = s(&v["repository"]["full_name"]);
    let author = s(&v["actor"]["display_name"]);
    let pr = &v["pullrequest"];
    let pr_event = |action: PrAction| Event::PullRequest {
        repo: repo.clone(),
        number: pr["id"].as_i64().unwrap_or_default(),
        action,
        head_sha: s(&pr["source"]["commit"]["hash"]),
        head_branch: s(&pr["source"]["branch"]["name"]),
        base_branch: s(&pr["destination"]["branch"]["name"]),
        title: s(&pr["title"]),
        author: author.clone(),
    };
    match kind {
        "repo:push" => v["push"]["changes"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|c| c["new"]["type"] == "branch")
            .map(|c| Event::Push {
                repo: repo.clone(),
                branch: s(&c["new"]["name"]),
                sha: s(&c["new"]["target"]["hash"]),
                message: s(&c["new"]["target"]["message"]),
                author: author.clone(),
                files: None,
            })
            .collect(),
        "pullrequest:created" => vec![pr_event(PrAction::Opened)],
        "pullrequest:updated" => vec![pr_event(PrAction::Updated)],
        "pullrequest:fulfilled" => vec![pr_event(PrAction::Merged)],
        "pullrequest:rejected" => vec![pr_event(PrAction::Closed)],
        "pullrequest:comment_created" => vec![Event::Comment {
            repo,
            number: pr["id"].as_i64().unwrap_or_default(),
            body: s(&v["comment"]["content"]["raw"]),
            author,
        }],
        _ => vec![],
    }
}

fn parse_azure(v: &Value) -> Option<Event> {
    let r = &v["resource"];
    let repo_of = |repo: &Value| format!("{}/{}", s(&repo["project"]["name"]), s(&repo["name"]));
    match v["eventType"].as_str()? {
        "git.push" => {
            let upd = r["refUpdates"].as_array()?.iter().find(|u| u["name"].as_str().map(|n| n.starts_with("refs/heads/")).unwrap_or(false))?;
            let sha = s(&upd["newObjectId"]);
            if sha.chars().all(|c| c == '0') {
                return None;
            }
            Some(Event::Push {
                repo: repo_of(&r["repository"]),
                branch: branch_of(upd["name"].as_str()?)?,
                sha,
                message: r["commits"].as_array().and_then(|c| c.first()).map(|c| s(&c["comment"])).unwrap_or_default(),
                author: s(&r["pushedBy"]["displayName"]),
                files: None,
            })
        }
        t @ ("git.pullrequest.created" | "git.pullrequest.updated" | "git.pullrequest.merged") => {
            let action = match (t, r["status"].as_str().unwrap_or("active")) {
                (_, "abandoned") => PrAction::Closed,
                ("git.pullrequest.merged", _) | (_, "completed") => PrAction::Merged,
                ("git.pullrequest.created", _) => PrAction::Opened,
                _ => PrAction::Updated,
            };
            Some(Event::PullRequest {
                repo: repo_of(&r["repository"]),
                number: r["pullRequestId"].as_i64()?,
                action,
                head_sha: s(&r["lastMergeSourceCommit"]["commitId"]),
                head_branch: branch_of(r["sourceRefName"].as_str()?)?,
                base_branch: branch_of(r["targetRefName"].as_str()?)?,
                title: s(&r["title"]),
                author: s(&r["createdBy"]["displayName"]),
            })
        }
        "ms.vss-code.git-pullrequest-comment-event" => Some(Event::Comment {
            repo: repo_of(&r["pullRequest"]["repository"]),
            number: r["pullRequest"]["pullRequestId"].as_i64()?,
            body: s(&r["comment"]["content"]),
            author: s(&r["comment"]["author"]["displayName"]),
        }),
        _ => None,
    }
}

/// `/cairn plan [stack…]` / `/cairn apply [stack…]` lines in a comment.
pub fn parse_command(body: &str) -> Option<(String, Vec<String>)> {
    for line in body.lines() {
        let mut w = line.split_whitespace();
        if !matches!(w.next(), Some("/cairn" | "cairn")) {
            continue;
        }
        let cmd = w.next()?;
        if matches!(cmd, "plan" | "apply") {
            return Some((cmd.to_string(), w.map(str::to_string).collect()));
        }
    }
    None
}

// ---------------------------------------------------------------- API client

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum State {
    Pending,
    Success,
    Failure,
}

/// Talks to one integration's API with its (unsealed) token.
pub struct Client<'a> {
    pub i: &'a Integration,
    pub token: String,
}

fn enc(s: &str) -> String {
    s.bytes()
        .map(|b| if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") })
        .collect()
}

/// Azure DevOps repos are `project/repo`.
fn azure_parts(repo: &str) -> Result<(&str, &str)> {
    repo.split_once('/').ok_or_else(|| anyhow!("Azure DevOps repositories are named project/repo"))
}

impl Client<'_> {
    fn api(&self) -> &str {
        self.i.api_url.trim_end_matches('/')
    }

    fn req(&self, method: &str, url: &str) -> ureq::Request {
        let r = crate::http::agent(url).request(method, url).timeout(Duration::from_secs(60)).set("User-Agent", "cairn");
        let b64 = |s: String| base64::engine::general_purpose::STANDARD.encode(s);
        match self.i.provider.as_str() {
            "github" => r
                .set("Authorization", &format!("Bearer {}", self.token))
                .set("Accept", "application/vnd.github+json")
                .set("X-GitHub-Api-Version", "2022-11-28"),
            "gitea" | "forgejo" => r.set("Authorization", &format!("token {}", self.token)),
            "gitlab" => r.set("PRIVATE-TOKEN", &self.token),
            "bitbucket" if !self.i.username.is_empty() => {
                r.set("Authorization", &format!("Basic {}", b64(format!("{}:{}", self.i.username, self.token))))
            }
            "bitbucket" => r.set("Authorization", &format!("Bearer {}", self.token)),
            "azure_devops" => r.set("Authorization", &format!("Basic {}", b64(format!(":{}", self.token)))),
            _ => r,
        }
    }

    fn send(&self, method: &str, url: &str, body: &Value) -> Result<Value> {
        match self.req(method, url).send_json(body.clone()) {
            Ok(r) => Ok(r.into_json().unwrap_or(Value::Null)),
            Err(ureq::Error::Status(code, r)) => {
                bail!("{method} {url}: HTTP {code}: {}", r.into_string().unwrap_or_default().chars().take(300).collect::<String>())
            }
            Err(e) => Err(e).with_context(|| format!("{method} {url}")),
        }
    }

    /// Download the tree at `rev` (commit SHA or branch) as a normalized `.tar.gz`.
    pub fn archive(&self, repo: &str, rev: &str) -> Result<Vec<u8>> {
        let api = self.api();
        let (url, zip) = match self.i.provider.as_str() {
            "github" => (format!("{api}/repos/{repo}/tarball/{}", enc(rev)), false),
            "gitea" | "forgejo" => (format!("{api}/repos/{repo}/archive/{}.tar.gz", enc(rev)), false),
            "gitlab" => (format!("{api}/projects/{}/repository/archive.tar.gz?sha={}", enc(repo), enc(rev)), false),
            "bitbucket" => {
                let web = if self.i.web_url.is_empty() { "https://bitbucket.org" } else { self.i.web_url.trim_end_matches('/') };
                (format!("{web}/{repo}/get/{}.tar.gz", enc(rev)), false)
            }
            "azure_devops" => {
                let (project, name) = azure_parts(repo)?;
                let vtype = if rev.len() == 40 && rev.chars().all(|c| c.is_ascii_hexdigit()) { "commit" } else { "branch" };
                (
                    format!(
                        "{api}/{}/_apis/git/repositories/{}/items?scopePath=/&recursionLevel=full&versionDescriptor.version={}&versionDescriptor.versionType={vtype}&$format=zip&download=true&api-version=7.1",
                        enc(project), enc(name), enc(rev)
                    ),
                    true,
                )
            }
            p => bail!("unknown provider {p}"),
        };
        let resp = match self.req("GET", &url).call() {
            Ok(r) => r,
            Err(ureq::Error::Status(code, _)) => bail!("downloading {repo}@{rev}: HTTP {code}"),
            Err(e) => return Err(e).with_context(|| format!("downloading {repo}@{rev}")),
        };
        let mut raw = Vec::new();
        resp.into_reader().take(MAX_ARCHIVE + 1).read_to_end(&mut raw)?;
        if raw.len() as u64 > MAX_ARCHIVE {
            bail!("repository archive is larger than 100 MiB");
        }
        if zip {
            zip_to_targz(&raw)
        } else {
            strip_top_dir(&raw)
        }
    }

    /// Set a commit status (check) for a stack.
    pub fn status(&self, repo: &str, sha: &str, state: State, context: &str, description: &str, url: &str) -> Result<()> {
        let api = self.api();
        let description: String = description.chars().take(140).collect();
        match self.i.provider.as_str() {
            "github" | "gitea" | "forgejo" => {
                let st = match state {
                    State::Pending => "pending",
                    State::Success => "success",
                    State::Failure => "failure",
                };
                self.send("POST", &format!("{api}/repos/{repo}/statuses/{sha}"),
                          &json!({"state": st, "context": context, "description": description, "target_url": url}))?;
            }
            "gitlab" => {
                let st = match state {
                    State::Pending => "running",
                    State::Success => "success",
                    State::Failure => "failed",
                };
                self.send("POST", &format!("{api}/projects/{}/statuses/{sha}", enc(repo)),
                          &json!({"state": st, "name": context, "description": description, "target_url": url}))?;
            }
            "bitbucket" => {
                let st = match state {
                    State::Pending => "INPROGRESS",
                    State::Success => "SUCCESSFUL",
                    State::Failure => "FAILED",
                };
                let key: String = context.chars().take(40).collect();
                self.send("POST", &format!("{api}/repositories/{repo}/commit/{sha}/statuses/build"),
                          &json!({"key": key, "state": st, "name": context, "description": description, "url": url}))?;
            }
            "azure_devops" => {
                let (project, name) = azure_parts(repo)?;
                let st = match state {
                    State::Pending => "pending",
                    State::Success => "succeeded",
                    State::Failure => "failed",
                };
                let (genre, cname) = context.split_once('/').unwrap_or(("cairn", context));
                self.send("POST", &format!("{api}/{}/_apis/git/repositories/{}/commits/{sha}/statuses?api-version=7.1", enc(project), enc(name)),
                          &json!({"state": st, "description": description, "targetUrl": url, "context": {"genre": genre, "name": cname}}))?;
            }
            p => bail!("unknown provider {p}"),
        }
        Ok(())
    }

    /// Create the pull request comment, or update it when `existing` is set.
    /// Returns the comment's ID.
    pub fn comment(&self, repo: &str, pr: i64, existing: Option<&str>, body: &str) -> Result<String> {
        let api = self.api();
        let id_of = |v: &Value| v["id"].as_i64().map(|i| i.to_string()).or_else(|| v["id"].as_str().map(str::to_string)).ok_or_else(|| anyhow!("no comment id in response"));
        match self.i.provider.as_str() {
            "github" | "gitea" | "forgejo" => match existing {
                Some(id) => id_of(&self.send("PATCH", &format!("{api}/repos/{repo}/issues/comments/{id}"), &json!({"body": body}))?).or(Ok(id.to_string())),
                None => id_of(&self.send("POST", &format!("{api}/repos/{repo}/issues/{pr}/comments"), &json!({"body": body}))?),
            },
            "gitlab" => {
                let base = format!("{api}/projects/{}/merge_requests/{pr}/notes", enc(repo));
                match existing {
                    Some(id) => self.send("PUT", &format!("{base}/{id}"), &json!({"body": body})).map(|_| id.to_string()),
                    None => id_of(&self.send("POST", &base, &json!({"body": body}))?),
                }
            }
            "bitbucket" => {
                let base = format!("{api}/repositories/{repo}/pullrequests/{pr}/comments");
                match existing {
                    Some(id) => self.send("PUT", &format!("{base}/{id}"), &json!({"content": {"raw": body}})).map(|_| id.to_string()),
                    None => id_of(&self.send("POST", &base, &json!({"content": {"raw": body}}))?),
                }
            }
            "azure_devops" => {
                let (project, name) = azure_parts(repo)?;
                let base = format!("{api}/{}/_apis/git/repositories/{}/pullRequests/{pr}/threads", enc(project), enc(name));
                match existing {
                    Some(id) => self
                        .send("PATCH", &format!("{base}/{id}/comments/1?api-version=7.1"), &json!({"content": body}))
                        .map(|_| id.to_string()),
                    None => id_of(&self.send("POST", &format!("{base}?api-version=7.1"),
                                             &json!({"comments": [{"parentCommentId": 0, "content": body, "commentType": 1}], "status": 1}))?),
                }
            }
            p => bail!("unknown provider {p}"),
        }
    }
}

/// Re-pack a provider tarball without its single top-level directory.
pub fn strip_top_dir(targz: &[u8]) -> Result<Vec<u8>> {
    let mut ar = tar::Archive::new(flate2::read::GzDecoder::new(targz));
    let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    let mut out = tar::Builder::new(gz);
    for entry in ar.entries().context("reading archive")? {
        let mut e = entry?;
        let path = e.path()?.into_owned();
        let mut comps = path.components();
        comps.next(); // top-level directory
        let rest = comps.as_path().to_path_buf();
        if rest.as_os_str().is_empty() {
            continue;
        }
        let kind = e.header().entry_type();
        if !(kind.is_file() || kind.is_dir() || kind.is_symlink()) {
            continue; // pax headers etc.
        }
        let mut h = e.header().clone();
        let mut data = Vec::new();
        e.read_to_end(&mut data)?;
        out.append_data(&mut h, &rest, data.as_slice())?;
    }
    Ok(out.into_inner()?.finish()?)
}

fn zip_to_targz(raw: &[u8]) -> Result<Vec<u8>> {
    let mut z = zip::ZipArchive::new(std::io::Cursor::new(raw)).context("reading zip archive")?;
    let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    let mut out = tar::Builder::new(gz);
    for i in 0..z.len() {
        let mut f = z.by_index(i)?;
        let Some(name) = f.enclosed_name() else { continue };
        let name = name.to_path_buf();
        if f.is_dir() || name.as_os_str().is_empty() {
            continue;
        }
        let mut data = Vec::new();
        f.read_to_end(&mut data)?;
        let mut h = tar::Header::new_gnu();
        h.set_size(data.len() as u64);
        h.set_mode(0o644);
        h.set_cksum();
        out.append_data(&mut h, &name, data.as_slice())?;
    }
    Ok(out.into_inner()?.finish()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers(pairs: &[(&'static str, String)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(*k, HeaderValue::from_str(v).unwrap());
        }
        h
    }

    #[test]
    fn signatures() {
        let body = br#"{"x":1}"#;
        let sig = hmac_hex("s", body);
        assert!(verify("github", "s", &headers(&[("x-hub-signature-256", format!("sha256={sig}"))]), body).is_ok());
        assert!(verify("github", "t", &headers(&[("x-hub-signature-256", format!("sha256={sig}"))]), body).is_err());
        assert!(verify("github", "s", &HeaderMap::new(), body).is_err());
        assert!(verify("forgejo", "s", &headers(&[("x-forgejo-signature", sig.clone())]), body).is_ok());
        assert!(verify("gitea", "s", &headers(&[("x-gitea-signature", sig.clone())]), body).is_ok());
        assert!(verify("bitbucket", "s", &headers(&[("x-hub-signature", format!("sha256={sig}"))]), body).is_ok());
        assert!(verify("gitlab", "s", &headers(&[("x-gitlab-token", "s".into())]), body).is_ok());
        assert!(verify("gitlab", "s", &headers(&[("x-gitlab-token", "x".into())]), body).is_err());
        let basic = base64::engine::general_purpose::STANDARD.encode("cairn:s");
        assert!(verify("azure_devops", "s", &headers(&[("authorization", format!("Basic {basic}"))]), body).is_ok());
        let basic = base64::engine::general_purpose::STANDARD.encode("cairn:x");
        assert!(verify("azure_devops", "s", &headers(&[("authorization", format!("Basic {basic}"))]), body).is_err());
    }

    #[test]
    fn github_events() {
        let push = json!({"ref": "refs/heads/main", "after": "abc", "repository": {"full_name": "o/r"},
                          "head_commit": {"message": "m"}, "pusher": {"name": "u"},
                          "commits": [{"added": ["a.tf"], "modified": ["b.tf"], "removed": []}]});
        let e = parse("github", &headers(&[("x-github-event", "push".into())]), push.to_string().as_bytes()).unwrap();
        match &e[0] {
            Event::Push { branch, files, .. } => {
                assert_eq!(branch, "main");
                assert_eq!(files.as_deref(), Some(&["a.tf".to_string(), "b.tf".to_string()][..]));
            }
            _ => panic!(),
        }
        let tag = json!({"ref": "refs/tags/v1", "after": "abc", "repository": {"full_name": "o/r"}});
        assert!(parse("github", &headers(&[("x-github-event", "push".into())]), tag.to_string().as_bytes()).unwrap().is_empty());
        let merged = json!({"action": "closed", "repository": {"full_name": "o/r"},
                            "pull_request": {"number": 7, "merged": true, "head": {"sha": "h", "ref": "f"}, "base": {"ref": "main"}}});
        let e = parse("github", &headers(&[("x-github-event", "pull_request".into())]), merged.to_string().as_bytes()).unwrap();
        assert!(matches!(&e[0], Event::PullRequest { action: PrAction::Merged, number: 7, .. }));
    }

    #[test]
    fn other_providers() {
        let mr = json!({"project": {"path_with_namespace": "g/r"}, "user": {"username": "u"},
                        "object_attributes": {"iid": 3, "action": "update", "oldrev": "o", "last_commit": {"id": "s"},
                                              "source_branch": "f", "target_branch": "main", "title": "t"}});
        let e = parse("gitlab", &headers(&[("x-gitlab-event", "Merge Request Hook".into())]), mr.to_string().as_bytes()).unwrap();
        assert!(matches!(&e[0], Event::PullRequest { action: PrAction::Updated, number: 3, .. }));
        let bb = json!({"repository": {"full_name": "w/r"}, "actor": {"display_name": "A"},
                        "push": {"changes": [{"new": {"type": "branch", "name": "main", "target": {"hash": "h", "message": "m"}}},
                                             {"new": {"type": "tag", "name": "v1", "target": {"hash": "h"}}}]}});
        assert_eq!(parse("bitbucket", &headers(&[("x-event-key", "repo:push".into())]), bb.to_string().as_bytes()).unwrap().len(), 1);
        let az = json!({"eventType": "git.pullrequest.updated", "resource": {"pullRequestId": 5, "status": "abandoned",
                        "repository": {"name": "r", "project": {"name": "p"}}, "sourceRefName": "refs/heads/f",
                        "targetRefName": "refs/heads/main", "lastMergeSourceCommit": {"commitId": "c"}}});
        let e = parse("azure_devops", &HeaderMap::new(), az.to_string().as_bytes()).unwrap();
        assert!(matches!(&e[0], Event::PullRequest { action: PrAction::Closed, ref repo, .. } if repo == "p/r"));
    }

    #[test]
    fn commands() {
        assert_eq!(parse_command("looks good\n/cairn plan net db"), Some(("plan".into(), vec!["net".into(), "db".into()])));
        assert_eq!(parse_command("cairn apply"), Some(("apply".into(), vec![])));
        assert_eq!(parse_command("/cairn destroy"), None);
        assert_eq!(parse_command("plan please"), None);
    }

    #[test]
    fn api_bases() {
        assert_eq!(api_base("forgejo", Some("https://git.lan/")).unwrap(), "https://git.lan/api/v1");
        assert_eq!(api_base("github", None).unwrap(), "https://api.github.com");
        assert!(api_base("azure_devops", None).is_err());
    }

    #[test]
    fn strips_archive_prefix() {
        let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        let mut b = tar::Builder::new(gz);
        let mut h = tar::Header::new_gnu();
        h.set_size(2);
        h.set_mode(0o644);
        h.set_cksum();
        b.append_data(&mut h, "repo-abc/dir/main.tf", &b"{}"[..]).unwrap();
        let out = strip_top_dir(&b.into_inner().unwrap().finish().unwrap()).unwrap();
        let mut ar = tar::Archive::new(flate2::read::GzDecoder::new(&out[..]));
        let names: Vec<String> = ar.entries().unwrap().map(|e| e.unwrap().path().unwrap().display().to_string()).collect();
        assert_eq!(names, vec!["dir/main.tf"]);
    }
}
