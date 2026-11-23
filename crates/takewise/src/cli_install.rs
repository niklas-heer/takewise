//! "Install Command Line Tool…": links `~/.local/bin/takewise` to the
//! binary inside `Takewise.app`, for a Takewise installed without Homebrew
//! (whose cask puts `takewise` on `PATH` itself).
//!
//! Only a link to some `Takewise.app` is ever replaced; any other file is
//! left alone. Nothing asks for an administrator: when the folder cannot be
//! written, the message suggests the Homebrew route instead.

use std::io::ErrorKind;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

/// The Homebrew command that installs the app and puts `takewise` on `PATH`.
pub const HOMEBREW: &str = "brew install --cask niklas-heer/tap/takewise";

/// Where the link goes: `TAKEWISE_LINK_DIR` (to try the menu item without
/// touching `~/.local/bin`), else `~/.local/bin`.
pub fn link_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("TAKEWISE_LINK_DIR") {
        return Some(PathBuf::from(dir));
    }
    std::env::var_os("HOME").map(|home| Path::new(&home).join(".local/bin"))
}

/// A path to the `takewise` binary of some `Takewise.app`.
fn is_takewise(path: &Path) -> bool {
    path.ends_with("Takewise.app/Contents/MacOS/takewise")
}

/// The `takewise` inside the running `Takewise.app`, if it runs from one.
pub fn bundled_binary() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?.canonicalize().ok()?;
    is_takewise(&exe).then_some(exe)
}

#[derive(Debug, PartialEq, Eq)]
pub enum Linked {
    Linked,
    AlreadyLinked,
}

/// Links `dir/takewise` to `target`, creating `dir`. A link to another
/// Takewise is replaced; any other file is left alone and reported.
pub fn link(dir: &Path, target: &Path) -> Result<Linked, String> {
    let path = dir.join("takewise");
    match std::fs::symlink_metadata(&path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            let current = std::fs::read_link(&path).map_err(|err| err.to_string())?;
            if current == target {
                return Ok(Linked::AlreadyLinked);
            }
            if !is_takewise(&current) {
                return Err(format!(
                    "{} links to {}, which is not Takewise; left it alone. Remove it to link Takewise there.",
                    path.display(),
                    current.display()
                ));
            }
        }
        Ok(_) => {
            return Err(format!(
                "{} is not a link to Takewise; left it alone. Remove it to link Takewise there.",
                path.display()
            ));
        }
        Err(err) if err.kind() == ErrorKind::NotFound => {}
        Err(err) => return Err(format!("{}: {err}", path.display())),
    }
    let linked = std::fs::create_dir_all(dir)
        .and_then(|()| match std::fs::remove_file(&path) {
            Err(err) if err.kind() != ErrorKind::NotFound => Err(err),
            _ => Ok(()),
        })
        .and_then(|()| symlink(target, &path));
    match linked {
        Ok(()) => Ok(Linked::Linked),
        Err(err) if err.kind() == ErrorKind::PermissionDenied => Err(format!(
            "Cannot write to {} ({err}). Install Takewise with Homebrew instead (`{HOMEBREW}`), which puts takewise on your PATH.",
            dir.display()
        )),
        Err(err) => Err(format!("Could not link {}: {err}", path.display())),
    }
}

/// Whether `dir` is on `path` (a `PATH` value).
fn on_path(dir: &Path, path: Option<&std::ffi::OsStr>) -> bool {
    path.is_some_and(|path| std::env::split_paths(path).any(|entry| entry == dir))
}

