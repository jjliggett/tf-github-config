//! Blob storage for state versions, configuration tarballs and registry
//! artifacts.
//!
//! Backends (`--blob-store`):
//! * `file:///path` (default `<data>/blobs`)
//! * `s3://bucket/prefix?region=…&endpoint=…&path_style=true`: AWS S3 and
//!   anything S3-compatible (MinIO, Cloudflare R2, Garage, Ceph RGW,
//!   Backblaze B2, and GCS through its interoperability API). Credentials come
//!   from `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY` and optionally
//!   `AWS_SESSION_TOKEN`. Signing is AWS Signature Version 4.
//! * `azblob://account/container/prefix?endpoint=…`: Azure Blob Storage with
//!   Shared Key auth (`AZURE_STORAGE_KEY`); `endpoint` allows Azurite.
//!
//! State and configuration blobs are encrypted at rest with the server's
//! secret key unless `--no-blob-encryption` (see [`crate::crypto`]).

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine as _;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

#[derive(Clone)]
pub enum Backend {
    Fs(PathBuf),
    S3(Arc<S3>),
    Azure(Arc<AzureBlob>),
}

pub struct BlobStore {
    pub backend: Backend,
    seal: Option<Arc<crate::crypto::SecretBox>>,
}

const MAGIC: &[u8] = b"CAIRNENC1";

impl BlobStore {
    pub fn open(spec: Option<&str>, default_dir: PathBuf, seal: Option<Arc<crate::crypto::SecretBox>>) -> Result<Self> {
        let backend = match spec {
            None => Backend::Fs(default_dir),
            Some(s) if s.starts_with("file://") => Backend::Fs(PathBuf::from(&s[7..])),
            Some(s) if s.starts_with("s3://") => Backend::S3(Arc::new(S3::from_url(s)?)),
            Some(s) if s.starts_with("azblob://") => Backend::Azure(Arc::new(AzureBlob::from_url(s)?)),
            Some(s) => bail!("unsupported blob store {s:?} (use file://, s3:// or azblob://)"),
        };
        if let Backend::Fs(d) = &backend {
            std::fs::create_dir_all(d)?;
        }
        Ok(BlobStore { backend, seal })
    }

    pub fn describe(&self) -> String {
        match &self.backend {
            Backend::Fs(d) => format!("file://{}", d.display()),
            Backend::S3(s) => format!("s3://{}/{}", s.bucket, s.prefix),
            Backend::Azure(a) => format!("azblob://{}/{}/{}", a.account, a.container, a.prefix),
        }
    }

    /// Store `data` under `key`; `secret` encrypts it at rest (when enabled).
    pub async fn put(&self, key: &str, data: Vec<u8>, secret: bool) -> Result<()> {
        let data = match (&self.seal, secret) {
            (Some(b), true) => {
                let mut out = MAGIC.to_vec();
                out.extend(b.seal_bytes(&data));
                out
            }
            _ => data,
        };
        match &self.backend {
            Backend::Fs(dir) => {
                let path = dir.join(key);
                if let Some(p) = path.parent() {
                    tokio::fs::create_dir_all(p).await?;
                }
                let tmp = path.with_extension("partial");
                tokio::fs::write(&tmp, &data).await?;
                tokio::fs::rename(&tmp, &path).await?;
                Ok(())
            }
            Backend::S3(s) => {
                let (s, k) = (s.clone(), key.to_string());
                tokio::task::spawn_blocking(move || s.put(&k, &data)).await?
            }
            Backend::Azure(a) => {
                let (a, k) = (a.clone(), key.to_string());
                tokio::task::spawn_blocking(move || a.put(&k, &data)).await?
            }
        }
    }

