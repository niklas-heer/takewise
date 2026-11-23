//! The speech model: where it lives, whether it is there, and installing it.
//!
//! The model is not bundled (1.5 GB). [`install`] downloads it over HTTPS
//! into the models directory ([`crate::whisper::models_dir`]): first into
//! `<file>.part`, resuming from that file's size with an HTTP `Range` request
//! after a dropped connection or an earlier run, then verifies the size and
//! SHA-256 pinned in [`PINNED`] and only then renames it into place. A file
//! that does not match is deleted and never installed.
//!
//! Each request reads the body for at most [`REQUEST_BUDGET`], so a stalled
//! connection is noticed; the download then goes on with a new request from
//! where it stopped. An attempt that made no progress counts as a failure,
//! and after [`Download::retries`] such failures in a row the install stops,
//! keeping the `.part` file for the next run.

use std::fmt::Write as _;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read as _, Seek as _, SeekFrom, Write as _};
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;
use sha2::{Digest as _, Sha256};

/// A model file Takewise knows how to download and verify.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ModelSpec {
    /// The file name in the models directory, as `[check] whisper_model` names it.
    pub file: String,
    pub url: String,
    /// Lowercase hex.
    pub sha256: String,
    /// In bytes.
    pub size: u64,
}

/// The default model, `ggml-large-v3-turbo.bin` from whisper.cpp's model
/// repository on Hugging Face. Its SHA-256 and size match both the file
/// listing of `ggerganov/whisper.cpp` (the LFS object id) and the copy used
/// to tune the checks (verified 2026-11-23).
pub const PINNED: PinnedModel = PinnedModel {
    file: "ggml-large-v3-turbo.bin",
    url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-large-v3-turbo.bin",
    sha256: "1fc70f774d38eb169993ac391eea357ef47c88757ef72ee5943879b7e8e2bc69",
    size: 1_624_555_275,
};

/// A [`ModelSpec`] as a constant.
#[derive(Debug, Clone, Copy)]
pub struct PinnedModel {
    pub file: &'static str,
    pub url: &'static str,
    pub sha256: &'static str,
    pub size: u64,
}

impl PinnedModel {
    pub fn spec(&self) -> ModelSpec {
        ModelSpec {
            file: self.file.to_owned(),
            url: self.url.to_owned(),
            sha256: self.sha256.to_owned(),
            size: self.size,
        }
    }
}

/// The pinned download for a model file name, if Takewise has one.
pub fn pinned(file: &str) -> Option<ModelSpec> {
    (file == PINNED.file).then(|| PINNED.spec())
}

/// How long one request may read the body before the download goes on with
/// a new request; also the longest a stalled connection goes unnoticed.
pub const REQUEST_BUDGET: Duration = Duration::from_secs(60);
/// How long connecting and receiving the response headers may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// Failed attempts in a row (without progress) before the install stops.
pub const RETRIES: u32 = 3;
/// How long to wait before trying again after a failed attempt.
const RETRY_PAUSE: Duration = Duration::from_secs(2);
/// The read and write size while downloading and hashing.
const CHUNK: usize = 256 * 1024;

/// Where a model stands in a models directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "kebab-case")]
pub enum ModelState {
    /// Neither the model nor a partial download.
    Missing,
    /// An interrupted download, resumed by the next install.
    Partial { bytes: u64 },
    /// The model, with the expected size (its hash is checked on install).
    Installed,
    /// A file of that name, but not the expected size: another model or a
    /// damaged copy.
    WrongSize { bytes: u64 },
}

/// [`ModelState`] with the paths and the expected file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ModelStatus {
    pub path: PathBuf,
    #[serde(flatten)]
    pub state: ModelState,
    pub model: ModelSpec,
}

/// The model's path in `dir`.
pub fn model_path(dir: &Path, spec: &ModelSpec) -> PathBuf {
    dir.join(&spec.file)
}

/// The partial download's path in `dir`.
pub fn part_path(dir: &Path, spec: &ModelSpec) -> PathBuf {
    dir.join(format!("{}.part", spec.file))
}

