//! Encryption at rest for secret values (sensitive variables).
//!
//! ChaCha20-Poly1305 with a 256-bit key read from `CAIRN_SECRET_KEY` (hex) or
//! `<data>/secret.key` (created with mode 0600 on first start). Stored form:
//! `enc:v1:<hex(nonce || ciphertext)>`.

use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};

use crate::util::random_bytes;

const PREFIX: &str = "enc:v1:";

pub struct SecretBox {
    cipher: ChaCha20Poly1305,
}

impl SecretBox {
    pub fn load(data_dir: &Path, env_key: Option<&str>) -> Result<Self> {
        let key = match env_key {
            Some(h) => hex::decode(h.trim()).context("CAIRN_SECRET_KEY must be hex")?,
            None => {
                let path = data_dir.join("secret.key");
                if path.exists() {
                    hex::decode(std::fs::read_to_string(&path)?.trim()).context("secret.key must be hex")?
                } else {
                    let mut k = [0u8; 32];
                    random_bytes(&mut k);
                    write_private(&path, hex::encode(k).as_bytes())?;
                    k.to_vec()
                }
            }
        };
        if key.len() != 32 {
            bail!("secret key must be 32 bytes (64 hex chars)");
        }
        Ok(SecretBox { cipher: ChaCha20Poly1305::new(Key::from_slice(&key)) })
    }

    pub fn seal(&self, plaintext: &str) -> String {
        let mut nonce = [0u8; 12];
        random_bytes(&mut nonce);
        let ct = self
            .cipher
            .encrypt(Nonce::from_slice(&nonce), plaintext.as_bytes())
            .expect("encryption cannot fail");
        let mut out = nonce.to_vec();
        out.extend(ct);
        format!("{PREFIX}{}", hex::encode(out))
    }

    /// Encrypt bytes: `nonce || ciphertext`.
    pub fn seal_bytes(&self, plaintext: &[u8]) -> Vec<u8> {
        let mut nonce = [0u8; 12];
        random_bytes(&mut nonce);
        let ct = self.cipher.encrypt(Nonce::from_slice(&nonce), plaintext).expect("encryption cannot fail");
        let mut out = nonce.to_vec();
        out.extend(ct);
        out
    }

    pub fn open_bytes(&self, data: &[u8]) -> Result<Vec<u8>> {
        if data.len() < 12 {
            bail!("ciphertext too short");
        }
        self.cipher
            .decrypt(Nonce::from_slice(&data[..12]), &data[12..])
            .map_err(|_| anyhow!("cannot decrypt blob (wrong key?)"))
    }

    pub fn open(&self, stored: &str) -> Result<String> {
        let Some(h) = stored.strip_prefix(PREFIX) else {
            return Ok(stored.to_string());
        };
        let raw = hex::decode(h)?;
        if raw.len() < 12 {
            bail!("ciphertext too short");
        }
        let pt = self
            .cipher
            .decrypt(Nonce::from_slice(&raw[..12]), &raw[12..])
            .map_err(|_| anyhow!("cannot decrypt secret (wrong key?)"))?;
        Ok(String::from_utf8(pt)?)
    }
}

pub fn write_private(path: &Path, data: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(path)?;
    f.write_all(data)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let dir = std::env::temp_dir().join(crate::util::new_id("crypto-test"));
        std::fs::create_dir_all(&dir).unwrap();
        let b = SecretBox::load(&dir, None).unwrap();
        let s = b.seal("hunter2");
        assert!(s.starts_with(PREFIX) && !s.contains("hunter2"));
        assert_eq!(b.open(&s).unwrap(), "hunter2");
        // Same key is reloaded from disk.
        let b2 = SecretBox::load(&dir, None).unwrap();
        assert_eq!(b2.open(&s).unwrap(), "hunter2");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
