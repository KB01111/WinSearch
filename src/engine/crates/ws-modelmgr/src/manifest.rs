//! Pinned model manifest: which files make up the embedding model, where to
//! fetch them, and how to recognise a compatible install afterwards.
//!
//! ## How the constants were obtained (2026-10-15)
//!
//! - File list and sizes come from the Hugging Face repo metadata for
//!   [`MODEL_ID`] at commit [`REPO_COMMIT`]: `HEAD` requests against
//!   `https://huggingface.co/<id>/resolve/main/<file>` return
//!   `X-Linked-Size` (309855456 for the text GGUF, 554821024 for mmproj).
//! - The repo is Xet-backed, so Hugging Face publishes **no whole-file
//!   SHA-256** in its metadata — only Xet chunk hashes (`X-Xet-Hash` /
//!   `X-Linked-ETag`), which are content-chunk addresses, not file hashes.
//!   Downloading ~865 MB just to hash it was deliberately avoided, so
//!   `sha256` is `None` for both GGUFs: verification falls back to exact
//!   size (+ presence). Whenever a manifest entry carries `Some(sha256)`,
//!   downloads and imports are hash-verified; the test suite pins real
//!   SHA-256 values for its small fixture files and exercises the hash path
//!   (including tamper rejection) there.
//! - `EMBEDDING_DIM` (768) is the unified EmbeddingGemma 2 space recorded in
//!   the plan; the compat record below binds the index to it.

use serde::{Deserialize, Serialize};

/// Hugging Face repo the GGUFs are downloaded from.
pub const MODEL_ID: &str = "ggml-org/embeddinggemma-2-GGUF";

/// Repo commit the sizes below were read at (`X-Repo-Commit` header).
pub const REPO_COMMIT: &str = "bfcd298762cc34d0357ece5ebdd31791a3a374d8";

/// Unified EmbeddingGemma 2 embedding width.
pub const EMBEDDING_DIM: u32 = 768;

/// Text-embedding weights (Q8_0, ~310 MB).
pub const TEXT_FILE: &str = "embeddinggemma-2-Q8_0.gguf";

/// Multimodal projector (Q8_0, ~555 MB).
pub const MMPROJ_FILE: &str = "mmproj-embeddinggemma-2-Q8_0.gguf";

/// `meta` key holding the [`ModelCompat`] JSON record in `index.db`.
pub const EMBEDDING_MODEL_META_KEY: &str = "embedding_model";

/// One file that makes up the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpectedFile {
    /// File name inside the model directory.
    pub name: &'static str,
    /// Exact byte size (`X-Linked-Size` at [`REPO_COMMIT`]).
    pub size: u64,
    /// Expected hex SHA-256, when known (see module docs). Verified on
    /// download/import whenever present.
    pub sha256: Option<&'static str>,
}

impl ExpectedFile {
    /// `https://huggingface.co/<MODEL_ID>/resolve/main/<name>`.
    pub fn url(&self) -> String {
        format!("https://huggingface.co/{MODEL_ID}/resolve/main/{}", self.name)
    }
}

/// Both GGUFs, in download order (text weights first).
pub const EXPECTED_FILES: [ExpectedFile; 2] = [
    ExpectedFile {
        name: TEXT_FILE,
        size: 309_855_456,
        sha256: None,
    },
    ExpectedFile {
        name: MMPROJ_FILE,
        size: 554_821_024,
        sha256: None,
    },
];

/// Combined download size (865 MB class).
pub const TOTAL_BYTES: u64 = EXPECTED_FILES[0].size + EXPECTED_FILES[1].size;

/// Compat record stored as JSON under [`EMBEDDING_MODEL_META_KEY`]. The
/// semantic pipeline (Phase 3+) reads it before trusting stored vectors: a
/// different model id or embedding width invalidates every vector and queues
/// a full re-embed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelCompat {
    pub model_id: String,
    pub embedding_dim: u32,
    pub repo_commit: String,
}

impl Default for ModelCompat {
    fn default() -> Self {
        Self::current()
    }
}

impl ModelCompat {
    /// The compat record for the manifest pinned in this build.
    pub fn current() -> Self {
        Self {
            model_id: MODEL_ID.to_string(),
            embedding_dim: EMBEDDING_DIM,
            repo_commit: REPO_COMMIT.to_string(),
        }
    }

    /// Serializes for the `meta` table.
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("ModelCompat is JSON-serializable")
    }

    /// Parses a stored record; unknown/invalid values mean incompatible.
    pub fn from_json(raw: &str) -> Option<Self> {
        serde_json::from_str(raw).ok()
    }

    /// True when stored vectors produced under `other` are still usable.
    pub fn is_compatible_with(&self, other: &ModelCompat) -> bool {
        self.model_id == other.model_id && self.embedding_dim == other.embedding_dim
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_covers_both_ggufs() {
        assert_eq!(EXPECTED_FILES.len(), 2);
        assert_eq!(EXPECTED_FILES[0].name, TEXT_FILE);
        assert_eq!(EXPECTED_FILES[1].name, MMPROJ_FILE);
        assert_eq!(TOTAL_BYTES, 309_855_456 + 554_821_024);
        for f in &EXPECTED_FILES {
            assert!(f.url().ends_with(f.name));
            assert!(f.url().contains(MODEL_ID));
        }
    }

    #[test]
    fn compat_record_round_trip_and_mismatch() {
        let current = ModelCompat::current();
        let back = ModelCompat::from_json(&current.to_json()).unwrap();
        assert_eq!(current, back);
        assert!(current.is_compatible_with(&back));

        let other_dim = ModelCompat {
            embedding_dim: 384,
            ..current.clone()
        };
        assert!(!current.is_compatible_with(&other_dim));

        let other_model = ModelCompat {
            model_id: "someone-else/other-model".into(),
            ..current.clone()
        };
        assert!(!current.is_compatible_with(&other_model));

        // A repo-commit bump alone does not invalidate vectors.
        let bumped_commit = ModelCompat {
            repo_commit: "deadbeef".into(),
            ..current.clone()
        };
        assert!(current.is_compatible_with(&bumped_commit));

        assert!(ModelCompat::from_json("not json").is_none());
    }
}
