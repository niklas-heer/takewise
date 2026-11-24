//! The project lock and atomic file replacement.

use std::fs::{self, File, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::{Result, io};

pub const LOCK_FILE: &str = ".lock";

/// A file whose exclusive advisory lock (`flock`) this holds, and releases
/// when dropped.
///
/// The lock is released explicitly, not by closing the file: a process
/// being spawned on another thread briefly shares this process's open
/// files, and a lock that only ended with its last open copy would outlive
/// its guard until that child has started. It still ends with the process,
/// so a crash leaves nothing locked.
#[derive(Debug)]
pub(crate) struct LockedFile(File);

impl LockedFile {
    /// Locks `file` without waiting; `WouldBlock` when another holder has
    /// it.
    pub(crate) fn try_lock(file: File) -> std::io::Result<Self> {
        match file.try_lock() {
            Ok(()) => Ok(Self(file)),
            Err(fs::TryLockError::WouldBlock) => Err(std::io::ErrorKind::WouldBlock.into()),
            Err(fs::TryLockError::Error(err)) => Err(err),
        }
    }

    /// Locks `file`, waiting for other holders.
    pub(crate) fn lock(file: File) -> std::io::Result<Self> {
        file.lock()?;
        Ok(Self(file))
    }
}

impl std::ops::Deref for LockedFile {
    type Target = File;

    fn deref(&self) -> &File {
        &self.0
    }
}

impl Drop for LockedFile {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

/// Holds the project's exclusive advisory lock until dropped.
#[derive(Debug)]
pub struct ProjectLock {
    _file: LockedFile,
}

/// Takes the exclusive advisory lock on `<dir>/.lock`, waiting for other
/// writers. Hold it only for short writes, never while recording.
///
/// Fails with `NotAProject` when `dir` has no `project.toml`, so a mistyped
/// path never gains a stray `.lock` file.
pub fn lock(dir: &Path) -> Result<ProjectLock> {
    if !dir.join(crate::project::PROJECT_FILE).is_file() {
        return Err(crate::error::Error::NotAProject(dir.to_path_buf()));
    }
    let path = dir.join(LOCK_FILE);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .map_err(io(&path))?;
    let file = LockedFile::lock(file).map_err(io(&path))?;
    Ok(ProjectLock { _file: file })
}

/// Replaces `path` with `bytes`: a temp file in the same directory, fsync,
/// rename over.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    with_temp(path, |file| file.write_all(bytes))
}

/// Writes through `write` into a temp file next to `path`, fsyncs and renames
/// it over `path`. The temp file is removed if anything fails.
pub(crate) fn with_temp(
    path: &Path,
    write: impl FnOnce(&mut File) -> std::io::Result<()>,
) -> Result<()> {
    let temp = write_temp(path, write)?;
    rename_temp(&temp, path)
}

/// Writes through `write` into a new temp file next to `path` and fsyncs it;
/// returns the temp file's path for `rename_temp`. Nothing is left behind if
/// writing fails.
pub(crate) fn write_temp(
    path: &Path,
    write: impl FnOnce(&mut File) -> std::io::Result<()>,
) -> Result<PathBuf> {
    let temp = temp_beside(path);
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        write(&mut file)?;
        file.sync_all()
    })();
    if let Err(err) = result {
        let _ = fs::remove_file(&temp);
        return Err(io(path)(err));
    }
    Ok(temp)
}

/// A name for a temp file next to `path` that nothing else uses:
/// `.<name>.<pid>-<n>.tmp`.
pub(crate) fn temp_beside(path: &Path) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let dir = path.parent().unwrap_or(Path::new("."));
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    dir.join(format!(
        ".{name}.{}-{}.tmp",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ))
}

/// Fsyncs the directory `path` is in, so a rename to `path` survives a
/// power loss.
pub(crate) fn sync_parent(path: &Path) -> Result<()> {
    let dir = match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir,
        _ => Path::new("."),
    };
    File::open(dir)
        .and_then(|dir| dir.sync_all())
        .map_err(io(dir))
}

/// Renames a temp file from `write_temp` over `path`, removing it on failure.
pub(crate) fn rename_temp(temp: &Path, path: &Path) -> Result<()> {
    fs::rename(temp, path).map_err(|err| {
        let _ = fs::remove_file(temp);
        io(path)(err)
    })
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::time::Duration;

    use super::*;

    #[test]
    fn write_atomic_replaces_contents() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("a.toml");
        write_atomic(&path, b"one").unwrap();
        write_atomic(&path, b"two").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"two");
        let leftovers: Vec<_> = std::fs::read_dir(tmp.path()).unwrap().collect();
        assert_eq!(leftovers.len(), 1, "no temp files left behind");
    }

    #[test]
    fn lock_refuses_a_directory_without_a_project() {
        let tmp = tempfile::tempdir().unwrap();
        let err = lock(tmp.path()).unwrap_err();
        assert!(
            matches!(err, crate::error::Error::NotAProject(_)),
            "{err:?}"
        );
        assert!(!tmp.path().join(LOCK_FILE).exists(), "no stray lock file");
        let missing = tmp.path().join("missing");
        assert!(matches!(
            lock(&missing).unwrap_err(),
            crate::error::Error::NotAProject(_)
        ));
    }

    #[test]
    fn a_dropped_lock_is_free_while_a_copy_of_its_file_is_open() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("audio");
        std::fs::write(&path, b"").unwrap();
        let open = || {
            OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
                .unwrap()
        };
        let file = open();
        // What a process being spawned holds until it has started.
        let copy = file.try_clone().unwrap();
        let held = LockedFile::try_lock(file).unwrap();
        let busy = LockedFile::try_lock(open()).unwrap_err();
        assert_eq!(busy.kind(), std::io::ErrorKind::WouldBlock);
        drop(held);
        assert!(LockedFile::try_lock(open()).is_ok(), "free at once");
        drop(copy);
    }

    #[test]
    fn lock_is_exclusive() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
        std::fs::write(dir.join(crate::project::PROJECT_FILE), "").unwrap();
        let first = lock(&dir).unwrap();

        let (tx, rx) = mpsc::channel();
        let other = dir.clone();
        let handle = std::thread::spawn(move || {
            let guard = lock(&other).unwrap();
            tx.send(()).unwrap();
            drop(guard);
        });
        assert!(
            rx.recv_timeout(Duration::from_millis(200)).is_err(),
            "second lock must wait"
        );
        drop(first);
        rx.recv_timeout(Duration::from_secs(5))
            .expect("second lock acquired after the first is released");
        handle.join().unwrap();
    }
}
