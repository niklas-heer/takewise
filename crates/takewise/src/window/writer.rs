//! The take writer: a thread fed directly by the capture. It places every
//! buffer with the gap-filling [`Assembler`], keeps all samples in memory
//! until the take is committed, and streams them into the take's partial
//! WAV once the window names it, flushing the header at least once a
//! second. So a take survives a window that stops rendering, a crash (up to
//! the last flush) and a disk that fails (from memory).

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use takewise_core::{Assembler, Dropout, PartialWav, RECORD_RATE, WavLock};
use takewise_media::CaptureBuffer;

use super::recorder::peak_dbfs;

/// How often the thread looks at its control channel while no audio comes.
const POLL: Duration = Duration::from_millis(10);

/// What the window can see of a running writer.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Progress {
    /// Non-empty buffers received.
    pub buffers: u64,
    /// Host time of the first sample (before latency is removed).
    pub first_host_s: Option<f64>,
    /// Peak of the last buffer in dBFS, and its host time.
    pub level: Option<(f64, f64)>,
    /// The highest peak of the take so far in dBFS.
    pub peak_dbfs: Option<f64>,
    /// Samples durable in the partial WAV (written and flushed).
    pub flushed: u64,
    /// Why the partial WAV could not be written; the audio stays in memory.
    pub error: Option<String>,
}

/// Everything a finished writer captured.
pub struct Written {
    /// Every sample, gaps filled with silence.
    pub samples: Vec<i32>,
    pub first_host_s: Option<f64>,
    pub dropouts: Vec<Dropout>,
    /// Whether the partial WAV holds every sample, finished and synced.
    pub complete: bool,
    /// The partial WAV's lock, held until the take is committed, also when
    /// the WAV could not be finished.
    pub lock: Option<WavLock>,
}

enum Control {
    Open(PathBuf),
    Stop,
    /// Makes finishing the partial WAV fail, as a failing disk would.
    #[cfg(test)]
    FailFinish,
}

pub struct TakeWriter {
    control: Sender<Control>,
    progress: Arc<Mutex<Progress>>,
    handle: Option<JoinHandle<Written>>,
}

impl TakeWriter {
    /// Starts the thread on the capture's receiver.
    pub fn spawn(capture: Receiver<CaptureBuffer>) -> Self {
        let (control, commands) = mpsc::channel();
        let progress = Arc::new(Mutex::new(Progress::default()));
        let shared = Arc::clone(&progress);
        let handle = std::thread::Builder::new()
            .name("takewise-take-writer".to_owned())
            .spawn(move || run(&capture, &commands, &shared))
            .ok();
        Self {
            control,
            progress,
            handle,
        }
    }

    pub fn progress(&self) -> Progress {
        self.progress
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Starts writing into the partial WAV at `wav` (which must not exist):
    /// everything captured so far, then each buffer as it arrives.
    pub fn open(&self, wav: PathBuf) {
        let _ = self.control.send(Control::Open(wav));
    }

    /// Makes finishing the partial WAV fail when the writer stops.
    #[cfg(test)]
    pub fn fail_finish(&self) {
        let _ = self.control.send(Control::FailFinish);
    }

    /// Ends the thread after the capture stopped (every buffer is already
    /// in the channel) and returns what it captured.
    pub fn finish(mut self) -> Written {
        let _ = self.control.send(Control::Stop);
        self.handle
            .take()
            .and_then(|handle| handle.join().ok())
            .unwrap_or_else(|| Written {
                samples: Vec::new(),
                first_host_s: None,
                dropouts: Vec::new(),
                complete: false,
                lock: None,
            })
    }
}

struct State {
    assembler: Assembler,
    samples: Vec<i32>,
    wav: Option<PartialWav>,
    /// Set once writing failed: the WAV is kept locked but not written.
    failed: bool,
    #[cfg(test)]
    fail_finish: bool,
}

impl State {
    fn open(&mut self, path: &Path, progress: &Mutex<Progress>) {
        let opened = PartialWav::create(path, RECORD_RATE).and_then(|mut wav| {
            wav.write(&self.samples)?;
            wav.flush()?;
            Ok(wav)
        });
        match opened {
            Ok(wav) => {
                set(progress, |p| p.flushed = wav.samples());
                self.wav = Some(wav);
            }
            Err(err) => self.fail(&err.to_string(), progress),
        }
    }

