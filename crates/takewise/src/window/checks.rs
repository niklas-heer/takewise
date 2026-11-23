//! Take checks off the UI thread: one worker checks queued takes in turn
//! (whisper-cli takes seconds per take) and reports back, and the window
//! shows "checking…" for a take until its check is in the ledger.
//!
//! Checks never compete with recording: while a take records the worker
//! holds its next check, and the tools of a check already running
//! (whisper-cli, ffmpeg) are stopped (SIGSTOP) and continued once the take
//! is committed. Quitting kills them, so none outlives the window and their
//! temporary files are removed; the cancelled check records nothing.
//!
//! The tools are this process's direct children whose arguments name them
//! and the worker's tag (`pkill -P … -f`): the transcriber in
//! `takewise-core` starts them in a temporary directory whose name carries
//! the tag ([`worker_tag`]), so two windows in one process never stop or
//! kill each other's checks.
//!
//! A crash or force-quit during a take leaves them stopped for good, so the
//! window and `takewise check` first kill the tools an earlier Takewise left
//! behind: orphans whose arguments name the transcriber's temporary
//! directory ([`reap_orphaned_tools`]).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use takewise_core::{self as tw, Project, Snapshot, TakeRef, Transcriber, Transcript};

/// How often the tools are stopped again while a take records, so one
/// started meanwhile (whisper-cli after ffmpeg) is stopped too.
const STOP_POLL: Duration = Duration::from_millis(200);
/// How often the tools are killed again while a cancelled check ends.
const KILL_POLL: Duration = Duration::from_millis(20);
/// How long quitting waits for a cancelled check to clean up.
const CANCEL_WAIT: Duration = Duration::from_secs(2);

/// A transcriber the worker thread can own.
pub type SharedTranscriber = Arc<dyn Transcriber + Send + Sync>;

/// What checks the takes: the transcriber, and the executables it runs as
/// child processes of this one (stopped while a take records, killed on
/// quit).
pub struct Checker {
    pub transcriber: SharedTranscriber,
    pub tools: Vec<PathBuf>,
    /// The temporary-directory prefix the transcriber's tools name
    /// ([`worker_tag`]): only tools whose arguments name it are signalled.
    pub tag: String,
}

/// A temporary-directory prefix no other check worker in this process
/// uses: the transcriber's prefix with a worker number.
pub fn worker_tag() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    format!("{}w{n}-", tw::whisper::TEMP_PREFIX)
}

/// A finished check: its verdict, or why the take could not be checked.
pub struct Done {
    pub take: TakeRef,
    /// The line as the check took it when it began (`None` when the line
    /// could not be read).
    pub snapshot: Option<Snapshot>,
    pub result: Result<tw::Verdict, String>,
}

#[derive(Default)]
struct Flags {
    /// A take records: no check starts, and running tools are stopped.
    paused: bool,
    /// The window quits: no check starts, and running tools are killed.
    cancelled: bool,
    /// While the transcriber runs (and with it maybe its tools): whether
    /// the checks were paused meanwhile.
    transcribing: Option<Paused>,
}

#[derive(Clone, Copy, Default)]
struct Paused(bool);

/// What the window, the worker and the tools' signaller share.
#[derive(Default)]
struct Control {
    flags: Mutex<Flags>,
    changed: Condvar,
}

impl Control {
    fn lock(&self) -> MutexGuard<'_, Flags> {
        self.flags.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn update(&self, f: impl FnOnce(&mut Flags)) {
        f(&mut self.lock());
        self.changed.notify_all();
    }

    /// Waits while paused; the flags once the checks may go on or were
    /// cancelled.
    fn wait_open(&self) -> MutexGuard<'_, Flags> {
        self.changed
            .wait_while(self.lock(), |f| f.paused && !f.cancelled)
            .unwrap_or_else(PoisonError::into_inner)
    }
}

/// The transcriber as the worker runs it: it starts only while the checks
/// may go on, transcribes again when a transcription failed while paused (a
/// stopped tool can outlast its timeout), and never returns once cancelled,
/// so the check records nothing.
struct Held {
    inner: SharedTranscriber,
    control: Arc<Control>,
}

