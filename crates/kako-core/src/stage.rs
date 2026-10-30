//! Per-run scratch directory: stage inputs, then remove the tree.

use std::fs;
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::Input;

/// Live scratch tree. Removing it is the only cleanup the runner does.
pub struct Scratch {
    path: PathBuf,
}

impl Scratch {
    /// Create a unique `<parent>/kako-run-<id>-…` with `inputs/` and `outputs/`.
    ///
    /// Allocation is exclusive `create_dir`. An existing path is never removed.
    pub fn create(parent: &Path, run_id: &str) -> Result<Self, String> {
        let safe: String = run_id
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        fs::create_dir_all(parent)
            .map_err(|error| format!("create scratch parent {}: {error}", parent.display()))?;
        let path = allocate_scratch_dir(parent, &safe)?;
        if let Err(error) = fs::create_dir(path.join("inputs"))
            .and_then(|_| fs::create_dir(path.join("outputs")))
            .and_then(|_| fs::create_dir(path.join("tmp")))
        {
            let _ = fs::remove_dir_all(&path);
            return Err(format!("create scratch {}: {error}", path.display()));
        }
        Ok(Self { path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn inputs_dir(&self) -> PathBuf {
        self.path.join("inputs")
    }

    pub fn outputs_dir(&self) -> PathBuf {
        self.path.join("outputs")
    }

    /// Copy each input into `inputs/<name><ext>`.
    ///
    /// Copies, never hard-links: the scratch tree is writable, so a link
    /// would let job SQL overwrite the caller's original file.
    ///
    /// Returns the staged relative file name for each input (for DuckDB views).
    pub fn stage_inputs(&self, inputs: &[Input]) -> Result<Vec<(String, PathBuf)>, String> {
        let mut staged = Vec::with_capacity(inputs.len());
        for input in inputs {
            let meta = fs::symlink_metadata(&input.path).map_err(|error| {
                format!("input `{}` {}: {error}", input.name, input.path.display())
            })?;
            if meta.file_type().is_symlink() {
                return Err(format!(
                    "input `{}` must not be a symlink: {}",
                    input.name,
                    input.path.display()
                ));
            }
            if !meta.is_file() {
                return Err(format!(
                    "input `{}` is not a file: {}",
                    input.name,
                    input.path.display()
                ));
            }
            let ext = input
                .path
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| format!(".{e}"))
                .unwrap_or_default();
            let file_name = format!("{}{ext}", input.name);
            let dest = self.inputs_dir().join(&file_name);
            stage_one(&input.path, &dest)?;
            staged.push((input.name.clone(), PathBuf::from("inputs").join(&file_name)));
        }
        Ok(staged)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn stage_one(src: &Path, dest: &Path) -> Result<(), String> {
    copy_unfollowed(src, dest)
        .map(|_| ())
        .map_err(|error| format!("stage {} -> {}: {error}", src.display(), dest.display()))
}

/// Copy without following a symlink at `src`. The dest is inside the scratch tree.
#[cfg(unix)]
fn copy_unfollowed(src: &Path, dest: &Path) -> io::Result<u64> {
    use std::fs::{File, OpenOptions};
    use std::io::copy;
    use std::os::unix::fs::OpenOptionsExt;
    let mut from = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(src)?;
    if !from.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not a regular file",
        ));
    }
    let mut to = File::create(dest)?;
    copy(&mut from, &mut to)
}

#[cfg(not(unix))]
fn copy_unfollowed(src: &Path, dest: &Path) -> io::Result<u64> {
    fs::copy(src, dest)
}

fn allocate_scratch_dir(parent: &Path, safe: &str) -> Result<PathBuf, String> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    for _ in 0..64 {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let path = parent.join(format!(
            "kako-run-{safe}-{}-{nanos}-{n}",
            std::process::id()
        ));
        match fs::create_dir(&path) {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(format!("create scratch {}: {error}", path.display()));
            }
        }
    }
    Err(format!(
        "could not allocate a unique scratch directory under {}",
        parent.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn stages_then_removes_the_tree() {
        let parent = tempfile::tempdir().unwrap();
        let src_dir = tempfile::tempdir().unwrap();
        let src = src_dir.path().join("in.csv");
        fs::write(&src, "a,b\n1,2\n").unwrap();
        let scratch_path;
        {
            let scratch = Scratch::create(parent.path(), "t1").unwrap();
            scratch_path = scratch.path().to_path_buf();
            assert!(scratch_path.is_dir());
            assert!(scratch.path().join("tmp").is_dir());
            let staged = scratch
                .stage_inputs(&[Input {
                    name: "orders".into(),
                    path: src.clone(),
                    markings: Vec::new(),
                    column_markings: Default::default(),
                    object_set: String::new(),
                }])
                .unwrap();
            assert_eq!(staged[0].0, "orders");
            assert!(scratch.path().join("inputs/orders.csv").is_file());
            fs::write(scratch.path().join("inputs/orders.csv"), "mutated\n").unwrap();
            assert_eq!(fs::read_to_string(&src).unwrap(), "a,b\n1,2\n");
        }
        assert!(
            !scratch_path.exists(),
            "scratch must be gone after Drop: {}",
            scratch_path.display()
        );
    }

    #[cfg(unix)]
    #[test]
    fn staging_rejects_a_symlink_input() {
        let parent = tempfile::tempdir().unwrap();
        let src_dir = tempfile::tempdir().unwrap();
        let real = src_dir.path().join("real.csv");
        fs::write(&real, "a,b\n1,2\n").unwrap();
        let link = src_dir.path().join("link.csv");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let scratch = Scratch::create(parent.path(), "t2").unwrap();
        let err = scratch
            .stage_inputs(&[Input {
                name: "orders".into(),
                path: link,
                markings: Vec::new(),
                column_markings: Default::default(),
                object_set: String::new(),
            }])
            .unwrap_err();
        assert!(err.contains("symlink"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn copy_does_not_follow_a_symlink_source() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real.csv");
        fs::write(&real, "secret\n").unwrap();
        let link = dir.path().join("link.csv");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let dest = dir.path().join("dest.csv");
        assert!(copy_unfollowed(&link, &dest).is_err());
        assert!(!dest.exists());
    }

    #[test]
    fn create_does_not_delete_an_existing_path() {
        let parent = tempfile::tempdir().unwrap();
        let leftover = parent.path().join("kako-run-same");
        fs::create_dir_all(&leftover).unwrap();
        fs::write(leftover.join("keep.txt"), "keep").unwrap();
        let first = Scratch::create(parent.path(), "same").unwrap();
        let second = Scratch::create(parent.path(), "same").unwrap();
        assert_ne!(first.path(), second.path());
        assert_ne!(first.path(), leftover.as_path());
        assert_ne!(second.path(), leftover.as_path());
        assert_eq!(
            fs::read_to_string(leftover.join("keep.txt")).unwrap(),
            "keep"
        );
        let prefix = "kako-run-same-";
        let first_name = first.path().file_name().unwrap().to_string_lossy();
        let second_name = second.path().file_name().unwrap().to_string_lossy();
        assert!(first_name.starts_with(prefix), "{first_name}");
        assert!(second_name.starts_with(prefix), "{second_name}");
    }
}
