//! Take checks off the UI thread: one worker checks queued takes in turn
//! (whisper-cli takes seconds per take) and reports back, and the window
//! shows "checking…" for a take until its check is in the ledger.
//!
//! Checks never compete with recording: while a take records the worker is
//! paused, so a check already running finishes but the next one waits until
//! the take is committed.

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Condvar, Mutex, PoisonError};

use takewise_core::{self as tw, Snapshot, TakeRef, Transcriber};

/// A transcriber the worker thread can own.
pub type SharedTranscriber = Arc<dyn Transcriber + Send + Sync>;

/// A finished check: its verdict, or why the take could not be checked.
pub struct Done {
    pub take: TakeRef,
    pub result: Result<tw::Verdict, String>,
}

/// Holds the worker back from its next check while closed.
#[derive(Default)]
struct Gate {
    paused: Mutex<bool>,
    opened: Condvar,
}

impl Gate {
    fn set(&self, paused: bool) {
        *self.paused.lock().unwrap_or_else(PoisonError::into_inner) = paused;
        if !paused {
            self.opened.notify_all();
        }
    }

    fn wait_open(&self) {
        let paused = self.paused.lock().unwrap_or_else(PoisonError::into_inner);
        drop(
            self.opened
                .wait_while(paused, |paused| *paused)
                .unwrap_or_else(PoisonError::into_inner),
        );
    }
}

pub struct Checks {
    /// `None` when the worker could not be started.
    jobs: Option<Sender<TakeRef>>,
    gate: Arc<Gate>,
    done: Receiver<Done>,
    /// Queued or running, once per queuing.
    pending: Vec<TakeRef>,
    /// Every take queued this session with the line it was queued against,
    /// so a take that cannot be checked is not queued again and again.
    tried: Vec<(TakeRef, Snapshot)>,
}

impl Checks {
    /// Starts the worker; it ends once the `Checks` is dropped and its
    /// current check is done.
    pub fn new(dir: PathBuf, transcriber: SharedTranscriber) -> Self {
        let (jobs_tx, jobs_rx) = channel::<TakeRef>();
        let (done_tx, done_rx) = channel();
        let gate = Arc::new(Gate::default());
        let worker_gate = Arc::clone(&gate);
        let worker = std::thread::Builder::new()
            .name("takewise-checks".to_owned())
            .spawn(move || {
                for take in jobs_rx {
                    worker_gate.wait_open();
                    let result =
                        tw::check_take(&dir, &take, &*transcriber, env!("CARGO_PKG_VERSION"))
                            .map(|checked| checked.verdict)
                            .map_err(|err| err.to_string());
                    if done_tx.send(Done { take, result }).is_err() {
                        break;
                    }
                }
            });
        Self {
            jobs: worker.is_ok().then_some(jobs_tx),
            gate,
            done: done_rx,
            pending: Vec::new(),
            tried: Vec::new(),
        }
    }

    /// Pauses the worker before its next check (a running one finishes),
    /// or lets it go on.
    pub fn set_paused(&self, paused: bool) {
        self.gate.set(paused);
    }

    /// Queues a check of `take` against the line as `snapshot` has it,
    /// unless it was queued against that line before.
    pub fn queue(&mut self, take: &TakeRef, snapshot: &Snapshot) {
        if self
            .tried
            .iter()
            .any(|(t, s)| t == take && s.matches(snapshot))
        {
            return;
        }
        let Some(jobs) = &self.jobs else {
            return;
        };
        if jobs.send(take.clone()).is_ok() {
            self.pending.push(take.clone());
            self.tried.push((take.clone(), snapshot.clone()));
        }
    }

    #[cfg(test)]
    pub fn is_pending(&self, take: &TakeRef) -> bool {
        self.pending.contains(take)
    }

    /// The take numbers of `line` being checked.
    pub fn pending_of(&self, line: &str) -> Vec<u32> {
        self.pending
            .iter()
            .filter(|t| t.line == line)
            .map(|t| t.take)
            .collect()
    }

    /// The checks finished since the last call.
    pub fn poll(&mut self) -> Vec<Done> {
        let done: Vec<Done> = self.done.try_iter().collect();
        for d in &done {
            if let Some(at) = self.pending.iter().position(|t| *t == d.take) {
                self.pending.remove(at);
            }
        }
        done
    }
}

impl Drop for Checks {
    /// Lets a paused worker go on, so it ends with the window.
    fn drop(&mut self) {
        self.gate.set(false);
    }
}
