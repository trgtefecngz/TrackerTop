//! Small filesystem helpers shared by the installer and the global project registry.

use anyhow::{Context, Result, bail};
use std::fs::{File, OpenOptions, TryLockError};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Writes `contents` to `path` atomically: data goes to a sibling temp file, is flushed to
/// disk (`fsync`), and is then renamed over the target. Readers never observe a half-written
/// file, and a power loss right after the rename cannot leave an empty file behind.
/// A failed write never leaves the temp file lying around.
pub fn atomic_write(path: &Path, contents: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    // pid + per-process counter: unique even when several threads write the same target.
    let seq = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp = sibling_path(path, &format!("tmp-{}-{seq}", std::process::id()));

    let written = (|| -> std::io::Result<()> {
        let mut file = File::create(&tmp)?;
        file.write_all(contents)?;
        file.sync_all()
    })();
    if let Err(e) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("failed to write {}", tmp.display()));
    }
    if let Err(e) = rename_with_retry(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("failed to replace {}", path.display()));
    }
    Ok(())
}

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// On Windows, replacing a file fails with `PermissionDenied` while another process
/// (antivirus, OneDrive, search indexer) briefly holds it open without delete-sharing.
/// Those holds last milliseconds, so retry a few times before giving up.
fn rename_with_retry(from: &Path, to: &Path) -> std::io::Result<()> {
    const ATTEMPTS: u32 = 5;
    let mut attempt = 0;
    loop {
        match std::fs::rename(from, to) {
            Ok(()) => return Ok(()),
            Err(e)
                if e.kind() == std::io::ErrorKind::PermissionDenied && attempt + 1 < ATTEMPTS =>
            {
                attempt += 1;
                std::thread::sleep(Duration::from_millis(15 * u64::from(attempt)));
            }
            Err(e) => return Err(e),
        }
    }
}

/// Copies an existing file to `<name>.trackertop.bak` before TrackerTop modifies it.
///
/// The FIRST backup wins: an existing `.trackertop.bak` is never overwritten, so running the
/// installer again cannot replace the pristine pre-TrackerTop copy with an already-modified one.
pub fn backup_file(path: &Path) -> Result<Option<PathBuf>> {
    if !path.is_file() {
        return Ok(None);
    }
    let bak = sibling_path(path, "trackertop.bak");
    if bak.exists() {
        return Ok(Some(bak));
    }
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

/// Human-readable form of a canonicalized path: strips the Windows verbatim prefix
/// (`\\?\C:\x` -> `C:\x`) and restores UNC shares (`\\?\UNC\srv\share` -> `\\srv\share`).
pub fn display_path(path: &Path) -> String {
    let s = path.display().to_string();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        return format!(r"\\{rest}");
    }
    s.strip_prefix(r"\\?\").map(str::to_string).unwrap_or(s)
}

/// Cross-process exclusive lock on `<path>.lock`, held through an operating-system file lock
/// (`LockFileEx` on Windows, `flock` on Unix).
///
/// The OS releases the lock when the holder exits or crashes, so there is no stale-lock
/// detection to get wrong: no spinning on undeletable lock files, no two processes breaking
/// the same stale lock, and no holder deleting a lock someone else now owns. The lock file
/// itself stays on disk and is reused.
pub struct FileLock {
    file: File,
}

impl FileLock {
    /// Waits up to `wait` for the lock.
    pub fn acquire(path: &Path, wait: Duration) -> Result<Self> {
        let lock_path = sibling_path(path, "lock");
        if let Some(parent) = lock_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .with_context(|| format!("failed to open lock {}", lock_path.display()))?;
        let deadline = Instant::now() + wait;
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(Self { file }),
                Err(TryLockError::WouldBlock) => {
                    if Instant::now() >= deadline {
                        bail!("timed out waiting for lock {}", lock_path.display());
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
                Err(TryLockError::Error(e)) => {
                    return Err(e)
                        .with_context(|| format!("failed to lock {}", lock_path.display()));
                }
            }
        }
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        // Closing the handle releases it too; unlock explicitly to release it immediately.
        let _ = self.file.unlock();
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
    fn failed_atomic_write_leaves_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        // Target is a non-empty directory: the final rename must fail
        let target = dir.path().join("cfg.json");
        std::fs::create_dir(&target).unwrap();
        std::fs::write(target.join("keep"), b"x").unwrap();

        assert!(atomic_write(&target, b"data").is_err());
        let names: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(names, vec!["cfg.json".to_string()], "temp file left behind");
    }

    #[test]
    fn backup_keeps_the_first_pristine_copy() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("settings.json");
        std::fs::write(&p, "original").unwrap();
        let bak = backup_file(&p).unwrap().unwrap();
        std::fs::write(&p, "modified by trackertop").unwrap();
        backup_file(&p).unwrap();
        assert_eq!(std::fs::read_to_string(bak).unwrap(), "original");
    }

    #[test]
    fn file_lock_is_exclusive_and_released_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("reg.json");
        let guard = FileLock::acquire(&p, Duration::from_millis(10)).unwrap();
        assert!(FileLock::acquire(&p, Duration::from_millis(50)).is_err());
        drop(guard);
        assert!(FileLock::acquire(&p, Duration::from_millis(10)).is_ok());
    }

    #[test]
    fn leftover_lock_file_from_a_crashed_process_does_not_block() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("reg.json");
        // Old-style lock file nobody holds an OS lock on
        std::fs::write(dir.path().join("reg.json.lock"), b"").unwrap();
        assert!(FileLock::acquire(&p, Duration::from_millis(10)).is_ok());
    }

    #[test]
    fn display_path_strips_verbatim_prefixes() {
        assert_eq!(display_path(Path::new(r"\\?\C:\Users\me")), r"C:\Users\me");
        assert_eq!(
            display_path(Path::new(r"\\?\UNC\server\share")),
            r"\\server\share"
        );
        assert_eq!(display_path(Path::new("/home/me")), "/home/me");
    }
}
