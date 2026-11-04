//! OpenTofu engine resolution.
//!
//! Order: `--tofu-bin` (pins one binary for every stack), then
//! `<data>/engines/<version>/tofu`, then a checksum-verified download of that
//! release (feature `download`), then `tofu` on PATH when no version was asked for.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use tokio::sync::Mutex;

pub struct Engines {
    dir: PathBuf,
    pinned: Option<PathBuf>,
    pub default_version: String,
    allow_download: bool,
    locks: std::sync::Mutex<HashMap<String, Arc<Mutex<()>>>>,
}

impl Engines {
    pub fn new(dir: PathBuf, pinned: Option<PathBuf>, default_version: String, allow_download: bool) -> Self {
        Engines { dir, pinned, default_version, allow_download, locks: Default::default() }
    }

    pub async fn resolve(&self, version: Option<&str>) -> Result<PathBuf> {
        if let Some(p) = &self.pinned {
            return Ok(p.clone());
        }
        let v = version.filter(|v| !v.is_empty()).unwrap_or(&self.default_version).trim_start_matches('v');
        if !v.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-') {
            bail!("invalid engine version {v:?}");
        }
        let path = self.dir.join(v).join("tofu");
        if path.exists() {
            return Ok(path);
        }
        let lock = self.locks.lock().unwrap().entry(v.to_string()).or_default().clone();
        let _g = lock.lock().await;
        if path.exists() {
            return Ok(path);
        }
        if self.allow_download {
            #[cfg(feature = "download")]
            {
                let (v2, dest) = (v.to_string(), path.clone());
                tokio::task::spawn_blocking(move || download(&v2, &dest))
                    .await?
                    .with_context(|| format!("downloading OpenTofu {v}"))?;
                return Ok(path);
            }
        }
        if version.is_none() {
            if let Some(p) = which("tofu") {
                return Ok(p);
            }
        }
        bail!("OpenTofu {v} is not installed in {} and downloads are disabled", self.dir.display())
    }
}

fn which(bin: &str) -> Option<PathBuf> {
    std::env::var_os("PATH")?
        .to_str()?
        .split(':')
        .map(|d| Path::new(d).join(bin))
        .find(|p| p.is_file())
}

#[cfg(feature = "download")]
fn download(version: &str, dest: &Path) -> Result<()> {
    use std::io::Read;
    let arch = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        "arm" => "arm",
        other => other,
    };
    let os = std::env::consts::OS;
    let file = format!("tofu_{version}_{os}_{arch}.zip");
    let base = format!("https://github.com/opentofu/opentofu/releases/download/v{version}");
    let agent = crate::http::agent(&base);

    let sums = agent.get(&format!("{base}/tofu_{version}_SHA256SUMS")).call()?.into_string()?;
    let want = sums
        .lines()
        .find_map(|l| l.strip_suffix(file.as_str()).map(|h| h.trim().to_string()))
        .with_context(|| format!("{file} not listed in SHA256SUMS"))?;

    let mut zip_bytes = Vec::new();
    agent.get(&format!("{base}/{file}")).call()?.into_reader().take(512 << 20).read_to_end(&mut zip_bytes)?;
    let got = crate::util::sha256_hex(&zip_bytes);
    if got != want {
        bail!("checksum mismatch for {file}: got {got}, want {want}");
    }
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(zip_bytes))?;
    let mut entry = zip.by_name("tofu")?;
    let dir = dest.parent().unwrap();
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(".tofu.partial");
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut out = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o755).open(&tmp)?;
        std::io::copy(&mut entry, &mut out)?;
    }
    std::fs::rename(&tmp, dest)?;
    tracing::info!(version, "installed OpenTofu");
    Ok(())
}
