//! Small helpers: IDs, timestamps, hashing, randomness.

use std::io::Read;

use sha2::{Digest, Sha256};

const ALPHABET: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

/// Fill `buf` from the OS CSPRNG.
pub fn random_bytes(buf: &mut [u8]) {
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(buf))
        .expect("reading /dev/urandom");
}

/// `prefix-` followed by 16 base62 characters (~95 bits), e.g. `run-4fTq0b9XzA1cD2eF`.
pub fn new_id(prefix: &str) -> String {
    let mut raw = [0u8; 16];
    random_bytes(&mut raw);
    let mut s = String::with_capacity(prefix.len() + 17);
    s.push_str(prefix);
    s.push('-');
    for b in raw {
        s.push(ALPHABET[(b % 62) as usize] as char);
    }
    s
}

/// A new API token: `cairn_` + 43 base62 chars (~256 bits).
pub fn new_token() -> String {
    let mut raw = [0u8; 43];
    random_bytes(&mut raw);
    let mut s = String::from("cairn_");
    for b in raw {
        s.push(ALPHABET[(b % 62) as usize] as char);
    }
    s
}

pub fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

/// RFC 3339 UTC timestamp, fixed-width milliseconds (so timestamps sort as strings).
pub fn now() -> String {
    let t = time::OffsetDateTime::now_utc();
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        t.year(),
        t.month() as u8,
        t.day(),
        t.hour(),
        t.minute(),
        t.second(),
        t.millisecond()
    )
}

/// Constant-time comparison for secrets.
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_prefixed_and_unique() {
        let a = new_id("run");
        let b = new_id("run");
        assert!(a.starts_with("run-") && a.len() == 20);
        assert_ne!(a, b);
    }

    #[test]
    fn timestamps_are_fixed_width() {
        let t = now();
        assert_eq!(t.len(), 24, "{t}");
        assert!(t.ends_with('Z'));
    }

    #[test]
    fn ct_eq_works() {
        assert!(ct_eq(b"abc", b"abc"));
        assert!(!ct_eq(b"abc", b"abd"));
        assert!(!ct_eq(b"abc", b"ab"));
    }
}
