//! Outbound HTTP (IdP, VCS APIs, object storage, engine downloads).
//!
//! ureq's `try_proxy_from_env` sends every request through the proxy and
//! ignores `NO_PROXY`, which breaks talking to local services. This picks a
//! direct or proxied agent per URL, honouring `NO_PROXY`/`no_proxy`
//! (exact hosts, `.suffix` / `*.suffix` domains, `*`, and IPv4 CIDRs).

use std::sync::OnceLock;
use std::time::Duration;

struct Agents {
    direct: ureq::Agent,
    proxied: Option<ureq::Agent>,
    no_proxy: Vec<String>,
}

fn agents() -> &'static Agents {
    static A: OnceLock<Agents> = OnceLock::new();
    A.get_or_init(|| {
        let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        let build = || ureq::AgentBuilder::new().timeout_connect(Duration::from_secs(10)).timeout(Duration::from_secs(300));
        let proxy = env("HTTPS_PROXY").or_else(|| env("https_proxy")).or_else(|| env("HTTP_PROXY")).or_else(|| env("http_proxy"));
        let proxied = proxy.and_then(|p| ureq::Proxy::new(p).ok()).map(|p| build().proxy(p).build());
        let no_proxy = env("NO_PROXY")
            .or_else(|| env("no_proxy"))
            .unwrap_or_default()
            .split(',')
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| !s.is_empty())
            .collect();
        Agents { direct: build().build(), proxied, no_proxy }
    })
}

/// The agent to use for `url`.
pub fn agent(url: &str) -> &'static ureq::Agent {
    let a = agents();
    match &a.proxied {
        Some(p) if !bypass(&a.no_proxy, host_of(url)) => p,
        _ => &a.direct,
    }
}

fn host_of(url: &str) -> &str {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let authority = authority.rsplit_once('@').map(|(_, h)| h).unwrap_or(authority);
    if let Some(v6) = authority.strip_prefix('[') {
        return v6.split(']').next().unwrap_or("");
    }
    authority.split(':').next().unwrap_or("")
}

fn bypass(rules: &[String], host: &str) -> bool {
    let host = host.to_ascii_lowercase();
    rules.iter().any(|r| {
        if r == "*" {
            return true;
        }
        if let Some((net, bits)) = r.split_once('/') {
            return in_cidr(&host, net, bits);
        }
        let suffix = r.trim_start_matches('*');
        if let Some(dom) = suffix.strip_prefix('.') {
            return host == dom || host.ends_with(&format!(".{dom}"));
        }
        host == *r
    })
}

fn in_cidr(host: &str, net: &str, bits: &str) -> bool {
    let (Ok(h), Ok(n), Ok(b)) = (host.parse::<std::net::Ipv4Addr>(), net.parse::<std::net::Ipv4Addr>(), bits.parse::<u32>()) else {
        return false;
    };
    if b > 32 {
        return false;
    }
    let mask = if b == 0 { 0 } else { u32::MAX << (32 - b) };
    (u32::from(h) & mask) == (u32::from(n) & mask)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hosts() {
        assert_eq!(host_of("http://127.0.0.1:5556/dex"), "127.0.0.1");
        assert_eq!(host_of("https://user:pw@git.example.com/x"), "git.example.com");
        assert_eq!(host_of("http://[::1]:80/"), "::1");
    }

    #[test]
    fn no_proxy_rules() {
        let r: Vec<String> = ["localhost", ".corp.example", "*.svc.local", "10.0.0.0/8"].iter().map(|s| s.to_string()).collect();
        assert!(bypass(&r, "localhost"));
        assert!(bypass(&r, "git.corp.example"));
        assert!(bypass(&r, "corp.example"));
        assert!(bypass(&r, "a.svc.local"));
        assert!(bypass(&r, "10.1.2.3"));
        assert!(!bypass(&r, "github.com"));
        assert!(!bypass(&r, "11.0.0.1"));
        assert!(bypass(&["*".to_string()], "anything"));
    }
}
