//! Minimal blocking client for the Cairn native API.

use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use serde_json::Value;

pub struct Client {
    pub host: String,
    token: String,
    agent: ureq::Agent,
}

/// An API error with the server's problem title.
#[derive(Debug)]
pub struct ApiError {
    pub status: u16,
    pub title: String,
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{} (HTTP {})", self.title, self.status)
    }
}
impl std::error::Error for ApiError {}

impl Client {
    pub fn new(host: &str, token: &str) -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(10))
            .timeout_read(Duration::from_secs(900))
            .build();
        Client { host: host.trim_end_matches('/').to_string(), token: token.to_string(), agent }
    }

    fn req(&self, method: &str, path: &str) -> ureq::Request {
        self.agent
            .request(method, &format!("{}{}", self.host, path))
            .set("Authorization", &format!("Bearer {}", self.token))
            .set("User-Agent", concat!("cairn-cli/", env!("CARGO_PKG_VERSION")))
    }

    fn finish(r: std::result::Result<ureq::Response, ureq::Error>) -> Result<ureq::Response> {
        match r {
            Ok(r) => Ok(r),
            Err(ureq::Error::Status(code, resp)) => {
                let body = resp.into_string().unwrap_or_default();
                let title = serde_json::from_str::<Value>(&body)
                    .ok()
                    .and_then(|v| v["title"].as_str().map(String::from))
                    .unwrap_or(body);
                Err(ApiError { status: code, title }.into())
            }
            Err(e) => Err(anyhow!("cannot reach Cairn: {e}")),
        }
    }

    fn json(r: ureq::Response) -> Result<Value> {
        let s = r.into_string()?;
        if s.is_empty() {
            return Ok(Value::Null);
        }
        Ok(serde_json::from_str(&s)?)
    }

    pub fn get(&self, path: &str) -> Result<Value> {
        Self::json(Self::finish(self.req("GET", path).call())?)
    }

    pub fn get_raw(&self, path: &str) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        Self::finish(self.req("GET", path).call())?.into_reader().read_to_end(&mut out)?;
        Ok(out)
    }

    pub fn send(&self, method: &str, path: &str, body: &Value) -> Result<Value> {
        Self::json(Self::finish(self.req(method, path).send_json(body.clone()))?)
    }

    pub fn post_bytes(&self, path: &str, body: &[u8]) -> Result<Value> {
        Self::json(Self::finish(self.req("POST", path).set("Content-Type", "application/gzip").send_bytes(body))?)
    }

    pub fn delete(&self, path: &str) -> Result<Value> {
        Self::json(Self::finish(self.req("DELETE", path).call())?)
    }

    /// Stream a run's log lines (JSON) until the run rests; returns lines seen.
    pub fn follow_logs(&self, run_id: &str, offset: u64, mut on_line: impl FnMut(&Value)) -> Result<u64> {
        let r = Self::finish(self.req("GET", &format!("/api/v1/runs/{run_id}/logs?follow=true&offset={offset}")).call())?;
        let mut n = offset;
        for line in BufReader::new(r.into_reader()).lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            n += 1;
            if let Ok(v) = serde_json::from_str::<Value>(&line) {
                on_line(&v);
            }
        }
        Ok(n)
    }

    /// Long-poll until the run is resting (finished or waiting for confirmation).
    pub fn wait_resting(&self, run_id: &str, timeout: Duration) -> Result<Value> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let r = self.get(&format!("/api/v1/runs/{run_id}?wait=30"))?;
            if is_resting(r["status"].as_str().unwrap_or("")) || std::time::Instant::now() > deadline {
                return Ok(r);
            }
        }
    }
}

pub fn is_resting(status: &str) -> bool {
    matches!(status, "applied" | "planned_and_finished" | "discarded" | "errored" | "canceled" | "planned")
}

/// Pack a directory as .tar.gz, skipping VCS metadata, provider caches and local state.
pub fn pack_dir(dir: &Path) -> Result<Vec<u8>> {
    let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    let mut tar = tar::Builder::new(gz);
    tar.follow_symlinks(false);
    add_dir(&mut tar, dir, dir)?;
    Ok(tar.into_inner()?.finish()?)
}

fn add_dir<W: std::io::Write>(tar: &mut tar::Builder<W>, root: &Path, dir: &Path) -> Result<()> {
    let mut entries: Vec<_> = std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))?.collect::<std::io::Result<_>>()?;
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let name = e.file_name().to_string_lossy().to_string();
        if matches!(name.as_str(), ".git" | ".terraform" | ".cairn") || name.ends_with(".tfstate") || name.ends_with(".tfstate.backup") {
            continue;
        }
        let path = e.path();
        let rel = path.strip_prefix(root)?;
        let ft = e.file_type()?;
        if ft.is_dir() {
            add_dir(tar, root, &path)?;
        } else if ft.is_file() {
            tar.append_path_with_name(&path, rel)?;
        }
    }
    Ok(())
}
