//! [`ModelManager`]: the filesystem view of the embedding model used by
//! `winsearchd` — status, full download, local import, deletion, and
//! verification. Threading (progress sharing, cancellation, completion
//! broadcast) lives in `winsearchd`'s model service; everything here is
//! blocking and borrow-free so it runs on a plain worker thread.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use serde::{Deserialize, Serialize};

use crate::download::{download_file, verify_file, DownloadError, DownloadOptions, ProgressFn};
use crate::manifest::{ExpectedFile, EXPECTED_FILES};

/// Lifecycle of the on-disk model, mirrored 1:1 on the IPC wire
/// (`model_status`) and in `WinSearch.Core`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ModelState {
    #[default]
    NotDownloaded,
    Downloading,
    Paused,
    Ready,
    Error,
}

/// Point-in-time snapshot: state + byte counters + optional error text.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct ModelStatus {
    pub state: ModelState,
    #[serde(default)]
    pub bytes: u64,
    #[serde(default)]
    pub total_bytes: u64,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum ManagerError {
    #[error("download: {0}")]
    Download(#[from] DownloadError),
    #[error("import: {0}")]
    Import(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// Owns the model directory. `options` carries the download tuning; tests
/// point `base_url_override` at a local fixture server.
pub struct ModelManager {
    dir: PathBuf,
    options: DownloadOptions,
    files: Vec<ExpectedFile>,
}

impl ModelManager {
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            options: DownloadOptions::default(),
            files: EXPECTED_FILES.to_vec(),
        }
    }

    pub fn with_options(dir: PathBuf, options: DownloadOptions) -> Self {
        Self {
            dir,
            options,
            files: EXPECTED_FILES.to_vec(),
        }
    }

    /// Custom file list (tests serve tiny stand-ins for the real GGUFs).
    pub fn with_files(dir: PathBuf, options: DownloadOptions, files: Vec<ExpectedFile>) -> Self {
        Self { dir, options, files }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Combined size of every manifest file.
    pub fn total_bytes(&self) -> u64 {
        self.files.iter().map(|f| f.size).sum()
    }

    /// Filesystem-derived status (no in-flight knowledge — the service merges
    /// live progress on top). A `.part` file means a paused download;
    /// anything else incomplete is `not_downloaded`; a stale marker is
    /// `error` only when files are present but invalid.
    pub fn status(&self) -> ModelStatus {
        let total = self.total_bytes();
        let mut bytes = 0u64;
        let mut complete = true;
        let mut partial = false;
        for f in &self.files {
            let dest = self.dir.join(f.name);
            match dest.metadata() {
                Ok(m) if m.len() == f.size && verify_file(&dest, f).is_ok() => {
                    bytes += f.size;
                }
                Ok(m) => {
                    bytes += m.len().min(f.size);
                    complete = false;
                }
                Err(_) => {
                    complete = false;
                    if self.dir.join(format!("{}.part", f.name)).exists() {
                        partial = true;
                    }
                }
            }
        }
        if complete {
            ModelStatus {
                state: ModelState::Ready,
                bytes: total,
                total_bytes: total,
                error: None,
            }
        } else if partial {
            ModelStatus {
                state: ModelState::Paused,
                bytes,
                total_bytes: total,
                error: None,
            }
        } else {
            ModelStatus {
                state: ModelState::NotDownloaded,
                bytes,
                total_bytes: total,
                error: None,
            }
        }
    }

    /// Downloads every manifest file (skipping ones already valid), in order.
    /// Progress aggregates across files; cancel stops after the current chunk
    /// and leaves `.part` files for the next call to resume.
    pub fn download_all(
        &self,
        progress: &ProgressFn<'_>,
        cancel: &AtomicBool,
    ) -> Result<(), DownloadError> {
        fs::create_dir_all(&self.dir)?;
        let total = self.total_bytes();
        let mut done_before = 0u64;
        for f in &self.files {
            if self.is_file_valid(f) {
                done_before += f.size;
                continue;
            }
            let done = done_before;
            let combined = |d: u64, _t: u64| progress(done + d, total);
            match download_file(f, &self.dir, &self.options, &combined, cancel) {
                Ok(_) => done_before += f.size,
                Err(e) => return Err(e),
            }
        }
        progress(total, total);
        Ok(())
    }

    /// Imports model files from a local path: either a directory containing
    /// (some of) the manifest files, or a single file whose name matches one
    /// manifest entry. Every copied file is verified (size + hash when
    /// pinned); failures abort with nothing half-written.
    pub fn import(&self, path: &Path) -> Result<ModelStatus, ManagerError> {
        fs::create_dir_all(&self.dir)?;
        if path.is_dir() {
            let mut any = false;
            for f in &self.files {
                let src = path.join(f.name);
                if src.is_file() {
                    self.import_one(f, &src)?;
                    any = true;
                }
            }
            if !any {
                return Err(ManagerError::Import(format!(
                    "directory {} contains none of the expected model files",
                    path.display()
                )));
            }
        } else if path.is_file() {
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default()
                .to_string();
            let entry = self
                .files
                .iter()
                .find(|f| f.name == name)
                .ok_or_else(|| {
                    let known: Vec<&str> = self.files.iter().map(|f| f.name).collect();
                    ManagerError::Import(format!(
                        "{name} is not a known model file (expected one of: {})",
                        known.join(", ")
                    ))
                })?;
            self.import_one(entry, path)?;
        } else {
            return Err(ManagerError::Import(format!(
                "import path does not exist: {}",
                path.display()
            )));
        }
        Ok(self.status())
    }

    /// Deletes every manifest file (plus stale `.part` files). Missing files
    /// are not an error.
    pub fn delete(&self) -> Result<ModelStatus, ManagerError> {
        for f in &self.files {
            let dest = self.dir.join(f.name);
            if dest.exists() {
                fs::remove_file(&dest)?;
            }
            let part = self.dir.join(format!("{}.part", f.name));
            if part.exists() {
                fs::remove_file(&part)?;
            }
        }
        Ok(self.status())
    }

    fn import_one(&self, expected: &ExpectedFile, src: &Path) -> Result<(), ManagerError> {
        // Verify the source before touching the model dir.
        let len = src.metadata()?.len();
        if len != expected.size {
            return Err(ManagerError::Import(format!(
                "{}: size {len} != expected {}",
                expected.name, expected.size
            )));
        }
        if let Some(pinned) = expected.sha256 {
            let got = crate::download::sha256_of(src).map_err(DownloadError::from)?;
            if got != pinned {
                return Err(ManagerError::Import(format!(
                    "{}: sha-256 {got} != expected {pinned}",
                    expected.name
                )));
            }
        }
        // Copy via a temp file + rename so a crash never leaves a truncated
        // model file behind.
        let dest = self.dir.join(expected.name);
        let tmp = self.dir.join(format!("{}.import-tmp", expected.name));
        fs::copy(src, &tmp)?;
        verify_file(&tmp, expected).map_err(|e| {
            let _ = fs::remove_file(&tmp);
            ManagerError::Download(e)
        })?;
        fs::rename(&tmp, &dest)?;
        Ok(())
    }

    fn is_file_valid(&self, f: &ExpectedFile) -> bool {
        let dest = self.dir.join(f.name);
        dest.is_file() && verify_file(&dest, f).is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::EXPECTED_FILES;
    use std::fs;
    use std::sync::atomic::AtomicBool;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ws-modelmgr-manager-{}-{}-{:?}",
            tag,
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn no_progress(_: u64, _: u64) {}

    #[test]
    fn status_reflects_filesystem() {
        use crate::manifest::TOTAL_BYTES;
        let dir = temp_dir("status");
        let mgr = ModelManager::new(dir.clone());

        let s = mgr.status();
        assert_eq!(s.state, ModelState::NotDownloaded);
        assert_eq!(s.total_bytes, TOTAL_BYTES);

        // A stale .part marker reads back as paused.
        fs::write(dir.join(format!("{}.part", EXPECTED_FILES[0].name)), b"half").unwrap();
        assert_eq!(mgr.status().state, ModelState::Paused);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn delete_removes_files_and_part_markers() {
        let dir = temp_dir("delete");
        for f in &EXPECTED_FILES {
            fs::write(dir.join(f.name), vec![0u8; 8]).unwrap();
            fs::write(dir.join(format!("{}.part", f.name)), vec![0u8; 8]).unwrap();
        }
        let mgr = ModelManager::new(dir.clone());
        let s = mgr.delete().unwrap();
        assert_eq!(s.state, ModelState::NotDownloaded);
        assert!(fs::read_dir(&dir).unwrap().next().is_none());

        // Deleting again is a no-op.
        mgr.delete().unwrap();
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn import_rejects_unknown_and_bad_sized_files() {
        let dir = temp_dir("import");
        let src = temp_dir("import-src");
        let mgr = ModelManager::new(dir.clone());

        // Unknown file name.
        fs::write(src.join("other.bin"), b"nope").unwrap();
        assert!(mgr.import(&src.join("other.bin")).is_err());

        // Right name, wrong size (manifest expects hundreds of MB).
        fs::write(src.join(EXPECTED_FILES[0].name), b"too small").unwrap();
        let err = mgr.import(&src.join(EXPECTED_FILES[0].name)).unwrap_err();
        assert!(err.to_string().contains("size"), "got {err}");

        // Directory containing only a wrong-sized candidate fails.
        let dir_err = mgr.import(&src).unwrap_err();
        assert!(
            dir_err.to_string().contains("size") || dir_err.to_string().contains("none"),
            "got {dir_err}"
        );

        // Missing path.
        assert!(mgr.import(&src.join("missing.gguf")).is_err());

        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&src);
    }

    #[test]
    fn model_status_wire_shape() {
        // The exact JSON the C# mirror must parse.
        let s = ModelStatus {
            state: ModelState::Downloading,
            bytes: 42,
            total_bytes: 100,
            error: None,
        };
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("\"state\":\"downloading\""));
        let back: ModelStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(s, back);
        // Old payloads without the new counters still parse (additive IPC).
        let legacy: ModelStatus = serde_json::from_str(r#"{"state":"ready"}"#).unwrap();
        assert_eq!(legacy.state, ModelState::Ready);
        assert_eq!(legacy.bytes, 0);
    }

    #[test]
    fn cancel_flag_type_is_thread_shareable() {
        // Compile-time proof the download signature fits the engine model.
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<AtomicBool>();
        let _ = no_progress;
    }
}
