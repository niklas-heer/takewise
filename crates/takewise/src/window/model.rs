//! The speech model banner: while the model is missing, the window offers
//! to install it (I, or the button), downloads it on a thread with
//! progress, and once it is installed checks again the takes whose words
//! went unchecked meanwhile.

use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex, PoisonError};

use takewise_core::models::{self, Download, ModelSpec, ModelState, Progress};
use takewise_core::{CheckConfig, Project};

use super::checks::Checker;

/// Installs the model; the real one downloads it, tests use a fake.
pub trait Installer: Send + Sync {
    /// Installs the model, telling `progress` how far it is; `progress`
    /// breaks to cancel.
    fn install(&self, progress: &mut dyn FnMut(Progress) -> ControlFlow<()>) -> Result<(), String>;
    /// The checker for `project` once the model is installed.
    fn checker(&self, project: &Project) -> Checker;
}

/// Downloads the pinned model into the models directory.
pub struct HuggingFace {
    dir: PathBuf,
    spec: ModelSpec,
}

impl Installer for HuggingFace {
    fn install(&self, progress: &mut dyn FnMut(Progress) -> ControlFlow<()>) -> Result<(), String> {
        models::install(&self.dir, &self.spec, &Download::default(), progress)
            .map(|_| ())
            .map_err(|err| err.to_string())
    }

    fn checker(&self, project: &Project) -> Checker {
        super::checker(project)
    }
}

/// The banner for the project's model in `dir`, when it can be installed
/// and is not: the configured model has a pinned download and neither it
/// nor a file of its name is there (a partial download is resumed).
pub fn prompt_for(config: &CheckConfig, dir: &Path) -> Option<ModelPrompt> {
    let spec = models::pinned(&config.whisper_model)?;
    match models::status(dir, &spec).state {
        ModelState::Missing | ModelState::Partial { .. } => {
            let size = spec.size;
            Some(ModelPrompt::new(
                Arc::new(HuggingFace {
                    dir: dir.to_path_buf(),
                    spec,
                }),
                size,
            ))
        }
        ModelState::Installed | ModelState::WrongSize { .. } => None,
    }
}

enum Stage {
    Offer,
    Installing {
        progress: Arc<Mutex<Option<Progress>>>,
        done: Receiver<Result<(), String>>,
    },
    Failed(String),
}

/// The banner's state.
pub struct ModelPrompt {
    installer: Arc<dyn Installer>,
    /// The model's size in bytes, for the offer.
    size: u64,
    stage: Stage,
    /// Set when the window closes: the download stops, its partial file
    /// stays for next time.
    cancel: Arc<AtomicBool>,
}

impl ModelPrompt {
    pub fn new(installer: Arc<dyn Installer>, size: u64) -> Self {
        ModelPrompt {
            installer,
            size,
            stage: Stage::Offer,
            cancel: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn installer(&self) -> Arc<dyn Installer> {
        Arc::clone(&self.installer)
    }

    pub fn installing(&self) -> bool {
        matches!(self.stage, Stage::Installing { .. })
    }

    /// Starts the install on a thread, unless it runs.
    pub fn start(&mut self) {
        if self.installing() {
            return;
        }
        let progress = Arc::new(Mutex::new(None));
        let (tx, done) = channel();
        let installer = Arc::clone(&self.installer);
        let shared = Arc::clone(&progress);
        let cancel = Arc::clone(&self.cancel);
        let spawned = std::thread::Builder::new()
            .name("takewise-model".to_owned())
            .spawn(move || {
                let result = installer.install(&mut |p| {
                    *shared.lock().unwrap_or_else(PoisonError::into_inner) = Some(p);
                    if cancel.load(Ordering::Relaxed) {
                        ControlFlow::Break(())
                    } else {
                        ControlFlow::Continue(())
                    }
                });
                let _ = tx.send(result);
            });
        self.stage = match spawned {
            Ok(_) => Stage::Installing { progress, done },
            Err(err) => Stage::Failed(format!("could not start the download: {err}")),
        };
    }

    /// Whether the install finished since the last call (a failure stays
    /// in the banner, offering to try again).
    pub fn poll(&mut self) -> bool {
        let Stage::Installing { done, .. } = &self.stage else {
            return false;
        };
        match done.try_recv() {
            Ok(Ok(())) => true,
            Ok(Err(err)) => {
                self.stage = Stage::Failed(err);
                false
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => false,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.stage = Stage::Failed("the download stopped".to_owned());
                false
            }
        }
    }

    /// Stops a running download (as the window closes).
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    /// How far the download is, 0 to 1, while it runs.
    pub fn fraction(&self) -> Option<f64> {
        let Stage::Installing { progress, .. } = &self.stage else {
            return None;
        };
        let p = *progress.lock().unwrap_or_else(PoisonError::into_inner);
        Some(p.map_or(0.0, |p| p.percent() / 100.0))
    }

    /// The banner's text.
    pub fn text(&self) -> String {
        let size = gigabytes(self.size);
        match &self.stage {
            Stage::Offer => format!(
                "Install the speech model ({size}) to check the words of each take. Press I. Until then, takes are checked without words."
            ),
            Stage::Installing { progress, .. } => {
                match *progress.lock().unwrap_or_else(PoisonError::into_inner) {
                    Some(p) if p.verifying => "Verifying the speech model…".to_owned(),
                    Some(p) => format!(
                        "Installing the speech model: {} of {size} ({:.0} %). Takes are checked without words until it is done.",
                        gigabytes(p.bytes),
                        p.percent()
                    ),
                    None => format!("Installing the speech model ({size})…"),
                }
            }
            Stage::Failed(err) => {
                format!("Could not install the speech model: {err}. Press I to try again.")
            }
        }
    }
}

/// "1.5 GB" (binary gigabytes, as the design names the model's size).
pub fn gigabytes(bytes: u64) -> String {
    format!("{:.1} GB", bytes as f64 / f64::from(1u32 << 30))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offered_only_while_the_pinned_model_is_missing() {
        let dir = tempfile::tempdir().expect("dir");
        let config = CheckConfig::default();
        let prompt = prompt_for(&config, dir.path()).expect("missing: offered");
        assert!(
            prompt.text().contains("Install the speech model (1.5 GB)"),
            "{}",
            prompt.text()
        );

        std::fs::write(dir.path().join("ggml-large-v3-turbo.bin.part"), b"x").expect("part");
        assert!(
            prompt_for(&config, dir.path()).is_some(),
            "partial: offered"
        );

        std::fs::File::create(dir.path().join("ggml-large-v3-turbo.bin"))
            .and_then(|file| file.set_len(models::PINNED.size))
            .expect("model");
        assert!(prompt_for(&config, dir.path()).is_none(), "installed");

        let other = CheckConfig {
            whisper_model: "ggml-base.bin".to_owned(),
            ..CheckConfig::default()
        };
        assert!(
            prompt_for(&other, tempfile::tempdir().expect("dir").path()).is_none(),
            "no pinned download for another model"
        );
    }
}
