//! `winsearchd` — the WinSearch indexing and search engine.
//!
//! Runs independently of the UI: it keeps indexing and answering searches over
//! the per-user named pipe even when no frontend is connected.

mod engine;
mod server;

use std::sync::Arc;

use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

use ws_platform::identity;
use ws_platform::paths;
use ws_platform::pipe;
use ws_platform::SingleInstanceGuard;

use engine::EngineService;

fn main() -> anyhow::Result<()> {
    if std::env::args().any(|a| a == "--version") {
        println!("winsearchd {}", server::ENGINE_VERSION);
        return Ok(());
    }

    paths::ensure_layout()?;
    init_logging();
    info!(version = server::ENGINE_VERSION, "winsearchd starting");

    // Only one engine per session; the pipe name is per-user but a second
    // engine would contend on the database anyway.
    let _guard = match SingleInstanceGuard::acquire_engine() {
        Some(g) => g,
        None => {
            info!("another winsearchd instance is already running; exiting");
            return Ok(());
        }
    };

    let pipe_name = identity::pipe_name()?;
    let engine = EngineService::new();
    let handler_engine = Arc::clone(&engine);

    info!(pipe = %pipe_name, "serving IPC");
    pipe::serve(&pipe_name, move |stream| {
        server::handle_connection(stream, Arc::clone(&handler_engine))
    })?;

    Ok(())
}

fn init_logging() {
    let logs_dir = paths::logs_dir();
    if let Err(e) = std::fs::create_dir_all(&logs_dir) {
        eprintln!("warning: cannot create log directory: {e}");
        return;
    }
    let appender = tracing_appender::rolling::daily(&logs_dir, "winsearchd.log");
    let (writer, guard) = tracing_appender::non_blocking(appender);
    // Keep the guard alive for the process lifetime.
    Box::leak(Box::new(guard));

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("winsearchd=info,ws_platform=info,ws_protocol=warn"));

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(writer)
        .with_ansi(false)
        .init();

    warn!(path = %logs_dir.display(), "file logging active");
}
