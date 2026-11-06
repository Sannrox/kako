//! SHA-256 content digests written as `sha256:<hex>`.

use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;

use sha2::{Digest as _, Sha256};

use crate::Digest;

/// Digest of an in-memory buffer.
pub fn hash_bytes(bytes: &[u8]) -> Digest {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    finish(hasher)
}

/// Digest of a file's contents. Reads the file; does not follow a policy
/// beyond what `File::open` does.
pub fn hash_file(path: &Path) -> Result<Digest, String> {
    let file = File::open(path).map_err(|error| format!("{}: {error}", path.display()))?;
    hash_reader(file).map_err(|error| format!("{}: {error}", path.display()))
}

/// Digest of everything `reader` yields.
pub fn hash_reader(reader: impl Read) -> std::io::Result<Digest> {
    let mut reader = BufReader::new(reader);
    let mut hasher = Sha256::new();
    let mut buf = [0_u8; 64 * 1024];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(finish(hasher))
}

fn finish(hasher: Sha256) -> Digest {
    Digest(format!("sha256:{:x}", hasher.finalize()))
}

/// Parse `sha256:` followed by 64 lowercase hex digits.
pub fn parse(value: &str) -> Result<Digest, String> {
    let hex = value
        .strip_prefix("sha256:")
        .ok_or_else(|| format!("digest `{value}` must be sha256:<64 lowercase hex digits>"))?;
    if hex.len() != 64 || !hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return Err(format!(
            "digest `{value}` must be sha256:<64 lowercase hex digits>"
        ));
    }
    Ok(Digest(value.to_string()))
}

/// True when `value` is written as a SHA-256 digest, even if the hex is invalid.
pub fn is_declared(value: &str) -> bool {
    value.starts_with("sha256:")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fips_vector_abc() {
        // FIPS 180-2 SHA-256 test vector for the three-byte message "abc".
        let digest = hash_bytes(b"abc");
        assert_eq!(
            digest.0,
            "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn file_hash_matches_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("blob");
        std::fs::write(&path, b"abc").unwrap();
        assert_eq!(hash_file(&path).unwrap(), hash_bytes(b"abc"));
    }

    #[test]
    fn parse_accepts_lowercase_sha256() {
        let digest =
            parse("sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")
                .unwrap();
        assert_eq!(digest, hash_bytes(b"abc"));
        assert!(is_declared(&digest.0));
    }

    #[test]
    fn parse_rejects_uppercase_and_short_hex() {
        assert!(
            parse("sha256:BA7816BF8F01CFEA414140DE5DAE2223B00361A396177A9CB410FF61F20015AD")
                .is_err()
        );
        assert!(parse("sha256:abc").is_err());
        assert!(parse("orders.csv").is_err());
        assert!(is_declared("sha256:not-hex"));
        assert!(!is_declared("orders.csv"));
    }
}