    fn push(&mut self, buffer: &CaptureBuffer, progress: &Mutex<Progress>) {
        if buffer.samples.is_empty() {
            return;
        }
        let silence = self
            .assembler
            .push(buffer.host_time_s, buffer.samples.len());
        let from = self.samples.len();
        self.samples.resize(from + silence, 0);
        self.samples.extend_from_slice(&buffer.samples);
        let first = self.assembler.first_host_s();
        set(progress, |p| {
            p.buffers += 1;
            p.first_host_s = first;
            let peak = peak_dbfs(&buffer.samples);
            p.level = Some((peak, buffer.host_time_s));
            p.peak_dbfs = Some(p.peak_dbfs.map_or(peak, |max| max.max(peak)));
        });
        if self.failed {
            return;
        }
        let Some(wav) = &mut self.wav else {
            return;
        };
        let written = wav.write(&self.samples[from..]).and_then(|()| {
            if wav.unflushed() >= u64::from(RECORD_RATE) {
                wav.flush()?;
            }
            Ok(())
        });
        match written {
            Ok(()) => {
                let flushed = wav.samples() - wav.unflushed();
                set(progress, |p| p.flushed = flushed);
            }
            Err(err) => self.fail(&err.to_string(), progress),
        }
    }

    fn fail(&mut self, error: &str, progress: &Mutex<Progress>) {
        self.failed = true;
        set(progress, |p| p.error = Some(error.to_owned()));
    }

    fn finish(self) -> Written {
        // The partial WAV stays locked whether or not it could be finished:
        // a take that failed here is written again from memory under the
        // same lock, so a repair never adopts it meanwhile.
        let (complete, lock) = match self.wav {
            Some(mut wav) => {
                let synced = !self.failed && wav.sync().is_ok();
                #[cfg(test)]
                let synced = synced && !self.fail_finish;
                (synced, Some(wav.into_lock()))
            }
            None => (false, None),
        };
        Written {
            first_host_s: self.assembler.first_host_s(),
            dropouts: self.assembler.dropouts().to_vec(),
            samples: self.samples,
            complete,
            lock,
        }
    }
}

fn set(progress: &Mutex<Progress>, f: impl FnOnce(&mut Progress)) {
    f(&mut progress.lock().unwrap_or_else(PoisonError::into_inner));
}

fn run(
    capture: &Receiver<CaptureBuffer>,
    commands: &Receiver<Control>,
    progress: &Mutex<Progress>,
) -> Written {
    let mut state = State {
        assembler: Assembler::new(RECORD_RATE),
        samples: Vec::new(),
        wav: None,
        failed: false,
        #[cfg(test)]
        fail_finish: false,
    };
    let mut capturing = true;
    'run: loop {
        loop {
            match commands.try_recv() {
                Ok(Control::Open(path)) => state.open(&path, progress),
                #[cfg(test)]
                Ok(Control::FailFinish) => state.fail_finish = true,
                Ok(Control::Stop) | Err(TryRecvError::Disconnected) => break 'run,
                Err(TryRecvError::Empty) => break,
            }
        }
        if !capturing {
            // The capture is gone: wait for the window's next command.
            match commands.recv() {
                Ok(Control::Open(path)) => state.open(&path, progress),
                #[cfg(test)]
                Ok(Control::FailFinish) => state.fail_finish = true,
                Ok(Control::Stop) | Err(_) => break 'run,
            }
            continue;
        }
        match capture.recv_timeout(POLL) {
            Ok(buffer) => state.push(&buffer, progress),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => capturing = false,
        }
    }
    // The capture has stopped: the buffers it sent are all in the channel.
    for buffer in capture.try_iter() {
        state.push(&buffer, progress);
    }
    state.finish()
}
