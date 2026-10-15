//! Supervisor for the `winsearch-embed` worker process.
//!
//! The supervisor owns the worker's lifetime on behalf of the engine and is
//! the worker's only client (one connection at a time, enforced by holding
//! [`Slot`] across a whole request). It:
//! - spawns the worker on the first embed request (never before —
//!   `EngineService::status` must stay spawn-free),
//! - handshakes every connection (`hello` doubles as the health ping and
//!   refreshes the cached `model_loaded`/`stub` flags),
//! - restarts with backoff after a transport failure and retries the
//!   in-flight job exactly once,
//! - never retries definitive worker answers (`model_not_available`,
//!   `unsupported`, ...).
//!
//! `EngineStatus.model_loaded` reads the cached flags: true when real weights
//! are resident **or** the worker serves deterministic stub vectors (test
//! setups). Production never sets the stub flag, so the overlay hint logic
//! is unaffected there.

use std::path::PathBuf;
use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tracing::{info, warn};
use ws_embed_protocol::{
    EmbedErrorCode, EmbedRequest, EmbedResponse, EMBED_PROTOCOL_VERSION, MAX_TEXTS_PER_JOB,
};
use ws_platform::{identity, pipe::PipeStream};
use ws_protocol::{read_message, write_message};

use crate::server::ENGINE_VERSION;

/// Base backoff before respawning a crashed worker; doubles per consecutive
/// failure, capped at [`MAX_BACKOFF`]. Fast enough to not stall queries,
/// slow enough to not hot-loop a poisoned worker.
const DEFAULT_BACKOFF_BASE: Duration = Duration::from_millis(250);
const MAX_BACKOFF: Duration = Duration::from_secs(5);
/// How long to wait for a freshly spawned worker's pipe to appear. Generous:
/// process start + DLL paging on a loaded machine takes seconds; the success
/// path returns on the first poll either way.
const SPAWN_WAIT: Duration = Duration::from_secs(30);
/// Connect retries inside one request. Covers the pipe-busy race: a readiness
/// probe (or a previous request) consumes the worker's current instance, and
/// the next instance appears a breath later — or seconds later on a loaded
/// machine. Bounded so a truly dead worker still surfaces as `Transport`
/// (restart path) instead of hanging the caller.
const CONNECT_RETRY: Duration = Duration::from_secs(15);
const CONNECT_POLL: Duration = Duration::from_millis(25);

#[derive(Debug, thiserror::Error)]
pub enum EmbedError {
    #[error("embed worker transport: {0}")]
    Transport(String),
    #[error("embed protocol violation: {0}")]
    Protocol(String),
    #[error("embed protocol version mismatch: {0}")]
    VersionMismatch(String),
    #[error("embedding model not available: {0}")]
    ModelNotAvailable(String),
    #[error("embedding failed: {0}")]
    EmbedFailed(String),
    #[error("unsupported by embed worker: {0}")]
    Unsupported(String),
    #[error("invalid embed request: {0}")]
    InvalidRequest(String),
    #[error("cannot start embed worker: {0}")]
    Spawn(String),
}

impl EmbedError {
    fn transport(e: impl std::fmt::Display) -> Self {
        Self::Transport(e.to_string())
    }

    fn from_worker(code: EmbedErrorCode, message: String) -> Self {
        match code {
            EmbedErrorCode::VersionMismatch => Self::VersionMismatch(message),
            EmbedErrorCode::InvalidRequest => Self::InvalidRequest(message),
            EmbedErrorCode::ModelNotAvailable => Self::ModelNotAvailable(message),
            EmbedErrorCode::EmbedFailed => Self::EmbedFailed(message),
            EmbedErrorCode::Unsupported => Self::Unsupported(message),
        }
    }
}

/// Snapshot of the worker's self-reported state (answers [`EmbedSupervisor::ping`]).
#[derive(Debug, Clone)]
pub struct WorkerSnapshot {
    pub model_loaded: bool,
    pub backend: Option<String>,
    pub stub: bool,
    pub dim: usize,
    pub uptime_secs: u64,
    pub jobs_served: u64,
}

#[derive(Debug, Clone, Default)]
pub struct EmbedStats {
    pub spawns: u64,
    pub restarts: u64,
    pub retries: u64,
    pub jobs: u64,
}

/// How to reach and launch the worker. Explicit config beats env beats
/// compiled defaults, so integration tests can run in parallel with unique
/// pipes and CLI args instead of mutating process-global env.
#[derive(Clone)]
pub struct EmbedConfig {
    pub worker_bin: PathBuf,
    pub pipe_name: String,
    pub worker_args: Vec<String>,
    pub backoff_base: Duration,
    /// Test hook: when set, backoff waits call this instead of blocking the
    /// thread, so tests assert the sleep schedule deterministically.
    /// `None` (production) blocks on the calling thread.
    pub sleeper: Option<std::sync::Arc<dyn Fn(Duration) + Send + Sync>>,
}