    pub async fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let raw = match &self.backend {
            Backend::Fs(dir) => match tokio::fs::read(dir.join(key)).await {
                Ok(b) => Some(b),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                Err(e) => return Err(e.into()),
            },
            Backend::S3(s) => {
                let (s, k) = (s.clone(), key.to_string());
                tokio::task::spawn_blocking(move || s.get(&k)).await??
            }
            Backend::Azure(a) => {
                let (a, k) = (a.clone(), key.to_string());
                tokio::task::spawn_blocking(move || a.get(&k)).await??
            }
        };
        match raw {
            Some(b) if b.starts_with(MAGIC) => {
                let seal = self.seal.as_ref().ok_or_else(|| anyhow!("blob {key} is encrypted but no key is configured"))?;
                Ok(Some(seal.open_bytes(&b[MAGIC.len()..])?))
            }
            other => Ok(other),
        }
    }

    pub async fn exists(&self, key: &str) -> Result<bool> {
        match &self.backend {
            Backend::Fs(dir) => Ok(tokio::fs::try_exists(dir.join(key)).await?),
            Backend::S3(s) => {
                let (s, k) = (s.clone(), key.to_string());
                tokio::task::spawn_blocking(move || s.head(&k)).await?
            }
            Backend::Azure(a) => {
                let (a, k) = (a.clone(), key.to_string());
                tokio::task::spawn_blocking(move || a.head(&k)).await?
            }
        }
    }

    pub async fn delete(&self, key: &str) -> Result<()> {
        match &self.backend {
            Backend::Fs(dir) => match tokio::fs::remove_file(dir.join(key)).await {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
                _ => Ok(()),
            },
            Backend::S3(s) => {
                let (s, k) = (s.clone(), key.to_string());
                tokio::task::spawn_blocking(move || s.delete(&k)).await?
            }
            Backend::Azure(a) => {
                let (a, k) = (a.clone(), key.to_string());
                tokio::task::spawn_blocking(move || a.delete(&k)).await?
            }
        }
    }
}

fn query_param<'a>(q: &'a str, name: &str) -> Option<&'a str> {
    q.split('&').find_map(|kv| kv.split_once('=').filter(|(k, _)| *k == name).map(|(_, v)| v))
}

/// Percent-encode per RFC 3986 (AWS "UriEncode"); `/` kept when `path`.
pub fn uri_encode(s: &str, path: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) || (path && b == b'/') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut m = HmacSha256::new_from_slice(key).expect("any key length");
    m.update(data);
    m.finalize().into_bytes().to_vec()
}

fn status_err(method: &str, url: &str, e: ureq::Error) -> anyhow::Error {
    match e {
        ureq::Error::Status(c, r) => anyhow!("{method} {url}: HTTP {c}: {}", r.into_string().unwrap_or_default().chars().take(300).collect::<String>()),
        e => anyhow!("{method} {url}: {e}"),
    }
}

// ---------------------------------------------------------------- S3 (SigV4)

pub struct S3 {
    pub bucket: String,
    pub prefix: String,
    region: String,
    endpoint: String,
    path_style: bool,
    access_key: String,
    secret_key: String,
    session_token: Option<String>,
}

impl S3 {
    pub fn from_url(url: &str) -> Result<Self> {
        let rest = &url["s3://".len()..];
        let (path, query) = rest.split_once('?').unwrap_or((rest, ""));
        let (bucket, prefix) = path.split_once('/').unwrap_or((path, ""));
        let region = query_param(query, "region").map(String::from)
            .or_else(|| std::env::var("AWS_REGION").ok())
            .unwrap_or_else(|| "us-east-1".into());
        let endpoint = query_param(query, "endpoint")
            .map(|e| e.replace("%3A", ":").replace("%2F", "/"))
            .unwrap_or_else(|| format!("https://s3.{region}.amazonaws.com"));
        let path_style = query_param(query, "path_style").map(|v| v == "true").unwrap_or(query_param(query, "endpoint").is_some());
        Ok(S3 {
            bucket: bucket.to_string(),
            prefix: prefix.trim_matches('/').to_string(),
            region,
            endpoint: endpoint.trim_end_matches('/').to_string(),
            path_style,
            access_key: std::env::var("AWS_ACCESS_KEY_ID").context("AWS_ACCESS_KEY_ID is required for s3://")?,
            secret_key: std::env::var("AWS_SECRET_ACCESS_KEY").context("AWS_SECRET_ACCESS_KEY is required for s3://")?,
            session_token: std::env::var("AWS_SESSION_TOKEN").ok().filter(|s| !s.is_empty()),
        })
    }

