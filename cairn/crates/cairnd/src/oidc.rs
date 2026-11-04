//! OpenID Connect: verify IdP-issued JWTs and run the login flow.
//!
//! * Bearer ID tokens (e.g. from CI) are verified against the issuer's JWKS
//!   (RS256/ES256/EdDSA), plus `iss`, `aud` and `exp`.
//! * Browser and CLI login: authorization code flow with PKCE. The callback
//!   mints a short-lived Cairn user token. For the CLI, the token is handed to
//!   a loopback redirect (`http://127.0.0.1:<port>/…`), like `tofu login`.
//!
//! Discovery and keys are fetched lazily and cached; an unknown `kid`
//! triggers at most one JWKS refresh per minute.

use std::collections::HashMap;
use std::sync::{Mutex, RwLock};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine as _;
use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::util::random_bytes;

#[derive(Clone, Debug)]
pub struct OidcConfig {
    pub issuer: String,
    pub client_id: String,
    pub client_secret: Option<String>,
    pub groups_claim: String,
    pub admin_group: Option<String>,
    pub scopes: String,
    /// Extra accepted `aud` values for bearer tokens (the client ID is always accepted).
    pub audiences: Vec<String>,
}

#[derive(Deserialize, Clone)]
struct Discovery {
    authorization_endpoint: String,
    token_endpoint: String,
    jwks_uri: String,
}

struct Keys {
    set: HashMap<String, (DecodingKey, Algorithm)>,
    fetched: Instant,
}

struct Pending {
    verifier: String,
    redirect: Option<String>,
    created: Instant,
}

pub struct Oidc {
    pub cfg: OidcConfig,
    discovery: RwLock<Option<Discovery>>,
    keys: RwLock<Option<Keys>>,
    pending: Mutex<HashMap<String, Pending>>,
}

/// Identity extracted from a verified ID token.
#[derive(Debug, Clone)]
pub struct Identity {
    pub subject: String,
    pub email: String,
    pub name: String,
    pub groups: Vec<String>,
    pub site_admin: bool,
    /// `exp` claim (seconds since the epoch; 0 if absent).
    pub exp: u64,
}

pub fn b64url(data: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(data)
}

impl Oidc {
    pub fn new(cfg: OidcConfig) -> Self {
        Oidc { cfg, discovery: RwLock::new(None), keys: RwLock::new(None), pending: Mutex::new(HashMap::new()) }
    }

    fn discovery(&self) -> Result<Discovery> {
        if let Some(d) = self.discovery.read().unwrap().clone() {
            return Ok(d);
        }
        let url = format!("{}/.well-known/openid-configuration", self.cfg.issuer.trim_end_matches('/'));
        let d: Discovery = crate::http::agent(&url).get(&url).call().with_context(|| format!("OIDC discovery at {url}"))?.into_json()?;
        *self.discovery.write().unwrap() = Some(d.clone());
        Ok(d)
    }

    fn refresh_keys(&self) -> Result<()> {
        if let Some(k) = self.keys.read().unwrap().as_ref() {
            if k.fetched.elapsed() < Duration::from_secs(60) {
                return Ok(());
            }
        }
        let d = self.discovery()?;
        let set: JwkSet = crate::http::agent(&d.jwks_uri).get(&d.jwks_uri).call()?.into_json()?;
        let mut map = HashMap::new();
        for jwk in &set.keys {
            let kid = jwk.common.key_id.clone().unwrap_or_default();
            let alg = jwk
                .common
                .key_algorithm
                .and_then(|a| a.to_string().parse::<Algorithm>().ok())
                .unwrap_or(match &jwk.algorithm {
                    jsonwebtoken::jwk::AlgorithmParameters::EllipticCurve(_) => Algorithm::ES256,
                    jsonwebtoken::jwk::AlgorithmParameters::OctetKeyPair(_) => Algorithm::EdDSA,
                    _ => Algorithm::RS256,
                });
            if let Ok(key) = DecodingKey::from_jwk(jwk) {
                map.insert(kid, (key, alg));
            }
        }
        *self.keys.write().unwrap() = Some(Keys { set: map, fetched: Instant::now() });
        Ok(())
    }

    /// Verify a JWT from the issuer (blocking: call from spawn_blocking).
    pub fn verify(&self, token: &str) -> Result<Identity> {
        let header = jsonwebtoken::decode_header(token)?;
        let kid = header.kid.clone().unwrap_or_default();
        let lookup = |keys: &RwLock<Option<Keys>>| {
            keys.read().unwrap().as_ref().and_then(|k| {
                k.set.get(&kid).or_else(|| if kid.is_empty() { k.set.values().next() } else { None }).map(|(key, alg)| (key.clone(), *alg))
            })
        };
        let (key, alg) = match lookup(&self.keys) {
            Some(k) => k,
            None => {
                self.refresh_keys()?;
                lookup(&self.keys).ok_or_else(|| anyhow!("unknown signing key {kid:?}"))?
            }
        };
        if alg != header.alg {
            bail!("token algorithm {:?} does not match key {:?}", header.alg, alg);
        }
        let mut v = Validation::new(alg);
        v.set_issuer(&[self.cfg.issuer.trim_end_matches('/'), self.cfg.issuer.as_str()]);
        let mut aud = vec![self.cfg.client_id.clone()];
        aud.extend(self.cfg.audiences.iter().cloned());
        v.set_audience(&aud);
        v.leeway = 30;
        let data = jsonwebtoken::decode::<Value>(token, &key, &v)?;
        Ok(self.identity(&data.claims))
    }