/// Whether the model is in `dir`, judged by name and size.
pub fn status(dir: &Path, spec: &ModelSpec) -> ModelStatus {
    let path = model_path(dir, spec);
    let state = match fs::metadata(&path) {
        Ok(meta) if meta.len() == spec.size => ModelState::Installed,
        Ok(meta) => ModelState::WrongSize { bytes: meta.len() },
        Err(_) => match fs::metadata(part_path(dir, spec)) {
            Ok(meta) => ModelState::Partial { bytes: meta.len() },
            Err(_) => ModelState::Missing,
        },
    };
    ModelStatus {
        path,
        state,
        model: spec.clone(),
    }
}

/// How far a download is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Progress {
    pub bytes: u64,
    pub total: u64,
    /// While the downloaded file is verified, after the last byte.
    pub verifying: bool,
}

impl Progress {
    /// 0 to 100.
    pub fn percent(&self) -> f64 {
        if self.total == 0 {
            return 100.0;
        }
        (self.bytes as f64 / self.total as f64 * 100.0).min(100.0)
    }
}

/// Why an install did not finish.
#[derive(Debug, thiserror::Error)]
pub enum InstallError {
    #[error("{}: {source}", path.display())]
    Io { path: PathBuf, source: io::Error },
    #[error("the download stopped at {bytes} of {total} bytes ({reason}); run it again to resume")]
    Interrupted {
        bytes: u64,
        total: u64,
        reason: String,
    },
    #[error(
        "the downloaded file does not match: SHA-256 {actual}, expected {expected}; it was deleted, run it again"
    )]
    HashMismatch { expected: String, actual: String },
    #[error(
        "the server sent {actual} bytes, expected {expected}; the download was deleted, run it again"
    )]
    SizeMismatch { expected: u64, actual: u64 },
    #[error(
        "{} exists but is {bytes} bytes, not the {expected} of {file}; move it away and run it again",
        path.display()
    )]
    WrongFile {
        path: PathBuf,
        file: String,
        bytes: u64,
        expected: u64,
    },
    #[error("another Takewise is installing {0} right now")]
    Busy(String),
    #[error("the download was cancelled; run it again to resume")]
    Cancelled,
}

impl InstallError {
    /// A stable, kebab-case code for JSON output.
    pub fn code(&self) -> &'static str {
        match self {
            InstallError::Io { .. } => "io",
            InstallError::Interrupted { .. } => "download-interrupted",
            InstallError::HashMismatch { .. } => "hash-mismatch",
            InstallError::SizeMismatch { .. } => "size-mismatch",
            InstallError::WrongFile { .. } => "wrong-file",
            InstallError::Busy(_) => "busy",
            InstallError::Cancelled => "cancelled",
        }
    }
}

fn io_err(path: &Path) -> impl FnOnce(io::Error) -> InstallError + '_ {
    move |source| InstallError::Io {
        path: path.to_path_buf(),
        source,
    }
}

/// How [`install`] downloads.
#[derive(Debug, Clone)]
pub struct Download {
    /// Failed attempts in a row (without progress) before giving up.
    pub retries: u32,
    /// The body budget of one request ([`REQUEST_BUDGET`]).
    pub request_budget: Duration,
    /// The pause before trying again.
    pub retry_pause: Duration,
}

impl Default for Download {
    fn default() -> Self {
        Download {
            retries: RETRIES,
            request_budget: REQUEST_BUDGET,
            retry_pause: RETRY_PAUSE,
        }
    }
}

/// What [`install`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Installed {
    Downloaded,
    /// The model was there already, with the expected size.
    AlreadyInstalled,
}