impl std::fmt::Debug for EmbedConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EmbedConfig")
            .field("worker_bin", &self.worker_bin)
            .field("pipe_name", &self.pipe_name)
            .field("worker_args", &self.worker_args)
            .field("backoff_base", &self.backoff_base)
            .field("sleeper_set", &self.sleeper.is_some())
            .finish()
    }
}

impl EmbedConfig {
    pub fn from_env_or_default() -> Self {
        let worker_bin = match std::env::var("WS_EMBED_BIN") {
            Ok(p) if !p.is_empty() => PathBuf::from(p),
            _ => default_worker_bin(),
        };
        let pipe_name = match std::env::var("WS_EMBED_PIPE") {
            Ok(p) if !p.is_empty() => p,
            _ => identity::embed_pipe_name().unwrap_or_else(|_| {
                // Identity lookup cannot realistically fail; a pid-suffixed
                // fallback keeps the supervisor usable if it ever does.
                format!(r"\\.\pipe\winsearch-embed-unknown-{}", std::process::id())
            }),
        };
        let backoff_base = match std::env::var("WS_EMBED_BACKOFF_MS") {
            Ok(v) => v
                .parse::<u64>()
                .map(Duration::from_millis)
                .unwrap_or(DEFAULT_BACKOFF_BASE),
            Err(_) => DEFAULT_BACKOFF_BASE,
        };
        Self {
            worker_bin,
            pipe_name,
            worker_args: Vec::new(),
            backoff_base,
            sleeper: None,
        }
    }
}

/// The worker ships next to the daemon (`winsearch-embed.exe` beside
/// `winsearchd.exe`, same layout as the installer target).
fn default_worker_bin() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
        .join(format!("winsearch-embed{}", std::env::consts::EXE_SUFFIX))
}

/// Owns the worker child (when we spawned it). Separate from the connection
/// mutex below so an admin/test kill lands even mid-request; the in-flight
/// job then fails over to the restart path.
struct Slot {
    child: Option<Child>,
}

pub struct EmbedSupervisor {
    config: EmbedConfig,
    /// Serializes whole requests: the worker sees one connection at a time.
    conn: Mutex<()>,
    slot: Mutex<Slot>,
    next_id: AtomicU64,
    failures: AtomicU32,
    model_loaded: AtomicBool,
    stub: AtomicBool,
    stats: Mutex<EmbedStats>,
}

impl EmbedSupervisor {
    pub fn new() -> Self {
        Self::with_config(EmbedConfig::from_env_or_default())
    }

    pub fn with_config(config: EmbedConfig) -> Self {
        Self {
            config,
            conn: Mutex::new(()),
            slot: Mutex::new(Slot { child: None }),
            next_id: AtomicU64::new(1),
            failures: AtomicU32::new(0),
            model_loaded: AtomicBool::new(false),
            stub: AtomicBool::new(false),
            stats: Mutex::new(EmbedStats::default()),
        }
    }

    pub fn worker_bin(&self) -> &PathBuf {
        &self.config.worker_bin
    }

    pub fn pipe_name(&self) -> &str {
        &self.config.pipe_name
    }

    /// Cached worker state: real weights resident, or stub vectors flowing.
    /// Updated by every handshake; never spawns a worker by itself.
    pub fn model_loaded(&self) -> bool {
        self.model_loaded.load(Ordering::Relaxed) || self.stub.load(Ordering::Relaxed)
    }