/// Links the running app's binary and says what happened, for the window.
pub fn install() -> String {
    let Some(target) = bundled_binary() else {
        return "Only Takewise.app can install the command line tool.".to_owned();
    };
    let Some(dir) = link_dir() else {
        return "Cannot find your home folder.".to_owned();
    };
    let shown = dir.join("takewise");
    match link(&dir, &target) {
        Err(err) => err,
        Ok(linked) => {
            let done = match linked {
                Linked::Linked => format!("Linked {} to Takewise.", shown.display()),
                Linked::AlreadyLinked => format!("{} already links to Takewise.", shown.display()),
            };
            // An app started from Finder has a minimal PATH; the shell's
            // own is what matters, so read the login shell's.
            let path = login_path();
            if on_path(&dir, path.as_deref()) {
                format!("{done} Open a new terminal and type takewise.")
            } else {
                format!(
                    "{done} {} is not on your PATH: add `export PATH=\"$HOME/.local/bin:$PATH\"` to ~/.zshrc.",
                    dir.display()
                )
            }
        }
    }
}

/// `PATH` as the login shell sets it.
fn login_path() -> Option<std::ffi::OsString> {
    let shell = std::env::var_os("SHELL").unwrap_or_else(|| "/bin/zsh".into());
    let out = std::process::Command::new(shell)
        .args(["-lc", "printf %s \"$PATH\""])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| std::ffi::OsString::from(String::from_utf8_lossy(&out.stdout).into_owned()))
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;

    const APP: &str = "/Applications/Takewise.app/Contents/MacOS/takewise";

    #[test]
    fn cli_link_creates_symlink() {
        let tmp = tempfile::tempdir().expect("dir");
        let bin = tmp.path().join(".local/bin");
        assert_eq!(link(&bin, Path::new(APP)), Ok(Linked::Linked));
        assert_eq!(
            std::fs::read_link(bin.join("takewise")).expect("link"),
            PathBuf::from(APP)
        );
        assert_eq!(link(&bin, Path::new(APP)), Ok(Linked::AlreadyLinked));

        // A link to another Takewise.app is replaced.
        let moved = "/Users/me/Downloads/Takewise.app/Contents/MacOS/takewise";
        assert_eq!(link(&bin, Path::new(moved)), Ok(Linked::Linked));
        assert_eq!(
            std::fs::read_link(bin.join("takewise")).expect("link"),
            PathBuf::from(moved)
        );
    }

    #[test]
    fn cli_link_refuses_foreign_file() {
        let tmp = tempfile::tempdir().expect("dir");
        let file = tmp.path().join("takewise");
        std::fs::write(&file, "#!/bin/sh\n").expect("file");
        let err = link(tmp.path(), Path::new(APP)).expect_err("refused");
        assert!(err.contains("not a link to Takewise"), "{err}");
        assert_eq!(std::fs::read_to_string(&file).expect("kept"), "#!/bin/sh\n");

        let other = tempfile::tempdir().expect("dir");
        symlink("/usr/local/bin/other-tool", other.path().join("takewise")).expect("link");
        let err = link(other.path(), Path::new(APP)).expect_err("refused");
        assert!(err.contains("not Takewise"), "{err}");
        assert_eq!(
            std::fs::read_link(other.path().join("takewise")).expect("kept"),
            PathBuf::from("/usr/local/bin/other-tool")
        );
    }

    #[test]
    fn cli_link_unwritable_suggests_homebrew() {
        let tmp = tempfile::tempdir().expect("dir");
        let bin = tmp.path().join("bin");
        std::fs::create_dir(&bin).expect("bin");
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o555)).expect("lock");
        let result = link(&bin, Path::new(APP));
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).expect("unlock");
        let err = result.expect_err("cannot write");
        assert!(
            err.contains("Cannot write to") && err.contains(HOMEBREW),
            "{err}"
        );
    }

    #[test]
    fn path_entries_are_matched_whole() {
        let dir = Path::new("/Users/me/.local/bin");
        assert!(on_path(
            dir,
            Some("/usr/bin:/Users/me/.local/bin:/bin".as_ref())
        ));
        assert!(!on_path(
            dir,
            Some("/usr/bin:/Users/me/.local/bin2".as_ref())
        ));
        assert!(!on_path(dir, None));
    }
}
