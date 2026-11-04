//! Just enough OpenPGP to name a key: the key ID of an ASCII-armored public
//! key (RFC 4880 §12.2 for v4 keys, RFC 9580 for v6). Signatures are
//! verified by OpenTofu, not here.

use anyhow::{bail, Context, Result};
use base64::Engine as _;
use sha1::Digest as _;

/// Decode the armored block into the binary key packets.
fn dearmor(armored: &str) -> Result<Vec<u8>> {
    let mut lines = armored.lines().map(str::trim).skip_while(|l| !l.starts_with("-----BEGIN PGP PUBLIC KEY BLOCK"));
    lines.next().context("no BEGIN PGP PUBLIC KEY BLOCK line")?;
    let mut in_headers = true;
    let mut b64 = String::new();
    for l in lines {
        if l.starts_with("-----END") {
            break;
        }
        if in_headers {
            if l.is_empty() {
                in_headers = false;
            } else if !l.contains(':') {
                in_headers = false;
                b64.push_str(l);
            }
            continue;
        }
        if l.starts_with('=') && l.len() == 5 {
            continue; // CRC-24
        }
        b64.push_str(l);
    }
    base64::engine::general_purpose::STANDARD.decode(b64).context("armor is not base64")
}

/// Key ID (16 uppercase hex digits) of the primary key.
pub fn key_id(armored: &str) -> Result<String> {
    let data = dearmor(armored)?;
    let (&first, rest) = data.split_first().context("empty key")?;
    if first & 0x80 == 0 {
        bail!("not an OpenPGP packet");
    }
    let (tag, len, rest) = if first & 0x40 != 0 {
        let tag = first & 0x3f;
        let (len, n) = match rest.first().copied().context("truncated")? {
            o @ 0..=191 => (o as usize, 1),
            o @ 192..=223 => ((((o as usize) - 192) << 8) + *rest.get(1).context("truncated")? as usize + 192, 2),
            255 => (u32::from_be_bytes(rest.get(1..5).context("truncated")?.try_into()?) as usize, 5),
            _ => bail!("partial body lengths are not valid for keys"),
        };
        (tag, len, &rest[n..])
    } else {
        let tag = (first >> 2) & 0x0f;
        let (len, n) = match first & 3 {
            0 => (*rest.first().context("truncated")? as usize, 1),
            1 => (u16::from_be_bytes(rest.get(0..2).context("truncated")?.try_into()?) as usize, 2),
            2 => (u32::from_be_bytes(rest.get(0..4).context("truncated")?.try_into()?) as usize, 4),
            _ => bail!("indeterminate packet length"),
        };
        (tag, len, &rest[n..])
    };
    if tag != 6 {
        bail!("first packet is not a public key (tag {tag})");
    }
    let body = rest.get(..len).context("truncated key packet")?;
    match body.first() {
        Some(4) => {
            let mut h = sha1::Sha1::new();
            h.update([0x99]);
            h.update((len as u16).to_be_bytes());
            h.update(body);
            Ok(hex::encode_upper(&h.finalize()[12..]))
        }
        Some(6) => {
            let mut h = sha2::Sha256::new();
            sha2::Digest::update(&mut h, [0x9b]);
            sha2::Digest::update(&mut h, (len as u32).to_be_bytes());
            sha2::Digest::update(&mut h, body);
            Ok(hex::encode_upper(&sha2::Digest::finalize(h)[..8]))
        }
        Some(v) => bail!("unsupported key version {v}"),
        None => bail!("empty key packet"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_garbage() {
        assert!(key_id("hello").is_err());
        assert!(key_id("-----BEGIN PGP PUBLIC KEY BLOCK-----\n\nAAAA\n-----END PGP PUBLIC KEY BLOCK-----").is_err());
    }
}