    pub fn stats(&self) -> EmbedStats {
        self.stats.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// True while our spawned child is alive (reaps it once it exits, e.g.
    /// after its idle timeout). Never probes the pipe: a foreign worker is
    /// not ours to report on.
    pub fn is_worker_running(&self) -> bool {
        let mut slot = self.slot.lock().unwrap_or_else(|p| p.into_inner());
        match slot.child.as_mut() {
            Some(child) => match child.try_wait() {
                Ok(None) => true,
                _ => {
                    slot.child = None;
                    false
                }
            },
            None => false,
        }
    }

    /// Kill our worker, if any, and forget it. The next request respawns.
    /// Test hook for the kill-mid-job test (and a future admin path).
    pub fn terminate_worker(&self) {
        let mut slot = self.slot.lock().unwrap_or_else(|p| p.into_inner());
        kill_locked(&mut slot);
    }

    /// Embed texts, one job per [`MAX_TEXTS_PER_JOB`] chunk, in order.
    /// Spawns the worker on first use; restarts it and retries once when the
    /// connection breaks mid-job.
    pub fn embed_texts(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
        if texts.is_empty() {
            return Err(EmbedError::InvalidRequest(
                "cannot embed an empty batch".into(),
            ));
        }
        let mut all = Vec::with_capacity(texts.len());
        for chunk in texts.chunks(MAX_TEXTS_PER_JOB) {
            let id = self.next_id.fetch_add(1, Ordering::Relaxed);
            let req = EmbedRequest::EmbedTexts {
                id,
                texts: chunk.to_vec(),
            };
            match self.transact(req) {
                Ok(EmbedResponse::Embeddings { vecs, .. }) => {
                    self.stats.lock().unwrap_or_else(|p| p.into_inner()).jobs += 1;
                    all.extend(vecs);
                }
                Ok(other) => {
                    return Err(EmbedError::Protocol(format!(
                        "unexpected reply to embed_texts: {}",
                        response_kind(&other)
                    )));
                }
                Err(EmbedError::Transport(_)) => {
                    // The worker died mid-job (or never answered): restart
                    // with backoff and retry the chunk exactly once.
                    self.stats.lock().unwrap_or_else(|p| p.into_inner()).retries += 1;
                    info!("embed job interrupted; restarting worker and retrying once");
                    let retry_id = self.next_id.fetch_add(1, Ordering::Relaxed);
                    let retry = EmbedRequest::EmbedTexts {
                        id: retry_id,
                        texts: chunk.to_vec(),
                    };
                    match self.transact_after_restart(retry) {
                        Ok(EmbedResponse::Embeddings { vecs, .. }) => {
                            self.stats.lock().unwrap_or_else(|p| p.into_inner()).jobs += 1;
                            all.extend(vecs);
                        }
                        Ok(other) => {
                            return Err(EmbedError::Protocol(format!(
                                "unexpected reply to retried embed_texts: {}",
                                response_kind(&other)
                            )));
                        }
                        Err(e) => return Err(e),
                    }
                }
                Err(e) => return Err(e),
            }
        }
        Ok(all)
    }

    /// Health ping: handshake + `status` round-trip, spawning the worker if
    /// needed. Refreshes the cached `model_loaded` flags.
    pub fn ping(&self) -> Result<WorkerSnapshot, EmbedError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        match self.transact(EmbedRequest::Status { id }) {
            Ok(EmbedResponse::WorkerStatus {
                model_loaded,
                backend,
                stub,
                dim,
                uptime_secs,
                jobs_served,
                ..
            }) => {
                self.model_loaded.store(model_loaded, Ordering::Relaxed);
                self.stub.store(stub, Ordering::Relaxed);
                Ok(WorkerSnapshot {
                    model_loaded,
                    backend,
                    stub,
                    dim,
                    uptime_secs,
                    jobs_served,
                })
            }
            Ok(other) => Err(EmbedError::Protocol(format!(
                "unexpected reply to status: {}",
                response_kind(&other)
            ))),
            Err(e) => Err(e),
        }
    }

    /// One attempt: ensure a worker, handshake, send, receive. Holds the
    /// connection mutex for the whole request (single client); the child
    /// slot is only taken briefly, so `terminate_worker` can kill mid-job.
    /// Transport failures leave the dead child in place for the caller to
    /// restart.
    fn transact(&self, req: EmbedRequest) -> Result<EmbedResponse, EmbedError> {
        let _conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        {
            let mut slot = self.slot.lock().unwrap_or_else(|p| p.into_inner());
            self.ensure_worker_locked(&mut slot)?;
        }
        match self.round_trip(&req) {
            Ok(resp) => {
                self.failures.store(0, Ordering::Relaxed);
                Ok(resp)
            }
            Err(e) => {
                if matches!(e, EmbedError::Transport(_)) {
                    self.failures.fetch_add(1, Ordering::Relaxed);
                }
                Err(e)
            }
        }
    }

    /// Restart path: drop the (presumed dead) child and run one attempt.
    /// Used exactly once per job after a transport failure. The backoff wait
    /// lives in `ensure_worker_locked` below, so the retry path sleeps
    /// exactly once (single sleep, `backoff_for` shape).
    fn transact_after_restart(&self, req: EmbedRequest) -> Result<EmbedResponse, EmbedError> {
        let _conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        {
            let mut slot = self.slot.lock().unwrap_or_else(|p| p.into_inner());
            kill_locked(&mut slot);
        }
        {
            let mut slot = self.slot.lock().unwrap_or_else(|p| p.into_inner());
            self.ensure_worker_locked(&mut slot)?;
        }
        match self.round_trip(&req) {
            Ok(resp) => {
                self.failures.store(0, Ordering::Relaxed);
                Ok(resp)
            }
            Err(e) => {
                if matches!(e, EmbedError::Transport(_)) {
                    self.failures.fetch_add(1, Ordering::Relaxed);
                }
                Err(e)
            }
        }
    }