    fn object(&self, key: &str) -> (String, String, String) {
        let full = if self.prefix.is_empty() { key.to_string() } else { format!("{}/{key}", self.prefix) };
        let (scheme, host) = self.endpoint.split_once("://").unwrap_or(("https", &self.endpoint));
        if self.path_style {
            let path = format!("/{}/{}", self.bucket, uri_encode(&full, true));
            (format!("{scheme}://{host}{path}"), host.to_string(), path)
        } else {
            let host = format!("{}.{host}", self.bucket);
            let path = format!("/{}", uri_encode(&full, true));
            (format!("{scheme}://{host}{path}"), host, path)
        }
    }

    fn request(&self, method: &str, key: &str, body: &[u8]) -> (ureq::Request, String) {
        let (url, host, path) = self.object(key);
        let amz_date = crate::util::amz_date();
        let payload_hash = hex::encode(Sha256::digest(body));
        let mut headers = vec![
            ("host".to_string(), host),
            ("x-amz-content-sha256".to_string(), payload_hash.clone()),
            ("x-amz-date".to_string(), amz_date.clone()),
        ];
        if let Some(t) = &self.session_token {
            headers.push(("x-amz-security-token".to_string(), t.clone()));
        }
        let auth = sigv4(&self.access_key, &self.secret_key, &self.region, "s3", method, &path, "", &mut headers, &payload_hash, &amz_date);
        let mut req = crate::http::agent(&url).request(method, &url).set("Authorization", &auth);
        for (k, v) in &headers {
            if k != "host" {
                req = req.set(k, v);
            }
        }
        (req, url)
    }

    pub fn put(&self, key: &str, data: &[u8]) -> Result<()> {
        let (req, url) = self.request("PUT", key, data);
        req.send_bytes(data).map_err(|e| status_err("PUT", &url, e))?;
        Ok(())
    }

    pub fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let (req, url) = self.request("GET", key, b"");
        match req.call() {
            Ok(r) => {
                let mut out = Vec::new();
                std::io::Read::read_to_end(&mut r.into_reader(), &mut out)?;
                Ok(Some(out))
            }
            Err(ureq::Error::Status(404, _)) => Ok(None),
            Err(e) => Err(status_err("GET", &url, e)),
        }
    }

    pub fn head(&self, key: &str) -> Result<bool> {
        let (req, url) = self.request("HEAD", key, b"");
        match req.call() {
            Ok(_) => Ok(true),
            Err(ureq::Error::Status(404, _)) => Ok(false),
            Err(e) => Err(status_err("HEAD", &url, e)),
        }
    }

    pub fn delete(&self, key: &str) -> Result<()> {
        let (req, url) = self.request("DELETE", key, b"");
        match req.call() {
            Ok(_) | Err(ureq::Error::Status(404, _)) => Ok(()),
            Err(e) => Err(status_err("DELETE", &url, e)),
        }
    }
}

/// AWS Signature Version 4. `headers` must include `host`; they are
/// lowercased and sorted in place. Returns the `Authorization` value.
#[allow(clippy::too_many_arguments)]
pub fn sigv4(
    access: &str,
    secret: &str,
    region: &str,
    service: &str,
    method: &str,
    path: &str,
    query: &str,
    headers: &mut [(String, String)],
    payload_hash: &str,
    amz_date: &str,
) -> String {
    for h in headers.iter_mut() {
        h.0 = h.0.to_ascii_lowercase();
        h.1 = h.1.trim().to_string();
    }
    headers.sort();
    let canonical_headers: String = headers.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();
    let signed: Vec<&str> = headers.iter().map(|(k, _)| k.as_str()).collect();
    let signed = signed.join(";");
    let canonical = format!("{method}\n{path}\n{query}\n{canonical_headers}\n{signed}\n{payload_hash}");
    let date = &amz_date[..8];
    let scope = format!("{date}/{region}/{service}/aws4_request");
    let to_sign = format!("AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}", hex::encode(Sha256::digest(canonical.as_bytes())));
    let k = hmac(format!("AWS4{secret}").as_bytes(), date.as_bytes());
    let k = hmac(&k, region.as_bytes());
    let k = hmac(&k, service.as_bytes());
    let k = hmac(&k, b"aws4_request");
    let sig = hex::encode(hmac(&k, to_sign.as_bytes()));
    format!("AWS4-HMAC-SHA256 Credential={access}/{scope}, SignedHeaders={signed}, Signature={sig}")
}

