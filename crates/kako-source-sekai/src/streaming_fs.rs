//! Crash-safe file updates for the streaming adapters.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// Exclusive lock on `dir/.lock`. Drop to release.
pub struct DirLock {
    _file: File,
}

impl DirLock {
    pub fn acquire(dir: &Path) -> Result<Self, String> {
        fs::create_dir_all(dir).map_err(|error| format!("create {}: {error}", dir.display()))?;
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(dir.join(".lock"))
            .map_err(|error| format!("open lock {}: {error}", dir.display()))?;
        file.lock()
            .map_err(|error| format!("lock {}: {error}", dir.display()))?;
        Ok(Self { _file: file })
    }
}

/// A single path component: no separators, not `.` / `..`.
pub fn safe_component<'a>(name: &'a str, what: &str) -> Result<&'a str, String> {
    let path = Path::new(name);
    if name.is_empty()
        || name.contains(['/', '\\', '\0'])
        || name == "."
        || name == ".."
        || path.is_absolute()
        || path.components().count() != 1
    {
        return Err(format!("{what} `{name}` is not a safe path component"));
    }
    Ok(name)
}

/// Write `bytes` to `path` via a sibling temp file, then rename.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("no parent for {}", path.display()))?;
    fs::create_dir_all(parent).map_err(|error| format!("create {}: {error}", parent.display()))?;
    let file_name = path
        .file_name()
        .ok_or_else(|| format!("no file name for {}", path.display()))?;
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp = parent.join(format!(
        ".{}.{}.{nanos}.tmp",
        file_name.to_string_lossy(),
        std::process::id()
    ));
    let write = (|| {
        let mut file =
            File::create(&tmp).map_err(|error| format!("create {}: {error}", tmp.display()))?;
        file.write_all(bytes)
            .map_err(|error| format!("write {}: {error}", tmp.display()))?;
        file.sync_all()
            .map_err(|error| format!("sync {}: {error}", tmp.display()))?;
        fs::rename(&tmp, path)
            .map_err(|error| format!("rename {} -> {}: {error}", tmp.display(), path.display()))?;
        File::open(parent)
            .map_err(|error| format!("sync {}: {error}", parent.display()))?
            .sync_all()
            .map_err(|error| format!("sync {}: {error}", parent.display()))?;
        Ok(())
    })();
    if write.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    write
}

pub fn atomic_write_json<T: serde::Serialize>(path: &Path, value: &T) -> Result<(), String> {
    let mut text = serde_json::to_string_pretty(value)
        .map_err(|error| format!("serialize {}: {error}", path.display()))?;
    text.push('\n');
    atomic_write(path, text.as_bytes())
}

pub fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Option<T>, String> {
    if !path.exists() {
        return Ok(None);
    }
    let text =
        fs::read_to_string(path).map_err(|error| format!("read {}: {error}", path.display()))?;
    serde_json::from_str(&text)
        .map(Some)
        .map_err(|error| format!("parse {}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsafe_components_are_refused() {
        for name in ["", ".", "..", "a/b", "a\\b", "/abs", "a\0b"] {
            assert!(safe_component(name, "group").is_err(), "{name}");
        }
        assert_eq!(
            safe_component("kako-orders", "group").unwrap(),
            "kako-orders"
        );
    }

    #[test]
    fn atomic_write_replaces_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.json");
        atomic_write(&path, b"one").unwrap();
        atomic_write(&path, b"two").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "two");
    }
}
