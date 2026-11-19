//! Small filesystem helpers shared by the installer and the global project registry.

use anyhow::{Context, Result, bail};
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

/// Writes `contents` to `path` atomically: data goes to a sibling temp file first and is
/// then renamed over the target, so readers never observe a half-written file.
pub fn atomic_write(path: &Path, contents: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let tmp = sibling_path(path, &format!("tmp-{}", std::process::id()));
    std::fs::write(&tmp, contents)
        .with_context(|| format!("failed to write {}", tmp.display()))?;
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("failed to replace {}", path.display()));
    }
    Ok(())
}

/// Copies an existing file to `<name>.trackertop.bak` before TrackerTop modifies it.
pub fn backup_file(path: &Path) -> Result<Option<PathBuf>> {
    if !path.is_file() {
        return Ok(None);
    }
    let bak = sibling_path(path, "trackertop.bak");
    std::fs::copy(path, &bak)
        .with_context(|| format!("failed to back up {} to {}", path.display(), bak.display()))?;
    Ok(Some(bak))
}

fn sibling_path(path: &Path, suffix: &str) -> PathBuf {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "file".to_string());
    path.with_file_name(format!("{name}.{suffix}"))
}

/// Cross-process advisory lock implemented with an exclusive lock file.
/// The lock file is removed when the guard is dropped.
pub struct FileLock {
    path: PathBuf,
}

impl FileLock {
    /// Tries to acquire `<path>.lock` for up to `wait`. Lock files older than `stale_after`
    /// (left behind by a crashed process) are broken automatically.
    pub fn acquire(path: &Path, wait: Duration, stale_after: Duration) -> Result<Self> {
        let lock_path = sibling_path(path, "lock");
        if let Some(parent) = lock_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let deadline = Instant::now() + wait;
        loop {
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&lock_path)
            {
                Ok(_) => return Ok(Self { path: lock_path }),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    let is_stale = std::fs::metadata(&lock_path)
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|t| SystemTime::now().duration_since(t).ok())
                        .is_some_and(|age| age > stale_after);
                    if is_stale {
                        let _ = std::fs::remove_file(&lock_path);
                        continue;
                    }
                    if Instant::now() >= deadline {
                        bail!("timed out waiting for lock {}", lock_path.display());
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
                Err(e) => {
                    return Err(e)
                        .with_context(|| format!("failed to create lock {}", lock_path.display()));
                }
            }
        }
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_write_replaces_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.json");
        atomic_write(&p, b"one").unwrap();
        atomic_write(&p, b"two").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "two");
        // No temp files left behind
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn file_lock_is_exclusive_and_released_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("reg.json");
        let long = Duration::from_secs(60);
        let guard = FileLock::acquire(&p, Duration::from_millis(10), long).unwrap();
        assert!(FileLock::acquire(&p, Duration::from_millis(50), long).is_err());
        drop(guard);
        assert!(FileLock::acquire(&p, Duration::from_millis(10), long).is_ok());
    }
}