// ---------------------------------------------------------------- Azure Blob (Shared Key)

pub struct AzureBlob {
    pub account: String,
    pub container: String,
    pub prefix: String,
    endpoint: String,
    key: Vec<u8>,
}

const AZ_VERSION: &str = "2021-08-06";

impl AzureBlob {
    pub fn from_url(url: &str) -> Result<Self> {
        let rest = &url["azblob://".len()..];
        let (path, query) = rest.split_once('?').unwrap_or((rest, ""));
        let mut parts = path.splitn(3, '/');
        let account = parts.next().filter(|s| !s.is_empty()).context("azblob://ACCOUNT/CONTAINER[/prefix]")?.to_string();
        let container = parts.next().filter(|s| !s.is_empty()).context("azblob://ACCOUNT/CONTAINER[/prefix]")?.to_string();
        let prefix = parts.next().unwrap_or("").trim_matches('/').to_string();
        let endpoint = query_param(query, "endpoint")
            .map(|e| e.replace("%3A", ":").replace("%2F", "/"))
            .unwrap_or_else(|| format!("https://{account}.blob.core.windows.net"));
        let key = std::env::var("AZURE_STORAGE_KEY").context("AZURE_STORAGE_KEY is required for azblob://")?;
        let key = base64::engine::general_purpose::STANDARD.decode(key.trim()).context("AZURE_STORAGE_KEY must be base64")?;
        Ok(AzureBlob { account, container, prefix, endpoint: endpoint.trim_end_matches('/').to_string(), key })
    }

    fn request(&self, method: &str, key: &str, len: usize, extra: &[(&str, &str)]) -> (ureq::Request, String) {
        let full = if self.prefix.is_empty() { key.to_string() } else { format!("{}/{key}", self.prefix) };
        let blob_path = format!("/{}/{}", self.container, uri_encode(&full, true));
        let url = format!("{}{blob_path}", self.endpoint);
        let date = httpdate_now();
        let mut ms: Vec<(String, String)> = vec![("x-ms-date".into(), date), ("x-ms-version".into(), AZ_VERSION.into())];
        for (k, v) in extra {
            ms.push((k.to_string(), v.to_string()));
        }
        // Azurite / custom endpoints put the account in the path: http://host/devstoreaccount1/container/blob
        let resource_path = match self.endpoint.split_once("://").map(|(_, h)| h).and_then(|h| h.split_once('/')) {
            Some((_, acct_path)) => format!("/{}/{acct_path}{blob_path}", self.account),
            None => format!("/{}{blob_path}", self.account),
        };
        let content_type = if method == "PUT" { "application/octet-stream" } else { "" };
        let sig = azure_shared_key(&self.key, method, len, content_type, &mut ms, &resource_path);
        let mut req = crate::http::agent(&url)
            .request(method, &url)
            .set("Authorization", &format!("SharedKey {}:{sig}", self.account));
        if !content_type.is_empty() {
            req = req.set("Content-Type", content_type);
        }
        for (k, v) in &ms {
            req = req.set(k, v);
        }
        (req, url)
    }

    pub fn put(&self, key: &str, data: &[u8]) -> Result<()> {
        let (req, url) = self.request("PUT", key, data.len(), &[("x-ms-blob-type", "BlockBlob")]);
        req.send_bytes(data).map_err(|e| status_err("PUT", &url, e))?;
        Ok(())
    }

