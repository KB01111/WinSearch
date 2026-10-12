//! Application data locations. Everything lives under `%LOCALAPPDATA%\WinSearch`
//! (models and indexes included) so uninstalling can remove it all.

use std::path::PathBuf;

pub const APP_DIR_NAME: &str = "WinSearch";

fn local_app_data() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(".").join("AppData").join("Local"))
}

/// Root for all engine-managed data: `%LOCALAPPDATA%\WinSearch`.
pub fn data_dir() -> PathBuf {
    local_app_data().join(APP_DIR_NAME)
}

pub fn logs_dir() -> PathBuf {
    data_dir().join("logs")
}

/// SQLite databases (`index.db`, `code.db`) live here.
pub fn db_dir() -> PathBuf {
    data_dir().join("db")
}

pub fn index_db_path() -> PathBuf {
    db_dir().join("index.db")
}

pub fn code_db_path() -> PathBuf {
    db_dir().join("code.db")
}

/// Downloaded/imported model files (text GGUF + mmproj).
pub fn model_dir() -> PathBuf {
    data_dir().join("models")
}

/// Thumbnail and derived-artifact cache; safe to delete at any time.
pub fn cache_dir() -> PathBuf {
    data_dir().join("cache")
}

pub fn thumbnail_cache_dir() -> PathBuf {
    cache_dir().join("thumbs")
}

/// UI-owned settings file (shared location; the frontend reads and writes it).
pub fn settings_path() -> PathBuf {
    data_dir().join("settings.json")
}

/// Creates the standard directory layout. Idempotent.
pub fn ensure_layout() -> std::io::Result<()> {
    for dir in [data_dir(), logs_dir(), db_dir(), model_dir(), thumbnail_cache_dir()] {
        std::fs::create_dir_all(dir)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_creation_is_idempotent() {
        ensure_layout().unwrap();
        ensure_layout().unwrap();
        assert!(data_dir().is_dir());
        assert!(index_db_path().to_string_lossy().ends_with("index.db"));
    }
}
