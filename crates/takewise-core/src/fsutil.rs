//! The project lock and atomic file replacement.

use std::fs::{self, File, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::{Result, io};

pub const LOCK_FILE: &str = ".lock";

/// Holds the project's exclusive advisory lock until dropped.
#[derive(Debug)]
pub struct ProjectLock {
    _file: File,
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
    file.lock().map_err(io(&path))?;
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
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let dir = path.parent().unwrap_or(Path::new("."));
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let temp = dir.join(format!(
        ".{name}.{}-{}.tmp",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
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
