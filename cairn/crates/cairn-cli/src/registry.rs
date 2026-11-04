//! `cairn module …` and `cairn provider …`: the private registry.

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use base64::Engine as _;
use clap::Subcommand;
use serde_json::json;

use crate::client::{pack_dir, Client};
use crate::Out;

#[derive(Subcommand)]
pub enum ModuleCmd {
    List,
    /// Upload a directory as a module version: `cairn module publish team/vpc/aws 1.2.0 --dir .`
    Publish {
        /// NAMESPACE/NAME/SYSTEM (namespace = space)
        address: String,
        version: String,
        #[arg(long, default_value = ".")]
        dir: PathBuf,
    },
    /// Delete a module, or one version with --version.
    Delete {
        address: String,
        #[arg(long)]
        version: Option<String>,
    },
}

#[derive(Subcommand)]
pub enum ProviderCmd {
    List,
    /// Publish a release directory (goreleaser layout): the platform zips,
    /// `*_SHA256SUMS` and `*_SHA256SUMS.sig`.
    Publish {
        /// NAMESPACE/TYPE
        address: String,
        version: String,
        #[arg(long, default_value = "dist")]
        dir: PathBuf,
        /// ASCII-armored public key that signed SHA256SUMS.
        #[arg(long)]
        gpg_key: PathBuf,
        /// Plugin protocol versions.
        #[arg(long = "protocol", default_value = "5.0")]
        protocols: Vec<String>,
    },
    Delete { address: String, version: String },
}

fn split<const N: usize>(address: &str, what: &str) -> Result<[String; N]> {
    let parts: Vec<String> = address.split('/').map(str::to_string).collect();
    parts.try_into().map_err(|_| anyhow::anyhow!("expected {what}"))
}

pub fn module(c: &Client, out: &Out, cmd: &ModuleCmd) -> Result<()> {
    match cmd {
        ModuleCmd::List => {
            let mut v = c.get("/api/v1/registry/modules")?;
            for m in v["items"].as_array_mut().into_iter().flatten() {
                m["latest"] = m["versions"][0]["version"].clone();
            }
            out.table(&v, &v["items"], &[("NAMESPACE", "namespace"), ("NAME", "name"), ("SYSTEM", "system"), ("LATEST", "latest")]);
        }
        ModuleCmd::Publish { address, version, dir } => {
            let [ns, name, system] = split::<3>(address, "NAMESPACE/NAME/SYSTEM")?;
            let tgz = pack_dir(dir)?;
            let v = c.send_bytes("POST", &format!("/api/v1/registry/modules/{ns}/{name}/{system}/{version}"), "application/gzip", &tgz)?;
            out.object(&v, &["source", "version", "sha256"]);
        }
        ModuleCmd::Delete { address, version } => {
            let [ns, name, system] = split::<3>(address, "NAMESPACE/NAME/SYSTEM")?;
            let v = version.as_ref().map(|v| format!("/{v}")).unwrap_or_default();
            c.delete(&format!("/api/v1/registry/modules/{ns}/{name}/{system}{v}"))?;
        }
    }
    Ok(())
}

pub fn provider(c: &Client, out: &Out, cmd: &ProviderCmd) -> Result<()> {
    match cmd {
        ProviderCmd::List => {
            let mut v = c.get("/api/v1/registry/providers")?;
            for p in v["items"].as_array_mut().into_iter().flatten() {
                p["latest"] = p["versions"][0]["version"].clone();
            }
            out.table(&v, &v["items"], &[("NAMESPACE", "namespace"), ("TYPE", "type"), ("LATEST", "latest")]);
        }
        ProviderCmd::Publish { address, version, dir, gpg_key, protocols } => {
            let [ns, kind] = split::<2>(address, "NAMESPACE/TYPE")?;
            let sums_path = [format!("terraform-provider-{kind}_{version}_SHA256SUMS"), "SHA256SUMS".into()]
                .iter()
                .map(|n| dir.join(n))
                .find(|p| p.exists())
                .with_context(|| format!("no terraform-provider-{kind}_{version}_SHA256SUMS in {}", dir.display()))?;
            let sums = std::fs::read_to_string(&sums_path)?;
            let sig = std::fs::read(sums_path.with_file_name(format!("{}.sig", sums_path.file_name().unwrap().to_string_lossy())))
                .context("reading the SHA256SUMS.sig detached signature")?;
            let key = std::fs::read_to_string(gpg_key).with_context(|| format!("reading {}", gpg_key.display()))?;
            let prefix = format!("terraform-provider-{kind}_{version}_");
            let zips: Vec<(String, String, String)> = sums
                .lines()
                .filter_map(|l| l.split_whitespace().nth(1))
                .filter_map(|f| {
                    let plat = f.strip_prefix(&prefix)?.strip_suffix(".zip")?;
                    let (os, arch) = plat.split_once('_')?;
                    Some((f.to_string(), os.to_string(), arch.to_string()))
                })
                .collect();
            if zips.is_empty() {
                bail!("SHA256SUMS lists no {prefix}<os>_<arch>.zip files");
            }
            let v = c.send("POST", &format!("/api/v1/registry/providers/{ns}/{kind}/{version}"), &json!({
                "protocols": protocols, "gpg_public_key": key, "shasums": sums,
                "shasums_sig": base64::engine::general_purpose::STANDARD.encode(sig)}))?;
            let mut platforms = vec![];
            for (file, os, arch) in &zips {
                let body = std::fs::read(dir.join(file)).with_context(|| format!("reading {file}"))?;
                c.send_bytes("PUT", &format!("/api/v1/registry/providers/{ns}/{kind}/{version}/{os}/{arch}"), "application/zip", &body)?;
                platforms.push(format!("{os}/{arch}"));
                if !out.json {
                    eprintln!("uploaded {file}");
                }
            }
            let p = c.get(&format!("/api/v1/registry/providers/{ns}/{kind}"))?;
            let r = json!({"source": p["source"], "version": version, "key_id": v["key_id"], "platforms": platforms});
            out.object(&r, &["source", "version", "key_id", "platforms"]);
        }
        ProviderCmd::Delete { address, version } => {
            let [ns, kind] = split::<2>(address, "NAMESPACE/TYPE")?;
            c.delete(&format!("/api/v1/registry/providers/{ns}/{kind}/{version}"))?;
        }
    }
    Ok(())
}