    /// The worker is usable when our child is alive or a foreign worker
    /// answers the pipe (e.g. left idling by a previous daemon). Otherwise
    /// spawn and wait for the pipe.
    fn ensure_worker_locked(&self, slot: &mut Slot) -> Result<(), EmbedError> {
        if let Some(child) = slot.child.as_mut() {
            match child
                .try_wait()
                .map_err(|e| EmbedError::Spawn(e.to_string()))?
            {
                None => return Ok(()),
                Some(status) => {
                    info!(%status, "embed worker exited; will respawn on demand");
                    slot.child = None;
                }
            }
        }
        if probe(&self.config.pipe_name) {
            // Someone else's worker is serving; use it without owning it.
            return Ok(());
        }
        let failures = self.failures.load(Ordering::Relaxed);
        if failures > 0 {
            // The single backoff sleep on every (re)spawn path: `transact`
            // counts the failure, the retry path lands here exactly once.
            self.backoff_sleep(failures);
        }
        let mut cmd = Command::new(&self.config.worker_bin);
        cmd.args(&self.config.worker_args);
        let child = match cmd.spawn() {
            Ok(child) => child,
            Err(e) => {
                // The pre-spawn probe may have missed a live worker
                // (instance-turnover race): a patient re-probe distinguishes
                // "worker serving" from "launch broken" before failing.
                if patient_probe(&self.config.pipe_name) {
                    info!("embed pipe serving without a spawnable worker; using it");
                    return Ok(());
                }
                return Err(EmbedError::Spawn(format!(
                    "cannot launch {}: {e}",
                    self.config.worker_bin.display()
                )));
            }
        };
        info!(bin = %self.config.worker_bin.display(), "embed worker spawned");
        slot.child = Some(child);
        {
            let mut stats = self.stats.lock().unwrap_or_else(|p| p.into_inner());
            stats.spawns += 1;
            if failures > 0 {
                stats.restarts += 1;
            }
        }
        wait_for_pipe(&self.config.pipe_name)
    }

    /// Connect, handshake (health ping + version check + flag refresh), send
    /// the job, read the reply. Any IO/EOF problem is `Transport` (retryable);
    /// worker `Error` answers are definitive.
    fn round_trip(&self, req: &EmbedRequest) -> Result<EmbedResponse, EmbedError> {
        let mut stream = connect_with_retry(&self.config.pipe_name)?;
        let hello = EmbedRequest::Hello {
            protocol_version: EMBED_PROTOCOL_VERSION,
            client: "winsearchd".to_string(),
            client_version: ENGINE_VERSION.to_string(),
        };
        write_message(&mut stream, &hello).map_err(EmbedError::transport)?;
        let handshake: Option<EmbedResponse> =
            read_message(&mut stream).map_err(EmbedError::transport)?;
        match handshake {
            Some(EmbedResponse::HelloAck {
                protocol_version,
                model_loaded,
                stub,
                ..
            }) => {
                if protocol_version != EMBED_PROTOCOL_VERSION {
                    return Err(EmbedError::VersionMismatch(format!(
                        "worker speaks v{protocol_version}, supervisor speaks v{EMBED_PROTOCOL_VERSION}"
                    )));
                }
                self.model_loaded.store(model_loaded, Ordering::Relaxed);
                self.stub.store(stub, Ordering::Relaxed);
            }
            Some(EmbedResponse::Error { code, message, .. }) => {
                return Err(EmbedError::from_worker(code, message));
            }
            Some(other) => {
                return Err(EmbedError::Protocol(format!(
                    "unexpected handshake reply: {}",
                    response_kind(&other)
                )));
            }
            None => {
                return Err(EmbedError::Transport(
                    "worker closed during handshake".into(),
                ))
            }
        }
        write_message(&mut stream, req).map_err(EmbedError::transport)?;
        let reply: Option<EmbedResponse> =
            read_message(&mut stream).map_err(EmbedError::transport)?;
        match reply {
            Some(EmbedResponse::Error { code, message, .. }) => {
                Err(EmbedError::from_worker(code, message))
            }
            Some(resp) => Ok(resp),
            None => Err(EmbedError::Transport("worker closed mid-job".into())),
        }
    }
}

impl Default for EmbedSupervisor {
    fn default() -> Self {
        Self::new()
    }
}

fn response_kind(resp: &EmbedResponse) -> &'static str {
    match resp {
        EmbedResponse::HelloAck { .. } => "hello_ack",
        EmbedResponse::Embeddings { .. } => "embeddings",
        EmbedResponse::WorkerStatus { .. } => "worker_status",
        EmbedResponse::Error { .. } => "error",
        EmbedResponse::ShutdownAck { .. } => "shutdown_ack",
    }
}

/// Instant readiness check: one connect attempt, immediately dropped. Note
/// this consumes a pipe instance — never assert on two back-to-back probes
/// (the successor needs a scheduling quantum); `connect_with_retry` and the
/// spaced polls below are the turnover-safe primitives.
fn probe(pipe_name: &str) -> bool {
    PipeStream::connect(pipe_name).is_ok()
}

