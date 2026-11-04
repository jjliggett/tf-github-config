//! Outgoing notifications: webhooks for run events.
//!
//! Endpoints are configured per space and apply to stacks in that space and
//! below. Without `notification` policies every run event (planned, applied,
//! errored, …) goes to every endpoint in scope. With them, only what the
//! policies' `webhook` rule asks for is sent:
//! `webhook contains {"endpoint_id": "...", "payload": {...}}` (payload optional).
//!
//! Requests carry `X-Cairn-Event` and `X-Cairn-Signature: sha256=<hmac>` and
//! are retried with backoff. Private and loopback addresses are refused
//! unless `--allow-private-webhooks` (SSRF protection).

use std::net::{IpAddr, ToSocketAddrs};
use std::time::Duration;

use anyhow::{bail, Result};
use hmac::{Hmac, Mac};
use rusqlite::{params, Connection};
use serde::Serialize;
use serde_json::{json, Value};
use sha2::Sha256;

use crate::app::AppRef;
use crate::events::Event;
use crate::util::{new_id, now};
use crate::{policy, store};

#[derive(Debug, Clone, Serialize)]
pub struct Webhook {
    pub id: String,
    pub space_id: String,
    pub url: String,
    #[serde(skip)]
    pub secret: String,
    pub created_at: String,
}

pub fn list(c: &Connection, space_id: &str) -> Result<Vec<Webhook>> {
    let mut st = c.prepare("SELECT id, space_id, url, secret, created_at FROM webhooks WHERE space_id = ?1 AND enabled = 1")?;
    let rows = st
        .query_map([space_id], |r| Ok(Webhook { id: r.get(0)?, space_id: r.get(1)?, url: r.get(2)?, secret: r.get(3)?, created_at: r.get(4)? }))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub fn create(c: &Connection, space_id: &str, url: &str, secret: &str) -> Result<String> {
    let id = new_id("whk");
    c.execute(
        "INSERT INTO webhooks (id, space_id, url, secret, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![id, space_id, url, secret, now()],
    )?;
    Ok(id)
}

pub fn delete(c: &Connection, space_id: &str, id: &str) -> Result<bool> {
    Ok(c.execute("DELETE FROM webhooks WHERE space_id = ?1 AND id = ?2", params![space_id, id])? > 0)
}

/// Refuse URLs that resolve to private, loopback or link-local addresses.
pub fn check_url(url: &str, allow_private: bool) -> Result<()> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .ok_or_else(|| anyhow::anyhow!("webhook URLs must be http(s)"))?;
    if allow_private {
        return Ok(());
    }
    let authority = rest.split(['/', '?']).next().unwrap_or("");
    let host_port = if authority.contains(':') { authority.to_string() } else { format!("{authority}:443") };
    for addr in host_port.to_socket_addrs()? {
        let private = match addr.ip() {
            IpAddr::V4(v) => v.is_private() || v.is_loopback() || v.is_link_local() || v.is_unspecified() || v.octets()[0] == 100 && v.octets()[1] >= 64 && v.octets()[1] < 128,
            IpAddr::V6(v) => v.is_loopback() || v.is_unspecified() || (v.segments()[0] & 0xfe00) == 0xfc00 || (v.segments()[0] & 0xffc0) == 0xfe80,
        };
        if private {
            bail!("{url} resolves to a private address; start cairnd with --allow-private-webhooks to permit it");
        }
    }
    Ok(())
}

pub fn sign(secret: &str, body: &[u8]) -> String {
    let mut m = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("any key");
    m.update(body);
    format!("sha256={}", hex::encode(m.finalize().into_bytes()))
}

const NOTIFY_ON: &[&str] = &["planned", "applied", "planned_and_finished", "errored", "discarded", "canceled"];

/// Background task: turn run events into webhook deliveries.
pub async fn sender(app: AppRef) {
    let mut rx = app.hub.events.subscribe();
    loop {
        let e = match rx.recv().await {
            Ok(e) => e,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                tracing::warn!(missed = n, "notification sender lagged");
                continue;
            }
            Err(_) => return,
        };
        if e.kind != "run.status" || !e.status.as_deref().map(|s| NOTIFY_ON.contains(&s)).unwrap_or(false) {
            continue;
        }
        if let Err(err) = dispatch(&app, &e).await {
            tracing::warn!("notification for {:?} failed: {err:#}", e.run_id);
        }
    }
}