/// Downloads `spec` into `dir`, resuming a partial download, and installs it
/// once its size and SHA-256 match. `progress` hears of every chunk and may
/// cancel (the partial download is kept).
pub fn install(
    dir: &Path,
    spec: &ModelSpec,
    options: &Download,
    mut progress: impl FnMut(Progress) -> ControlFlow<()>,
) -> Result<Installed, InstallError> {
    let path = model_path(dir, spec);
    match fs::metadata(&path) {
        Ok(meta) if meta.len() == spec.size => return Ok(Installed::AlreadyInstalled),
        Ok(meta) => {
            return Err(InstallError::WrongFile {
                path,
                file: spec.file.clone(),
                bytes: meta.len(),
                expected: spec.size,
            });
        }
        Err(_) => {}
    }
    fs::create_dir_all(dir).map_err(io_err(dir))?;
    let part = part_path(dir, spec);
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&part)
        .map_err(io_err(&part))?;
    match file.try_lock() {
        Ok(()) => {}
        Err(fs::TryLockError::WouldBlock) => return Err(InstallError::Busy(spec.file.clone())),
        Err(fs::TryLockError::Error(err)) => return Err(io_err(&part)(err)),
    }

    let agent = agent(options);
    let mut failures = 0;
    loop {
        let have = file.metadata().map_err(io_err(&part))?.len();
        if have > spec.size {
            file.set_len(0).map_err(io_err(&part))?;
            continue;
        }
        if have == spec.size {
            break;
        }
        match fetch(&agent, spec, &mut file, &part, have, &mut progress) {
            Ok(()) => {}
            Err(Fetch::Cancelled) => return Err(InstallError::Cancelled),
            Err(Fetch::Local(err)) => return Err(err),
            Err(Fetch::Remote(reason)) => {
                let now = file.metadata().map_err(io_err(&part))?.len();
                if now > have {
                    failures = 0;
                } else {
                    failures += 1;
                }
                if failures > options.retries {
                    return Err(InstallError::Interrupted {
                        bytes: now,
                        total: spec.size,
                        reason,
                    });
                }
                if now <= have {
                    std::thread::sleep(options.retry_pause);
                }
            }
        }
    }

    if progress(Progress {
        bytes: spec.size,
        total: spec.size,
        verifying: true,
    })
    .is_break()
    {
        return Err(InstallError::Cancelled);
    }
    let actual = sha256_of(&mut file).map_err(io_err(&part))?;
    if actual != spec.sha256.to_ascii_lowercase() {
        drop(file);
        let _ = fs::remove_file(&part);
        return Err(InstallError::HashMismatch {
            expected: spec.sha256.clone(),
            actual,
        });
    }
    file.sync_all().map_err(io_err(&part))?;
    drop(file);
    fs::rename(&part, &path).map_err(io_err(&path))?;
    if let Ok(dir) = File::open(dir) {
        let _ = dir.sync_all();
    }
    Ok(Installed::Downloaded)
}

fn agent(options: &Download) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_connect(Some(CONNECT_TIMEOUT))
        .timeout_recv_response(Some(CONNECT_TIMEOUT))
        .timeout_recv_body(Some(options.request_budget))
        .user_agent(concat!("takewise/", env!("CARGO_PKG_VERSION")))
        .build()
        .into()
}

enum Fetch {
    /// The network or the server: worth trying again.
    Remote(String),
    /// The disk: not worth trying again.
    Local(InstallError),
    Cancelled,
}

/// One request: appends what the server sends from `have` on to `file`.
fn fetch(
    agent: &ureq::Agent,
    spec: &ModelSpec,
    file: &mut File,
    part: &Path,
    have: u64,
    progress: &mut impl FnMut(Progress) -> ControlFlow<()>,
) -> Result<(), Fetch> {
    let mut request = agent.get(&spec.url);
    if have > 0 {
        request = request.header("Range", format!("bytes={have}-"));
    }
    let response = match request.call() {
        Ok(response) => response,
        Err(ureq::Error::StatusCode(416)) => {
            // The range starts at or past the end: the partial file is not a
            // prefix of what the server has. Start over.
            file.set_len(0)
                .map_err(|err| Fetch::Local(io_err(part)(err)))?;
            return Err(Fetch::Remote("the server refused to resume".to_owned()));
        }
        Err(err) => return Err(Fetch::Remote(err.to_string())),
    };
    let mut bytes = match response.status().as_u16() {
        206 => have,
        200 => {
            // The server ignored the range and sends the whole file.
            file.set_len(0)
                .map_err(|err| Fetch::Local(io_err(part)(err)))?;
            0
        }
        status => return Err(Fetch::Remote(format!("HTTP status {status}"))),
    };
    file.seek(SeekFrom::Start(bytes))
        .map_err(|err| Fetch::Local(io_err(part)(err)))?;
    let mut body = response.into_body().into_reader();
    let mut buffer = vec![0; CHUNK];
    loop {
        let read = match body.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => read,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(Fetch::Remote(err.to_string())),
        };
        if bytes + read as u64 > spec.size {
            let actual = bytes + read as u64;
            drop(body);
            let _ = file.set_len(0);
            let _ = fs::remove_file(part);
            return Err(Fetch::Local(InstallError::SizeMismatch {
                expected: spec.size,
                actual,
            }));
        }
        file.write_all(&buffer[..read])
            .map_err(|err| Fetch::Local(io_err(part)(err)))?;
        bytes += read as u64;
        if progress(Progress {
            bytes,
            total: spec.size,
            verifying: false,
        })
        .is_break()
        {
            let _ = file.flush();
            return Err(Fetch::Cancelled);
        }
    }
    file.flush()
        .map_err(|err| Fetch::Local(io_err(part)(err)))?;
    if bytes < spec.size {
        return Err(Fetch::Remote(format!(
            "the connection closed at {bytes} of {} bytes",
            spec.size
        )));
    }
    Ok(())
}

