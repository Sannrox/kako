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
    let mut reader = BufReader::new(file);
    let mut hasher = Sha256::new();
    let mut buf = [0_u8; 64 * 1024];
    loop {
        let n = reader
            .read(&mut buf)
            .map_err(|error| format!("{}: {error}", path.display()))?;
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
}
