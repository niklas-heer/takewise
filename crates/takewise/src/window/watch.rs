//! Watching the project folder (`FSEvents` through `notify`) so the window
//! reloads when an agent edits `script.toml` or appends to `takes.jsonl`.

use std::path::Path;
use std::sync::mpsc::{Receiver, channel};

use notify::{RecommendedWatcher, RecursiveMode, Watcher as _};

/// The files whose changes the window shows.
const WATCHED: [&str; 3] = ["script.toml", "takes.jsonl", "project.toml"];

pub struct Watcher {
    _watcher: RecommendedWatcher,
    rx: Receiver<()>,
}

impl Watcher {
    pub fn new(dir: &Path) -> notify::Result<Self> {
        let (tx, rx) = channel();
        let mut watcher =
            notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
                let relevant = event.is_ok_and(|event| {
                    event.paths.iter().any(|path| {
                        path.file_name()
                            .and_then(|name| name.to_str())
                            .is_some_and(|name| WATCHED.contains(&name))
                    })
                });
                if relevant {
                    // A closed receiver means the window is gone.
                    let _ = tx.send(());
                }
            })?;
        watcher.watch(dir, RecursiveMode::NonRecursive)?;
        Ok(Self {
            _watcher: watcher,
            rx,
        })
    }

    /// Whether a watched file changed since the last call.
    pub fn changed(&self) -> bool {
        let mut changed = false;
        while self.rx.try_recv().is_ok() {
            changed = true;
        }
        changed
    }
}