    pub fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let (req, url) = self.request("GET", key, 0, &[]);
        match req.call() {
            Ok(r) => {
                let mut out = Vec::new();
                std::io::Read::read_to_end(&mut r.into_reader(), &mut out)?;
                Ok(Some(out))
            }
            Err(ureq::Error::Status(404, _)) => Ok(None),
            Err(e) => Err(status_err("GET", &url, e)),
        }
    }

    pub fn head(&self, key: &str) -> Result<bool> {
        let (req, url) = self.request("HEAD", key, 0, &[]);
        match req.call() {
            Ok(_) => Ok(true),
            Err(ureq::Error::Status(404, _)) => Ok(false),
            Err(e) => Err(status_err("HEAD", &url, e)),
        }
    }

    pub fn delete(&self, key: &str) -> Result<()> {
        let (req, url) = self.request("DELETE", key, 0, &[]);
        match req.call() {
            Ok(_) | Err(ureq::Error::Status(404, _)) => Ok(()),
            Err(e) => Err(status_err("DELETE", &url, e)),
        }
    }
}

/// Azure Storage Shared Key signature (blob service, version 2015-02-21+).
pub fn azure_shared_key(key: &[u8], method: &str, len: usize, content_type: &str, ms: &mut [(String, String)], resource: &str) -> String {
    for h in ms.iter_mut() {
        h.0 = h.0.to_ascii_lowercase();
    }
    ms.sort();
    let canonical_headers: String = ms.iter().map(|(k, v)| format!("{k}:{}\n", v.trim())).collect();
    let length = if len == 0 { String::new() } else { len.to_string() };
    // VERB, Content-Encoding, Content-Language, Content-Length, Content-MD5, Content-Type, Date,
    // If-Modified-Since, If-Match, If-None-Match, If-Unmodified-Since, Range
    let to_sign = format!("{method}\n\n\n{length}\n\n{content_type}\n\n\n\n\n\n\n{canonical_headers}{resource}");
    base64::engine::general_purpose::STANDARD.encode(hmac(key, to_sign.as_bytes()))
}

fn httpdate_now() -> String {
    let t = time::OffsetDateTime::now_utc();
    const DAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    format!(
        "{}, {:02} {} {} {:02}:{:02}:{:02} GMT",
        DAYS[t.weekday().number_days_from_monday() as usize],
        t.day(),
        MONTHS[t.month() as usize - 1],
        t.year(),
        t.hour(),
        t.minute(),
        t.second()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The "GET Object" example from the AWS SigV4 documentation.
    #[test]
    fn sigv4_matches_aws_documentation_example() {
        let empty = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        let mut headers = vec![
            ("Host".into(), "examplebucket.s3.amazonaws.com".into()),
            ("Range".into(), "bytes=0-9".into()),
            ("x-amz-content-sha256".into(), empty.into()),
            ("x-amz-date".into(), "20130524T000000Z".into()),
        ];
        let auth = sigv4(
            "AKIAIOSFODNN7EXAMPLE",
            "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
            "us-east-1",
            "s3",
            "GET",
            "/test.txt",
            "",
            &mut headers,
            empty,
            "20130524T000000Z",
        );
        assert!(auth.ends_with("Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"), "{auth}");
        assert!(auth.contains("SignedHeaders=host;range;x-amz-content-sha256;x-amz-date"));
    }

    #[test]
    fn uri_encoding() {
        assert_eq!(uri_encode("state/a b+c.json", true), "state/a%20b%2Bc.json");
        assert_eq!(uri_encode("a/b", false), "a%2Fb");
    }

    #[test]
    fn s3_urls() {
        std::env::set_var("AWS_ACCESS_KEY_ID", "x");
        std::env::set_var("AWS_SECRET_ACCESS_KEY", "y");
        let s = S3::from_url("s3://bkt/pre/fix?region=eu-west-1").unwrap();
        assert_eq!(s.object("k.json").0, "https://bkt.s3.eu-west-1.amazonaws.com/pre/fix/k.json");
        let m = S3::from_url("s3://bkt?endpoint=http://127.0.0.1:9000").unwrap();
        assert_eq!(m.object("k").0, "http://127.0.0.1:9000/bkt/k");
    }
}