    fn identity(&self, c: &Value) -> Identity {
        let groups = match &c[&self.cfg.groups_claim] {
            Value::Array(a) => a.iter().filter_map(|g| g.as_str().map(String::from)).collect(),
            Value::String(s) => s.split([',', ' ']).filter(|s| !s.is_empty()).map(String::from).collect(),
            _ => vec![],
        };
        let site_admin = self.cfg.admin_group.as_ref().map(|a| groups.iter().any(|g: &String| g == a)).unwrap_or(false);
        Identity {
            subject: format!("{}#{}", self.cfg.issuer.trim_end_matches('/'), c["sub"].as_str().unwrap_or_default()),
            email: c["email"].as_str().unwrap_or_default().to_string(),
            name: c["name"]
                .as_str()
                .or_else(|| c["preferred_username"].as_str())
                .unwrap_or_default()
                .to_string(),
            groups,
            site_admin,
            exp: c["exp"].as_u64().unwrap_or(0),
        }
    }

    /// Start a login: returns the IdP authorization URL.
    pub fn start(&self, callback_url: &str, redirect: Option<String>) -> Result<String> {
        let d = self.discovery()?;
        let mut raw = [0u8; 32];
        random_bytes(&mut raw);
        let verifier = b64url(&raw);
        random_bytes(&mut raw);
        let state = b64url(&raw[..16]);
        let challenge = b64url(&Sha256::digest(verifier.as_bytes()));
        {
            let mut p = self.pending.lock().unwrap();
            p.retain(|_, v| v.created.elapsed() < Duration::from_secs(600));
            p.insert(state.clone(), Pending { verifier, redirect, created: Instant::now() });
        }
        let q = [
            ("response_type", "code"),
            ("client_id", self.cfg.client_id.as_str()),
            ("redirect_uri", callback_url),
            ("scope", self.cfg.scopes.as_str()),
            ("state", state.as_str()),
            ("code_challenge", challenge.as_str()),
            ("code_challenge_method", "S256"),
        ];
        let qs: Vec<String> = q.iter().map(|(k, v)| format!("{k}={}", urlencode(v))).collect();
        let sep = if d.authorization_endpoint.contains('?') { '&' } else { '?' };
        Ok(format!("{}{sep}{}", d.authorization_endpoint, qs.join("&")))
    }

    /// Finish a login: exchange the code, verify the ID token.
    pub fn finish(&self, code: &str, state: &str, callback_url: &str) -> Result<(Identity, Option<String>)> {
        let p = self.pending.lock().unwrap().remove(state).ok_or_else(|| anyhow!("unknown or expired login state"))?;
        let d = self.discovery()?;
        let mut form = vec![
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", callback_url),
            ("client_id", self.cfg.client_id.as_str()),
            ("code_verifier", p.verifier.as_str()),
        ];
        if let Some(s) = &self.cfg.client_secret {
            form.push(("client_secret", s.as_str()));
        }
        let resp: Value = match crate::http::agent(&d.token_endpoint).post(&d.token_endpoint).send_form(&form) {
            Ok(r) => r.into_json()?,
            Err(ureq::Error::Status(c, r)) => bail!("token endpoint returned {c}: {}", r.into_string().unwrap_or_default()),
            Err(e) => return Err(e.into()),
        };
        let id_token = resp["id_token"].as_str().ok_or_else(|| anyhow!("no id_token in token response"))?;
        Ok((self.verify(id_token)?, p.redirect))
    }
}

pub fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") })
        .collect()
}

/// Only loopback redirects are allowed for CLI logins (no open redirect).
pub fn is_loopback_redirect(url: &str) -> bool {
    ["http://127.0.0.1:", "http://localhost:", "http://[::1]:"].iter().any(|p| url.starts_with(p))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_only() {
        assert!(is_loopback_redirect("http://127.0.0.1:5555/cb"));
        assert!(!is_loopback_redirect("https://evil.example/cb"));
        assert!(!is_loopback_redirect("http://127.0.0.1.evil.example/cb"));
    }

    #[test]
    fn pkce_challenge_matches_rfc7636_example() {
        // RFC 7636 appendix B
        let v = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(b64url(&Sha256::digest(v.as_bytes())), "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }
}