/// The SHA-256 of `file` from its start, as lowercase hex.
fn sha256_of(file: &mut File) -> io::Result<String> {
    file.seek(SeekFrom::Start(0))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0; CHUNK];
    loop {
        match file.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => hasher.update(&buffer[..read]),
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(err) => return Err(err),
        }
    }
    Ok(hasher
        .finalize()
        .iter()
        .fold(String::with_capacity(64), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        }))
}

#[cfg(test)]
pub(crate) mod fixture {
    //! A local HTTP server standing in for Hugging Face: it serves one file,
    //! honours `Range`, can cut a response short, and logs each request's
    //! range.

    use std::io::{BufRead as _, BufReader, Write as _};
    use std::net::{Shutdown, TcpListener, TcpStream};
    use std::sync::{Arc, Mutex};

    /// What the server does with one request (the last one repeats).
    #[derive(Debug, Clone, Copy)]
    pub enum Reply {
        /// Serve the request, honouring `Range`.
        Serve,
        /// Promise the rest but close the connection after this many bytes.
        CutAfter(usize),
        /// Ignore `Range` and send the whole file with 200.
        IgnoreRange,
        /// Redirect to the same server's `/file`.
        Redirect,
        /// Answer 503.
        Unavailable,
    }

    pub struct Server {
        pub url: String,
        /// The `Range` header of each request (`None` when it had none).
        pub ranges: Arc<Mutex<Vec<Option<String>>>>,
    }

    pub fn serve(body: Vec<u8>, replies: Vec<Reply>) -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/model.bin", listener.local_addr().unwrap());
        let ranges = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&ranges);
        std::thread::spawn(move || {
            for (n, stream) in listener.incoming().enumerate() {
                let Ok(stream) = stream else { break };
                let reply = replies[n.min(replies.len() - 1)];
                handle(stream, &body, reply, &log);
            }
        });
        Server { url, ranges }
    }

    fn handle(mut stream: TcpStream, body: &[u8], reply: Reply, log: &Mutex<Vec<Option<String>>>) {
        let mut range = None;
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                break;
            }
            if let Some((name, value)) = line.split_once(':')
                && name.eq_ignore_ascii_case("range")
            {
                range = Some(value.trim().to_owned());
            }
        }
        log.lock().unwrap().push(range.clone());
        let start: usize = range
            .as_deref()
            .and_then(|r| r.strip_prefix("bytes="))
            .and_then(|r| r.strip_suffix('-'))
            .and_then(|r| r.parse().ok())
            .unwrap_or(0);
        let total = body.len();
        let _ = match reply {
            Reply::Redirect => write!(
                stream,
                "HTTP/1.1 302 Found\r\nLocation: /file\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            ),
            Reply::Unavailable => write!(
                stream,
                "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            ),
            Reply::IgnoreRange => write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {total}\r\nConnection: close\r\n\r\n"
            )
            .and_then(|()| stream.write_all(body)),
            Reply::Serve | Reply::CutAfter(_) if start >= total && total > 0 => write!(
                stream,
                "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */{total}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            ),
            Reply::Serve | Reply::CutAfter(_) => {
                let rest = &body[start..];
                let head = if range.is_some() {
                    format!(
                        "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {start}-{}/{total}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        total - 1,
                        rest.len()
                    )
                } else {
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {total}\r\nConnection: close\r\n\r\n"
                    )
                };
                let sent = match reply {
                    Reply::CutAfter(n) => &rest[..n.min(rest.len())],
                    _ => rest,
                };
                stream
                    .write_all(head.as_bytes())
                    .and_then(|()| stream.write_all(sent))
            }
        };
        let _ = stream.flush();
        let _ = stream.shutdown(Shutdown::Both);
    }
}

#[cfg(test)]
mod tests {
    use super::fixture::{Reply, serve};
    use super::*;