/// Spaced re-poll after a failed instant probe: distinguishes a live worker
/// in instance turnover from a truly absent pipe. Only used on paths where
/// spawning already failed, so the success path never pays for it.
fn patient_probe(pipe_name: &str) -> bool {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(5) {
        if probe(pipe_name) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

/// Connect with bounded retries (see `CONNECT_RETRY`). The first attempt
/// usually succeeds; retries cover the just-spawned / instance-turnover
/// windows without ever blocking the caller indefinitely.
fn connect_with_retry(pipe_name: &str) -> Result<PipeStream, EmbedError> {
    let start = Instant::now();
    loop {
        match PipeStream::connect(pipe_name) {
            Ok(stream) => return Ok(stream),
            Err(e) => {
                if start.elapsed() >= CONNECT_RETRY {
                    return Err(EmbedError::transport(e));
                }
                std::thread::sleep(CONNECT_POLL);
            }
        }
    }
}

fn wait_for_pipe(pipe_name: &str) -> Result<(), EmbedError> {
    // Spaced polls: each success consumes an instance, so back-to-back
    // probes would chase turnover; 100 ms spacing lets the server listen
    // again. The final gap is covered by `connect_with_retry`.
    let start = Instant::now();
    while start.elapsed() < SPAWN_WAIT {
        if probe(pipe_name) {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    // The child may still come up (slow model dir, cold DLLs); a later
    // request retries. Report honestly rather than hanging the caller.
    warn!("embed worker pipe did not appear within {SPAWN_WAIT:?}");
    Err(EmbedError::Spawn(format!(
        "worker pipe {pipe_name} did not appear within {SPAWN_WAIT:?}"
    )))
}

/// `base * 2^(failures-1)` capped at 5 s. Pure for tests.
fn backoff_for(failures: u32, base: Duration) -> Duration {
    if failures == 0 {
        return Duration::ZERO;
    }
    let shift = failures.saturating_sub(1).min(10);
    let scaled = base.saturating_mul(1 << shift);
    scaled.min(MAX_BACKOFF)
}

impl EmbedSupervisor {
    /// Waits out the backoff for `failures` consecutive transport failures
    /// (`backoff_for` shape). Honors the test `sleeper` hook when set.
    fn backoff_sleep(&self, failures: u32) {
        let d = backoff_for(failures, self.config.backoff_base);
        if d.is_zero() {
            return;
        }
        match &self.config.sleeper {
            Some(sleeper) => sleeper(d),
            None => std::thread::sleep(d),
        }
    }
}

fn kill_locked(slot: &mut Slot) {
    if let Some(mut child) = slot.child.take() {
        let _ = child.kill();
        let _ = child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
    use std::sync::Arc;

    /// Scripted in-process fake worker: answers handshakes, then behaves per
    /// `mode` so retry/backoff paths are deterministic without a real binary.
    struct FakeWorker {
        pipe_name: String,
        jobs_seen: Arc<Mutex<Vec<usize>>>,
        _thread: std::thread::JoinHandle<()>,
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Mode {
        /// First job connection dies after the handshake (crash mid-job);
        /// later ones serve stub vectors.
        DropFirstJob,
        /// Every job is answered with `model_not_available` (definitive).
        AlwaysUnavailable,
        /// Serve stub vectors immediately.
        Serve,
    }

    static PIPE_SEQ: AtomicUsize = AtomicUsize::new(0);

    impl FakeWorker {
        fn start(mode: Mode) -> Self {
            let n = PIPE_SEQ.fetch_add(1, AtomicOrdering::Relaxed);
            let pipe_name = format!(r"\\.\pipe\winsearch-embed-fake-{}-{n}", std::process::id());
            let jobs_seen = Arc::new(Mutex::new(Vec::new()));
            let jobs2 = Arc::clone(&jobs_seen);
            let name2 = pipe_name.clone();
            let dropped = Arc::new(AtomicBool::new(false));
            let dropped2 = Arc::clone(&dropped);
            let thread = std::thread::Builder::new()
                .name("fake-embed-worker".into())
                .spawn(move || {
                    if let Err(e) = ws_platform::pipe::serve(&name2, move |mut stream| {
                        let hello: Option<EmbedRequest> = read_message(&mut stream).unwrap_or(None);
                        if !matches!(hello, Some(EmbedRequest::Hello { .. })) {
                            return;
                        }
                        write_message(
                            &mut stream,
                            &EmbedResponse::HelloAck {
                                protocol_version: EMBED_PROTOCOL_VERSION,
                                worker_version: "fake".into(),
                                features: vec!["embed_texts".into()],
                                model_loaded: false,
                                stub: true,
                            },
                        )
                        .ok();
                        let req: Option<EmbedRequest> = read_message(&mut stream).unwrap_or(None);
                        match req {
                            Some(EmbedRequest::Status { id }) => {
                                write_message(
                                    &mut stream,
                                    &EmbedResponse::WorkerStatus {
                                        id,
                                        model_loaded: false,
                                        backend: Some("stub".into()),
                                        stub: true,
                                        dim: ws_embed_protocol::STUB_DIM,
                                        uptime_secs: 7,
                                        jobs_served: jobs2.lock().unwrap().len() as u64,
                                    },
                                )
                                .ok();
                            }
                            Some(EmbedRequest::EmbedTexts { id, texts }) => {
                                jobs2.lock().unwrap().push(texts.len());
                                match mode {
                                    Mode::AlwaysUnavailable => {
                                        write_message(
                                            &mut stream,
                                            &EmbedResponse::Error {
                                                id: Some(id),
                                                code: EmbedErrorCode::ModelNotAvailable,
                                                message: "no model".into(),
                                            },
                                        )
                                        .ok();
                                    }
                                    Mode::DropFirstJob
                                        if !dropped2.swap(true, AtomicOrdering::SeqCst) =>
                                    {
                                        // Crash: close without answering.
                                    }
                                    _ => {
                                        write_message(
                                            &mut stream,
                                            &EmbedResponse::Embeddings {
                                                id,
                                                vecs: ws_embed_protocol::stub_embed_texts(
                                                    &texts,
                                                    ws_embed_protocol::STUB_DIM,
                                                ),
                                                dim: ws_embed_protocol::STUB_DIM,
                                                backend: "stub".into(),
                                                stub: true,
                                            },
                                        )
                                        .ok();
                                    }
                                }
                            }
                            // Anything else on a test double: close quietly.
                            _ => {}
                        }
                    }) {
                        eprintln!("fake serve exited: {e}");
                    }
                })
                .unwrap();
            // Wait until the accept loop listens. Spaced connect-polls (see
            // `probe`): the first success may consume the only instance, so
            // settle, then re-poll rather than asserting back-to-back.
            let start = Instant::now();
            let mut up = false;
            while start.elapsed() < Duration::from_secs(60) {
                if probe(&pipe_name) {
                    up = true;
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            assert!(up, "fake worker pipe never appeared (first sighting)");
            std::thread::sleep(Duration::from_secs(3));
            let start = Instant::now();
            let mut steady = false;
            while start.elapsed() < Duration::from_secs(30) {
                if probe(&pipe_name) {
                    steady = true;
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            assert!(steady, "fake worker pipe never settled");
            Self {
                pipe_name,
                jobs_seen,
                _thread: thread,
            }
        }
    }

    fn test_supervisor(pipe_name: String) -> EmbedSupervisor {
        test_supervisor_with_sleeper(pipe_name, Duration::from_millis(1), None)
    }

    fn test_supervisor_with_sleeper(
        pipe_name: String,
        backoff_base: Duration,
        sleeper: Option<std::sync::Arc<dyn Fn(Duration) + Send + Sync>>,
    ) -> EmbedSupervisor {
        EmbedSupervisor::with_config(EmbedConfig {
            // Never spawned in these tests: the fake is already serving, so
            // `ensure` takes the probe path.
            worker_bin: PathBuf::from(r"C:\definitely-not-a-worker.exe"),
            pipe_name,
            worker_args: Vec::new(),
            backoff_base,
            sleeper,
        })
    }

    #[test]
    fn backoff_doubles_and_caps() {
        let base = Duration::from_millis(250);
        assert_eq!(backoff_for(0, base), Duration::ZERO);
        assert_eq!(backoff_for(1, base), Duration::from_millis(250));
        assert_eq!(backoff_for(2, base), Duration::from_millis(500));
        assert_eq!(backoff_for(3, base), Duration::from_secs(1));
        assert_eq!(backoff_for(99, base), MAX_BACKOFF);
    }

    #[test]
    fn live_worker_reuse_never_sleeps() {
        // Retrying against an already-serving worker costs no backoff: the
        // single sleep lives on the spawn path (see the respawn test below).
        // The recorder proves the fast path waits never.
        let fake = FakeWorker::start(Mode::DropFirstJob);
        let sleeps = std::sync::Arc::new(Mutex::new(Vec::new()));
        let sleeps2 = std::sync::Arc::clone(&sleeps);
        let sup = test_supervisor_with_sleeper(
            fake.pipe_name.clone(),
            Duration::from_millis(250),
            Some(std::sync::Arc::new(move |d: Duration| {
                sleeps2.lock().unwrap().push(d);
            })),
        );

        let vecs = sup.embed_texts(&["retry me".to_string()]).expect("retry succeeds");
        assert_eq!(vecs.len(), 1);
        assert_eq!(sup.stats().retries, 1);
        assert!(
            sleeps.lock().unwrap().is_empty(),
            "live-worker retry must not back off"
        );
    }

    #[test]
    fn crash_mid_job_restarts_and_retries_once() {
        let fake = FakeWorker::start(Mode::DropFirstJob);
        let sup = test_supervisor(fake.pipe_name.clone());

        let texts = vec!["alpha".to_string(), "beta".to_string()];
        let vecs = sup.embed_texts(&texts).expect("retry must succeed");
        assert_eq!(vecs.len(), 2);
        assert_eq!(
            vecs[0],
            ws_embed_protocol::stub_vector("alpha", ws_embed_protocol::STUB_DIM)
        );

        let stats = sup.stats();
        assert_eq!(
            stats.retries, 1,
            "one interrupted job retried, got {stats:?}"
        );
        assert_eq!(stats.spawns, 0, "fake was already serving; nothing spawned");
        // Both attempts reached a worker (first died, second served).
        assert_eq!(*fake.jobs_seen.lock().unwrap(), vec![2, 2]);
        // The handshake refreshed the cache: stub counts as servable.
        assert!(sup.model_loaded());
    }

    #[test]
    fn definitive_worker_errors_are_not_retried() {
        let fake = FakeWorker::start(Mode::AlwaysUnavailable);
        let sup = test_supervisor(fake.pipe_name.clone());

        let err = sup.embed_texts(&["x".to_string()]).unwrap_err();
        assert!(
            matches!(err, EmbedError::ModelNotAvailable(_)),
            "got {err:?}"
        );
        assert_eq!(sup.stats().retries, 0);
        assert_eq!(fake.jobs_seen.lock().unwrap().len(), 1);
    }

    #[test]
    fn large_batches_split_into_capped_jobs() {
        let fake = FakeWorker::start(Mode::Serve);
        let sup = test_supervisor(fake.pipe_name.clone());

        let texts: Vec<String> = (0..(MAX_TEXTS_PER_JOB + 44))
            .map(|i| format!("t{i}"))
            .collect();
        let vecs = sup.embed_texts(&texts).expect("chunked embed");
        assert_eq!(vecs.len(), texts.len());
        assert_eq!(
            *fake.jobs_seen.lock().unwrap(),
            vec![MAX_TEXTS_PER_JOB, 44],
            "one capped job plus the remainder"
        );
    }

    #[test]
    fn empty_batch_is_rejected_before_touching_the_worker() {
        let fake = FakeWorker::start(Mode::Serve);
        let sup = test_supervisor(fake.pipe_name.clone());
        assert!(matches!(
            sup.embed_texts(&[]),
            Err(EmbedError::InvalidRequest(_))
        ));
        assert!(fake.jobs_seen.lock().unwrap().is_empty());
    }

    #[test]
    fn ping_reports_worker_snapshot() {
        let fake = FakeWorker::start(Mode::Serve);
        let sup = test_supervisor(fake.pipe_name);
        let snap = sup.ping().expect("ping");
        assert!(snap.stub);
        assert!(!snap.model_loaded, "fake serves no real weights");
        assert_eq!(snap.backend.as_deref(), Some("stub"));
        assert_eq!(snap.dim, ws_embed_protocol::STUB_DIM);
        assert!(snap.uptime_secs <= 120);
        assert!(sup.model_loaded());
    }

    #[test]
    fn config_env_fallbacks() {
        // No WS_EMBED_* set by these tests (explicit config everywhere), so
        // the defaults resolve to the shipped layout.
        let cfg = EmbedConfig::from_env_or_default();
        assert!(cfg.pipe_name.starts_with(r"\\.\pipe\winsearch-embed-"));
        assert_eq!(
            cfg.worker_bin.file_name().unwrap().to_string_lossy(),
            format!("winsearch-embed{}", std::env::consts::EXE_SUFFIX)
        );
    }

    /// Real-binary tests: drive the actual `winsearch-embed` process on a
    /// unique pipe with the deterministic stub path (no model file needed).
    mod real_binary {
        use super::*;
        use std::sync::OnceLock;

        static SEQ: AtomicUsize = AtomicUsize::new(0);

        /// The worker takes a machine-global single-instance mutex, so only
        /// one real-binary test may run at a time (they would otherwise kill
        /// each other's workers at startup).
        static SERIAL: OnceLock<Mutex<()>> = OnceLock::new();

        fn serial() -> std::sync::MutexGuard<'static, ()> {
            SERIAL
                .get_or_init(|| Mutex::new(()))
                .lock()
                .unwrap_or_else(|p| p.into_inner())
        }

        fn worker_bin() -> PathBuf {
            // Test executables live in target/<profile>/deps/; the worker
            // binary is built next to it by any workspace build.
            let mut p = std::env::current_exe().expect("current exe");
            p.pop(); // deps
            p.pop(); // <profile>
            let bin = p.join(format!("winsearch-embed{}", std::env::consts::EXE_SUFFIX));
            assert!(
                bin.is_file(),
                "worker binary missing at {} — run a workspace build first (`cargo build --workspace`)",
                bin.display()
            );
            bin
        }

        fn real_supervisor(extra_args: &[&str]) -> EmbedSupervisor {
            real_supervisor_traced(extra_args).0
        }

        /// Real supervisor plus a recorder for its backoff waits (no real
        /// waiting when the worker binary is exercised).
        fn real_supervisor_traced(
            extra_args: &[&str],
        ) -> (EmbedSupervisor, std::sync::Arc<Mutex<Vec<Duration>>>) {
            let n = SEQ.fetch_add(1, AtomicOrdering::Relaxed);
            let pipe = format!(r"\\.\pipe\winsearch-embed-int-{}-{n}", std::process::id());
            let mut args = vec!["--pipe".to_string(), pipe.clone()];
            args.extend(extra_args.iter().map(|s| s.to_string()));
            let sleeps = std::sync::Arc::new(Mutex::new(Vec::new()));
            let sleeps2 = std::sync::Arc::clone(&sleeps);
            let sup = EmbedSupervisor::with_config(EmbedConfig {
                worker_bin: worker_bin(),
                pipe_name: pipe,
                worker_args: args,
                backoff_base: Duration::from_millis(10),
                sleeper: Some(std::sync::Arc::new(move |d: Duration| {
                    sleeps2.lock().unwrap().push(d);
                })),
            });
            (sup, sleeps)
        }

        fn unit_norm(v: &[f32]) -> f64 {
            v.iter().map(|x| f64::from(*x).powi(2)).sum::<f64>().sqrt()
        }

        fn wait_until(timeout: Duration, mut cond: impl FnMut() -> bool) -> bool {
            let start = Instant::now();
            while !cond() {
                if start.elapsed() > timeout {
                    return false;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            true
        }

        #[test]
        fn real_worker_round_trips_a_text_batch() {
            let _serial = serial();
            let sup = real_supervisor(&["--stub", "--idle-secs", "60"]);
            assert!(!sup.is_worker_running(), "nothing spawned yet (lazy)");

            let texts = vec![
                "budget spreadsheet".to_string(),
                "receipt from the hardware store".to_string(),
                "quarterly review notes".to_string(),
            ];
            let vecs = sup.embed_texts(&texts).expect("stub round-trip");
            assert!(sup.is_worker_running(), "first request spawned the worker");
            assert_eq!(vecs.len(), 3);
            for v in &vecs {
                assert_eq!(v.len(), ws_embed_protocol::STUB_DIM);
                assert!((unit_norm(v) - 1.0).abs() < 1e-5, "stub not unit-norm");
            }
            // Deterministic: same batch, same vectors.
            let again = sup.embed_texts(&texts).expect("second batch");
            assert_eq!(vecs, again);
            assert!(sup.model_loaded(), "stub servable counts as loaded");

            let snap = sup.ping().expect("health ping");
            assert!(snap.stub);
            assert!(snap.jobs_served >= 2);
            sup.terminate_worker();
        }

        #[test]
        fn killing_the_worker_mid_job_restarts_and_retries() {
            let _serial = serial();
            let (sup, sleeps) =
                real_supervisor_traced(&["--stub", "--idle-secs", "60", "--stub-delay-ms", "10"]);
            // ~3 s of stub work: the kill below lands mid-job even on a
            // heavily loaded machine.
            let texts: Vec<String> = (0..300).map(|i| format!("mid-job item {i}")).collect();

            let worker = std::thread::scope(|s| {
                let handle = s.spawn(|| sup.embed_texts(&texts));
                // Wait for the spawn, then kill well inside the ~1 s stub job.
                assert!(
                    wait_until(Duration::from_secs(15), || sup.is_worker_running()),
                    "worker never started"
                );
                std::thread::sleep(Duration::from_millis(300));
                sup.terminate_worker();
                handle.join().expect("embed thread panicked")
            });
            let vecs = worker.expect("job retried after restart must succeed");
            assert_eq!(vecs.len(), texts.len());
            let stats = sup.stats();
            assert!(stats.retries >= 1, "expected a retry, got {stats:?}");
            assert!(stats.restarts >= 1, "expected a restart, got {stats:?}");
            // Single backoff on the respawn path (I2): exactly one wait of
            // `backoff_for(1, base)` for the whole kill → restart → retry.
            assert_eq!(
                *sleeps.lock().unwrap(),
                vec![backoff_for(1, Duration::from_millis(10))],
                "exactly one backoff sleep with the backoff_for shape"
            );

            // The respawned worker serves follow-up requests.
            let follow = sup
                .embed_texts(&["after restart".to_string()])
                .expect("follow-up");
            assert_eq!(follow.len(), 1);
            sup.terminate_worker();
        }

        #[test]
        fn idle_worker_exits_and_respawns_on_demand() {
            let _serial = serial();
            let sup = real_supervisor(&["--stub", "--idle-secs", "2"]);
            sup.embed_texts(&["warm".to_string()]).expect("warmup job");
            assert!(sup.is_worker_running());

            assert!(
                wait_until(Duration::from_secs(90), || !sup.is_worker_running()),
                "worker did not idle-exit (no lingering process)"
            );
            let spawns_before = sup.stats().spawns;

            // Next request transparently respawns.
            let vecs = sup
                .embed_texts(&["after idle".to_string()])
                .expect("respawn");
            assert_eq!(vecs.len(), 1);
            assert!(sup.stats().spawns > spawns_before);
            sup.terminate_worker();
        }
    }
}