impl Transcriber for Held {
    fn transcribe(&self, wav: &Path) -> Transcript {
        loop {
            {
                let mut flags = self.control.wait_open();
                if flags.cancelled {
                    drop(flags);
                    abandon();
                }
                flags.transcribing = Some(Paused(false));
            }
            let transcript = self.inner.transcribe(wav);
            let (cancelled, Paused(paused_during)) = {
                let mut flags = self.control.lock();
                let paused = flags.transcribing.take().unwrap_or_default();
                (flags.cancelled, paused)
            };
            self.control.changed.notify_all();
            if cancelled {
                abandon();
            }
            match transcript {
                Transcript::Failed(_) if paused_during => {}
                transcript => return transcript,
            }
        }
    }

    fn model(&self) -> String {
        self.inner.model()
    }
}

/// Parks the worker for good: its check was cancelled, and returning would
/// record it.
fn abandon() -> ! {
    loop {
        std::thread::park();
    }
}

/// Stops the tools while paused, continues them after, and kills them once
/// cancelled; returns when cancelled and no transcription runs.
fn signal_tools(control: &Control, tools: &[PathBuf], tag: &str) {
    let mut stopped = false;
    let mut flags = control.lock();
    loop {
        if flags.cancelled {
            if flags.transcribing.is_none() {
                return;
            }
            drop(flags);
            signal(tools, tag, "KILL");
            flags = control.lock();
            flags = control
                .changed
                .wait_timeout(flags, KILL_POLL)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        } else if flags.paused && flags.transcribing.is_some() {
            flags.transcribing = Some(Paused(true));
            drop(flags);
            signal(tools, tag, "STOP");
            stopped = true;
            flags = control.lock();
            flags = control
                .changed
                .wait_timeout(flags, STOP_POLL)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        } else if stopped && !flags.paused {
            drop(flags);
            signal(tools, tag, "CONT");
            stopped = false;
            flags = control.lock();
        } else {
            flags = control
                .changed
                .wait(flags)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }
}

/// Sends `signal` (such as `STOP`) to this process's direct children whose
/// arguments name one of `tools` (also while `nice` still starts one) and
/// then a file under a directory named with `tag`.
fn signal(tools: &[PathBuf], tag: &str, signal: &str) {
    let pattern: Vec<String> = tools
        .iter()
        .map(|tool| escape_regex(&tool.to_string_lossy()))
        .collect();
    let pattern = format!("({}).*/{}", pattern.join("|"), escape_regex(tag));
    let _ = Command::new("/usr/bin/pkill")
        .arg(format!("-{signal}"))
        .args(["-P", &std::process::id().to_string(), "-f"])
        .arg(pattern)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// `text` as an extended regular expression matching it literally.
fn escape_regex(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for c in text.chars() {
        if "\\.^$|?*+()[]{}".contains(c) {
            escaped.push('\\');
        }
        escaped.push(c);
    }
    escaped
}

/// A process as `ps` lists it.
#[derive(Debug)]
struct Process {
    pid: u32,
    ppid: u32,
    command: String,
}

/// Kills the tools (whisper-cli, ffmpeg) that a Takewise which crashed or
/// was force-quit left behind, maybe stopped: continued, then killed.
/// Returns how many there were.
pub fn reap_orphaned_tools(tools: &[PathBuf]) -> usize {
    let Ok(out) = Command::new("/bin/ps")
        .args(["-axww", "-o", "pid=,ppid=,command="])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
    else {
        return 0;
    };
    if !out.status.success() {
        return 0;
    }
    let processes = parse_ps(&String::from_utf8_lossy(&out.stdout));
    reap(&processes, tools, |signal, pids| {
        let _ = Command::new("/bin/kill")
            .arg(format!("-{signal}"))
            .args(pids.iter().map(u32::to_string))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    })
}

/// Sends the orphaned tools among `processes` `CONT` (a stopped one would
/// otherwise only die once continued), then `KILL`; returns how many.
fn reap(
    processes: &[Process],
    tools: &[PathBuf],
    mut send: impl FnMut(&'static str, &[u32]),
) -> usize {
    let pids = orphaned_tools(processes, tools);
    if !pids.is_empty() {
        send("CONT", &pids);
        send("KILL", &pids);
    }
    pids.len()
}

/// The processes that run one of `tools` (by path, or by name when it was
/// found elsewhere then) on a file in the transcriber's temporary directory,
/// and whose parent is gone (adopted by launchd, or no longer listed).
fn orphaned_tools(processes: &[Process], tools: &[PathBuf]) -> Vec<u32> {
    let marker = format!("/{}", tw::whisper::TEMP_PREFIX);
    processes
        .iter()
        .filter(|p| p.ppid == 1 || !processes.iter().any(|q| q.pid == p.ppid))
        .filter(|p| p.command.contains(&marker) && runs_tool(&p.command, tools))
        .map(|p| p.pid)
        .collect()
}

/// Whether `command` (as `ps` shows it) runs one of `tools`.
fn runs_tool(command: &str, tools: &[PathBuf]) -> bool {
    let program = command.split_whitespace().next().map(Path::new);
    tools.iter().any(|tool| {
        let path = tool.to_string_lossy();
        command
            .strip_prefix(path.as_ref())
            .is_some_and(|rest| rest.is_empty() || rest.starts_with(' '))
            || tool
                .file_name()
                .is_some_and(|name| program.and_then(Path::file_name) == Some(name))
    })
}

/// The lines of `ps -o pid=,ppid=,command=`.
fn parse_ps(out: &str) -> Vec<Process> {
    out.lines()
        .filter_map(|line| {
            let (pid, rest) = line.trim_start().split_once(char::is_whitespace)?;
            let (parent, command) = rest.trim_start().split_once(char::is_whitespace)?;
            Some(Process {
                pid: pid.parse().ok()?,
                ppid: parent.parse().ok()?,
                command: command.trim_start().to_owned(),
            })
        })
        .collect()
}

pub struct Checks {
    /// `None` when the worker could not be started.
    jobs: Option<Sender<TakeRef>>,
    control: Arc<Control>,
    /// Whether a signaller stops and kills the tools.
    signalled: bool,
    done: Receiver<Done>,
    /// Queued or running, each take once.
    pending: Vec<TakeRef>,
    /// Every take checked this session with the line as its check took it
    /// and the `checker` that checked it, so a take that cannot be checked
    /// is not queued again and again, while one checked by other settings
    /// is.
    tried: Vec<(TakeRef, Snapshot, String)>,
    /// The `checker` this worker's checks record.
    checker: String,
}

impl Checks {
    /// Starts the worker; it ends once the `Checks` is dropped, abandoning
    /// a check in progress.
    pub fn new(dir: PathBuf, checker: Checker) -> Self {
        let Checker {
            transcriber,
            tools,
            tag,
        } = checker;
        let checker = tw::checker(env!("CARGO_PKG_VERSION"), &transcriber.model());
        let (jobs_tx, jobs_rx) = channel::<TakeRef>();
        let (done_tx, done_rx) = channel();
        let control = Arc::new(Control::default());
        let held = Held {
            inner: transcriber,
            control: Arc::clone(&control),
        };
        let worker_control = Arc::clone(&control);
        let worker = std::thread::Builder::new()
            .name("takewise-checks".to_owned())
            .spawn(move || {
                for take in jobs_rx {
                    if worker_control.wait_open().cancelled {
                        break;
                    }
                    let dequeued = Project::open(&dir)
                        .ok()
                        .and_then(|project| project.snapshot(&take.line));
                    let outcome = tw::check_take(&dir, &take, &held, env!("CARGO_PKG_VERSION"));
                    let (snapshot, result) = match outcome {
                        Ok(done) => (Some(done.snapshot), Ok(done.verdict)),
                        Err(err) => (dequeued, Err(err.to_string())),
                    };
                    let done = Done {
                        take,
                        snapshot,
                        result,
                    };
                    if done_tx.send(done).is_err() {
                        break;
                    }
                }
            });
        let signaller_control = Arc::clone(&control);
        let signalled = !tools.is_empty()
            && std::thread::Builder::new()
                .name("takewise-check-tools".to_owned())
                .spawn(move || signal_tools(&signaller_control, &tools, &tag))
                .is_ok();
        Self {
            jobs: worker.is_ok().then_some(jobs_tx),
            control,
            signalled,
            done: done_rx,
            pending: Vec::new(),
            tried: Vec::new(),
            checker,
        }
    }

    /// Pauses the checks (the next one waits, a running one's tools are
    /// stopped), or lets them go on.
    pub fn set_paused(&self, paused: bool) {
        self.control.update(|flags| {
            if let Some(during) = flags.transcribing.as_mut().filter(|_| paused) {
                *during = Paused(true);
            }
            flags.paused = paused;
        });
    }

    /// Cancels the checks as the window quits: none starts, a running one
    /// records nothing and its tools are killed; waits briefly until they
    /// are gone and have removed their temporary files.
    pub fn cancel(&self) {
        self.control.update(|flags| flags.cancelled = true);
        if self.signalled {
            drop(
                self.control
                    .changed
                    .wait_timeout_while(self.control.lock(), CANCEL_WAIT, |f| {
                        f.transcribing.is_some()
                    })
                    .unwrap_or_else(PoisonError::into_inner),
            );
        }
    }

    /// Queues a check of `take`, whose line `snapshot` has now, unless it
    /// is queued or running (its check takes the line as it is when it
    /// begins; one that turns out stale is queued again once done) or was
    /// checked against that line by this worker's checker before.
    pub fn queue(&mut self, take: &TakeRef, snapshot: &Snapshot) {
        if self.pending.contains(take)
            || self
                .tried
                .iter()
                .any(|(t, s, by)| t == take && s.matches(snapshot) && *by == self.checker)
        {
            return;
        }
        let Some(jobs) = &self.jobs else {
            return;
        };
        if jobs.send(take.clone()).is_ok() {
            self.pending.push(take.clone());
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
            self.pending.retain(|t| *t != d.take);
            if let Some(snapshot) = &d.snapshot {
                self.tried
                    .push((d.take.clone(), snapshot.clone(), self.checker.clone()));
            }
        }
        done
    }
}

impl Drop for Checks {
    /// Cancels the checks, so the worker and the signaller end with the
    /// window.
    fn drop(&mut self) {
        self.control.update(|flags| flags.cancelled = true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WHISPER: &str = "/opt/homebrew/bin/whisper-cli";
    const FFMPEG: &str = "/opt/homebrew/bin/ffmpeg";
    const TMP: &str = "/var/folders/xy/T/takewise-whisper-a1B2c3";

    fn tools() -> Vec<PathBuf> {
        vec![PathBuf::from(WHISPER), PathBuf::from(FFMPEG)]
    }

    fn process(pid: u32, parent: u32, command: &str) -> Process {
        Process {
            pid,
            ppid: parent,
            command: command.to_owned(),
        }
    }

    fn whisper_of(dir: &str) -> String {
        format!(
            "{WHISPER} -m /models/ggml-base.bin -f {dir}/take.wav -l en -ojf -of {dir}/take -np"
        )
    }

    fn ffmpeg_of(dir: &str) -> String {
        format!(
            "{FFMPEG} -nostdin -y -loglevel error -i /p/takes/a/001.wav -ar 16000 -ac 1 -c:a pcm_s16le {dir}/take.wav"
        )
    }

    /// A child of this process whose arguments name a file under `tag`,
    /// as a check's tools name their temporary directory.
    fn child_tagged(tag: &str) -> std::process::Child {
        Command::new("/bin/sh")
            .args(["-c", "sleep 30; :"])
            .arg(format!("/var/folders/T/{tag}x1/take.wav"))
            .spawn()
            .unwrap()
    }

    fn stopped(pid: u32) -> bool {
        let out = Command::new("/bin/ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().starts_with('T')
    }

    #[test]
    fn a_worker_signals_only_its_own_tools() {
        let mine = worker_tag();
        let other = worker_tag();
        assert_ne!(mine, other);
        assert!(mine.starts_with(tw::whisper::TEMP_PREFIX), "{mine}");
        let mut own = child_tagged(&mine);
        let mut theirs = child_tagged(&other);
        let sh = [PathBuf::from("/bin/sh")];
        signal(&sh, &mine, "STOP");
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while !stopped(own.id()) && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let (own_stopped, theirs_stopped) = (stopped(own.id()), stopped(theirs.id()));
        for child in [&mut own, &mut theirs] {
            let _ = child.kill();
            let _ = child.wait();
        }
        assert!(own_stopped, "its own tool is stopped");
        assert!(!theirs_stopped, "another window's tool runs on");
    }

    #[test]
    fn orphaned_tools_of_a_takewise_check_are_found() {
        let list = vec![
            process(1, 0, "/sbin/launchd"),
            process(500, 1, &whisper_of(TMP)),
            process(501, 1, &ffmpeg_of(TMP)),
        ];
        assert_eq!(orphaned_tools(&list, &tools()), vec![500, 501]);
    }

    #[test]
    fn a_parent_missing_from_the_list_is_gone() {
        let list = vec![process(500, 777, &whisper_of(TMP))];
        assert_eq!(orphaned_tools(&list, &tools()), vec![500]);
    }

    #[test]
    fn tools_of_a_running_takewise_are_left_alone() {
        let list = vec![
            process(
                400,
                1,
                "/Applications/Takewise.app/Contents/MacOS/takewise record x",
            ),
            process(500, 400, &whisper_of(TMP)),
        ];
        assert_eq!(orphaned_tools(&list, &tools()), Vec::<u32>::new());
    }

    #[test]
    fn tools_without_the_takewise_temporary_directory_are_left_alone() {
        let list = vec![
            process(500, 1, &ffmpeg_of("/Users/me/Movies/export")),
            process(501, 1, &whisper_of("/tmp/other-app-1234")),
        ];
        assert_eq!(orphaned_tools(&list, &tools()), Vec::<u32>::new());
    }

    #[test]
    fn other_programs_naming_the_directory_are_left_alone() {
        let list = vec![
            process(500, 1, &format!("/bin/rm -rf {TMP}")),
            process(501, 1, &format!("/usr/bin/tail -f {TMP}/take.json")),
        ];
        assert_eq!(orphaned_tools(&list, &tools()), Vec::<u32>::new());
    }

    #[test]
    fn a_tool_found_elsewhere_now_is_still_matched_by_name() {
        let list = vec![process(
            500,
            1,
            &whisper_of(TMP).replace(WHISPER, "/usr/local/bin/whisper-cli"),
        )];
        assert_eq!(orphaned_tools(&list, &tools()), vec![500]);
    }

    #[test]
    fn a_tool_path_with_spaces_is_matched() {
        let tool = "/Users/me/my tools/whisper-cli";
        let list = vec![process(500, 1, &whisper_of(TMP).replace(WHISPER, tool))];
        assert_eq!(orphaned_tools(&list, &[PathBuf::from(tool)]), vec![500]);
    }

    #[test]
    fn ps_output_is_parsed() {
        let out = format!(
            "    1     0 /sbin/launchd\n  500     1 {}\n garbage\n\n",
            whisper_of(TMP)
        );
        let list = parse_ps(&out);
        assert_eq!(list.len(), 2);
        assert_eq!((list[1].pid, list[1].ppid), (500, 1));
        assert_eq!(list[1].command, whisper_of(TMP));
    }

    #[test]
    fn reaping_continues_then_kills_each_orphan() {
        let list = vec![
            process(500, 1, &whisper_of(TMP)),
            process(600, 1, "/usr/bin/vim notes.txt"),
            process(501, 1, &ffmpeg_of(TMP)),
        ];
        let mut sent = Vec::new();
        let reaped = reap(&list, &tools(), |signal, pids| {
            sent.push((signal, pids.to_vec()));
        });
        assert_eq!(reaped, 2);
        assert_eq!(
            sent,
            vec![("CONT", vec![500, 501]), ("KILL", vec![500, 501])]
        );
    }

    #[test]
    fn nothing_to_reap_sends_nothing() {
        let list = vec![process(600, 1, "/usr/bin/vim notes.txt")];
        let mut sent = 0;
        assert_eq!(reap(&list, &tools(), |_, _| sent += 1), 0);
        assert_eq!(sent, 0);
    }
}