    /// Deterministic bytes standing in for a model.
    fn model_bytes(len: usize) -> Vec<u8> {
        let mut state: u32 = 7;
        (0..len)
            .map(|_| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (state >> 24) as u8
            })
            .collect()
    }

    fn spec_for(url: &str, body: &[u8]) -> ModelSpec {
        let hash = Sha256::digest(body)
            .iter()
            .fold(String::new(), |mut hex, byte| {
                let _ = write!(hex, "{byte:02x}");
                hex
            });
        ModelSpec {
            file: "ggml-test.bin".to_owned(),
            url: url.to_owned(),
            sha256: hash,
            size: body.len() as u64,
        }
    }

    fn once() -> Download {
        Download {
            retries: 0,
            request_budget: Duration::from_secs(10),
            retry_pause: Duration::ZERO,
        }
    }

    fn go_on(_: Progress) -> ControlFlow<()> {
        ControlFlow::Continue(())
    }

    #[test]
    fn the_pinned_model_is_large_v3_turbo_from_hugging_face() {
        let spec = pinned("ggml-large-v3-turbo.bin").unwrap();
        assert!(
            spec.url
                .starts_with("https://huggingface.co/ggerganov/whisper.cpp/")
        );
        assert_eq!(spec.sha256.len(), 64);
        assert_eq!(spec.size, 1_624_555_275);
        assert_eq!(pinned("ggml-base.bin"), None);
    }

    #[test]
    fn downloads_and_verifies() {
        let body = model_bytes(1_000_000);
        let server = serve(body.clone(), vec![Reply::Redirect, Reply::Serve]);
        let spec = spec_for(&server.url, &body);
        let dir = tempfile::tempdir().unwrap();
        let mut seen = Vec::new();
        let done = install(dir.path(), &spec, &once(), |p| {
            seen.push(p);
            ControlFlow::Continue(())
        })
        .unwrap();
        assert_eq!(done, Installed::Downloaded);
        assert_eq!(fs::read(model_path(dir.path(), &spec)).unwrap(), body);
        assert!(!part_path(dir.path(), &spec).exists());
        let last = seen.last().unwrap();
        assert!(last.verifying && (last.percent() - 100.0).abs() < 1e-9);
        assert!(seen.iter().any(|p| !p.verifying && p.bytes < p.total));
        assert!(seen.windows(2).all(|w| w[0].bytes <= w[1].bytes));

        let again = install(dir.path(), &spec, &once(), go_on).unwrap();
        assert_eq!(again, Installed::AlreadyInstalled);
    }

    #[test]
    fn resumes_after_cut() {
        let body = model_bytes(600_000);
        let cut = 250_000;
        let server = serve(
            body.clone(),
            vec![Reply::CutAfter(cut), Reply::Unavailable, Reply::Serve],
        );
        let spec = spec_for(&server.url, &body);
        let dir = tempfile::tempdir().unwrap();

        let err = install(dir.path(), &spec, &once(), go_on).unwrap_err();
        assert!(
            matches!(err, InstallError::Interrupted { bytes, .. } if bytes == cut as u64),
            "{err}"
        );
        assert_eq!(
            status(dir.path(), &spec).state,
            ModelState::Partial { bytes: cut as u64 }
        );

        install(dir.path(), &spec, &once(), go_on).unwrap();
        assert_eq!(fs::read(model_path(dir.path(), &spec)).unwrap(), body);
        assert_eq!(
            *server.ranges.lock().unwrap(),
            vec![
                None,
                Some(format!("bytes={cut}-")),
                Some(format!("bytes={cut}-"))
            ]
        );
    }

    #[test]
    fn resumes_within_one_run_while_it_makes_progress() {
        let body = model_bytes(600_000);
        let server = serve(
            body.clone(),
            vec![
                Reply::CutAfter(100_000),
                Reply::CutAfter(100_000),
                Reply::Serve,
            ],
        );
        let spec = spec_for(&server.url, &body);
        let dir = tempfile::tempdir().unwrap();
        install(dir.path(), &spec, &once(), go_on).unwrap();
        assert_eq!(fs::read(model_path(dir.path(), &spec)).unwrap(), body);
        assert_eq!(server.ranges.lock().unwrap().len(), 3);
    }

    #[test]
    fn a_server_ignoring_the_range_starts_over() {
        let body = model_bytes(300_000);
        let server = serve(body.clone(), vec![Reply::IgnoreRange]);
        let spec = spec_for(&server.url, &body);
        let dir = tempfile::tempdir().unwrap();
        fs::write(part_path(dir.path(), &spec), &body[..1000]).unwrap();
        install(dir.path(), &spec, &once(), go_on).unwrap();
        assert_eq!(fs::read(model_path(dir.path(), &spec)).unwrap(), body);
    }

    #[test]
    fn rejects_bad_hash() {
        let body = model_bytes(200_000);
        let server = serve(body.clone(), vec![Reply::Serve]);
        let mut spec = spec_for(&server.url, &body);
        spec.sha256 = "0".repeat(64);
        let dir = tempfile::tempdir().unwrap();
        let err = install(dir.path(), &spec, &once(), go_on).unwrap_err();
        assert!(matches!(err, InstallError::HashMismatch { .. }), "{err}");
        assert_eq!(err.code(), "hash-mismatch");
        assert!(!model_path(dir.path(), &spec).exists());
        assert!(!part_path(dir.path(), &spec).exists());
        assert_eq!(status(dir.path(), &spec).state, ModelState::Missing);
    }

    #[test]
    fn rejects_a_larger_download() {
        let body = model_bytes(200_000);
        let server = serve(body.clone(), vec![Reply::Serve]);
        let mut spec = spec_for(&server.url, &body);
        spec.size = 100_000;
        let dir = tempfile::tempdir().unwrap();
        let err = install(dir.path(), &spec, &once(), go_on).unwrap_err();
        assert!(matches!(err, InstallError::SizeMismatch { .. }), "{err}");
        assert!(!part_path(dir.path(), &spec).exists());
    }

    #[test]
    fn cancelling_keeps_the_partial_download() {
        let body = model_bytes(2_000_000);
        let server = serve(body.clone(), vec![Reply::Serve]);
        let spec = spec_for(&server.url, &body);
        let dir = tempfile::tempdir().unwrap();
        let err = install(dir.path(), &spec, &once(), |_| ControlFlow::Break(())).unwrap_err();
        assert!(matches!(err, InstallError::Cancelled), "{err}");
        assert!(matches!(
            status(dir.path(), &spec).state,
            ModelState::Partial { bytes } if bytes > 0
        ));
    }

    #[test]
    fn a_foreign_file_of_that_name_is_left_alone() {
        let spec = spec_for("http://127.0.0.1:9/never", b"model");
        let dir = tempfile::tempdir().unwrap();
        fs::write(model_path(dir.path(), &spec), b"something else").unwrap();
        let err = install(dir.path(), &spec, &once(), go_on).unwrap_err();
        assert_eq!(err.code(), "wrong-file");
        assert_eq!(
            fs::read(model_path(dir.path(), &spec)).unwrap(),
            b"something else"
        );
    }

    #[test]
    fn status_reports_missing_and_present() {
        let body = model_bytes(1000);
        let spec = spec_for("http://127.0.0.1:9/never", &body);
        let dir = tempfile::tempdir().unwrap();
        let missing = status(dir.path(), &spec);
        assert_eq!(missing.state, ModelState::Missing);
        assert_eq!(missing.path, dir.path().join("ggml-test.bin"));

        fs::write(part_path(dir.path(), &spec), &body[..10]).unwrap();
        assert_eq!(
            status(dir.path(), &spec).state,
            ModelState::Partial { bytes: 10 }
        );

        fs::write(model_path(dir.path(), &spec), &body[..999]).unwrap();
        assert_eq!(
            status(dir.path(), &spec).state,
            ModelState::WrongSize { bytes: 999 }
        );

        fs::write(model_path(dir.path(), &spec), &body).unwrap();
        let present = status(dir.path(), &spec);
        assert_eq!(present.state, ModelState::Installed);
        let json = serde_json::to_value(&present).unwrap();
        assert_eq!(json["state"], "installed");
        assert_eq!(json["model"]["file"], "ggml-test.bin");
    }

    #[test]
    fn a_second_install_at_once_is_refused() {
        let body = model_bytes(1000);
        let spec = spec_for("http://127.0.0.1:9/never", &body);
        let dir = tempfile::tempdir().unwrap();
        let holder = File::create(part_path(dir.path(), &spec)).unwrap();
        holder.lock().unwrap();
        let err = install(dir.path(), &spec, &once(), go_on).unwrap_err();
        assert_eq!(err.code(), "busy");
    }
}
