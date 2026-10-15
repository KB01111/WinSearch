//! `winsearch-embed` — the WinSearch embedding inference worker.
//!
//! A crash-isolated process owned by `winsearchd`'s supervisor. It serves the
//! internal embed-worker protocol (see `ws-embed-protocol` and
//! `docs/ipc-protocol.md`) on the per-user pipe
//! `\\.\pipe\winsearch-embed-<SID>`, loading the model lazily on the first
//! job and exiting after [`server::DEFAULT_IDLE_SECS`] without work — process
//! exit returns all GPU memory to the driver, which is the full GPU release.
//!
//! Configuration is CLI-first with env fallbacks (documented in
//! `docs/ipc-protocol.md`):
//! `--pipe/--idle-secs/--stub/--model/--backend/--stub-delay-ms`, else
//! `WS_EMBED_PIPE/WS_EMBED_IDLE_SECS/WS_EMBED_STUB/WS_EMBED_MODEL/
//! WS_EMBED_BACKEND/WS_EMBED_STUB_DELAY_MS`, else compiled defaults.

mod server;

use tracing::info;
use ws_platform::SingleInstanceGuard;

use server::WorkerConfig;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--version") {
        println!("winsearch-embed {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    init_logging();

    // Exactly one worker per session serves the pipe. A loser here means the
    // winner is already serving: exit quietly so the supervisor's pipe-wait
    // connects to it.
    let _guard = match SingleInstanceGuard::acquire_embed() {
        Some(g) => g,
        None => {
            eprintln!("winsearch-embed: another instance is already serving; exiting");
            return Ok(());
        }
    };

    let mut config = WorkerConfig::from_env()?;
    config.apply_args(&args[1..])?;

    info!(
        pipe = %config.pipe_name,
        stub = config.stub,
        idle_secs = config.idle_timeout.as_secs(),
        "winsearch-embed starting"
    );
    server::run(&config)
}

fn init_logging() {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("winsearch_embed=info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .try_init();
}