async fn dispatch(app: &AppRef, e: &Event) -> Result<()> {
    let Some(run_id) = &e.run_id else { return Ok(()) };
    let Some(stack) = app.db.read(|c| store::get_stack(c, &e.stack_id))? else { return Ok(()) };
    let chain = app.space_chain(&stack.space_id);
    let mut hooks = vec![];
    for space in &chain {
        hooks.extend(app.db.read(|c| list(c, space))?);
    }
    if hooks.is_empty() {
        return Ok(());
    }
    let run = app.db.read(|c| store::get_run(c, run_id))?;
    let default_payload = json!({
        "event": format!("run.{}", e.status.as_deref().unwrap_or("")),
        "at": e.at,
        "run": run,
        "stack": {"id": stack.id, "name": stack.name, "space": stack.space, "labels": stack.labels},
    });
    let policies = app.db.read(|c| policy::for_stack(c, "notification", &stack.id, &chain, &stack.labels))?;
    let deliveries: Vec<(Webhook, Value)> = if policies.is_empty() {
        hooks.into_iter().map(|h| (h, default_payload.clone())).collect()
    } else {
        let verdicts = app.policies.run(&policies, &default_payload)?;
        let mut out = vec![];
        for v in &verdicts {
            for w in v.set("webhook") {
                let Some(h) = hooks.iter().find(|h| Some(h.id.as_str()) == w["endpoint_id"].as_str()) else { continue };
                let payload = if w["payload"].is_null() { default_payload.clone() } else { w["payload"].clone() };
                out.push((h.clone(), payload));
            }
        }
        out
    };
    for (hook, payload) in deliveries {
        let secret = if hook.secret.is_empty() { String::new() } else { app.secrets.open(&hook.secret)? };
        let allow_private = app.cfg.allow_private_webhooks;
        let event = default_payload["event"].as_str().unwrap_or("").to_string();
        tokio::spawn(async move {
            let body = payload.to_string();
            for (attempt, wait) in [0u64, 1, 4, 16].iter().enumerate() {
                if *wait > 0 {
                    tokio::time::sleep(Duration::from_secs(*wait)).await;
                }
                let (url, body2, secret2, event2) = (hook.url.clone(), body.clone(), secret.clone(), event.clone());
                let res = tokio::task::spawn_blocking(move || -> Result<()> {
                    check_url(&url, allow_private)?;
                    let mut req = crate::http::agent(&url)
                        .post(&url)
                        .timeout(Duration::from_secs(10))
                        .set("Content-Type", "application/json")
                        .set("X-Cairn-Event", &event2);
                    if !secret2.is_empty() {
                        req = req.set("X-Cairn-Signature", &sign(&secret2, body2.as_bytes()));
                    }
                    req.send_string(&body2)?;
                    Ok(())
                })
                .await;
                match res {
                    Ok(Ok(())) => return,
                    Ok(Err(e)) => tracing::debug!(attempt, url = %hook.url, "webhook delivery failed: {e:#}"),
                    Err(e) => tracing::debug!("webhook task failed: {e}"),
                }
            }
            tracing::warn!(url = %hook.url, "webhook delivery gave up after 4 attempts");
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssrf_guard() {
        assert!(check_url("http://127.0.0.1:9/x", false).is_err());
        assert!(check_url("http://10.1.2.3/x", false).is_err());
        assert!(check_url("http://127.0.0.1:9/x", true).is_ok());
        assert!(check_url("ftp://example.com", true).is_err());
    }

    #[test]
    fn signature_format() {
        // HMAC-SHA256("key", "The quick brown fox jumps over the lazy dog")
        assert_eq!(
            sign("key", b"The quick brown fox jumps over the lazy dog"),
            "sha256=f7bc83f430538424b13298e6aa6fb143ef4d59a14946175997479dbc2d1a3cd8"
        );
    }
}
