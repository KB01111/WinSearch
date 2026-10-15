//! Shared value types for the index.

use std::path::Path;

/// What a stored entry is, for result typing and filtering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    Folder,
    Application,
    Image,
    Screenshot,
    Document,
    Audio,
    Video,
    File,
}

/// Coarse result type of a search hit (mirrors the wire `ResultKind` minus
/// the non-indexed kinds such as `Calculator`/`Settings`, which are
/// synthesized at query time).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HitKind {
    Application,
    File,
    Folder,
    Image,
    Screenshot,
    Document,
    Audio,
    Video,
}

/// One filesystem (or application) entry to index. All strings are UTF-8
/// paths as produced by [`std::path::Path::to_string_lossy`] callers.
#[derive(Debug, Clone, Default)]
pub struct EntryRecord {
    pub root_id: i64,
    pub parent_id: Option<i64>,
    pub path: String,
    pub name: String,
    pub ext: String,
    pub is_dir: bool,
    pub size: u64,
    /// Unix seconds (mtime).
    pub mtime: i64,
    pub volume_serial: u32,
    pub file_id: u64,
    /// True for catalogued applications (shortcut or executable).
    pub is_app: bool,
    /// Resolved launch target for applications (exe behind a shortcut).
    pub app_target: String,
    /// Extra searchable keywords (application target names etc.).
    pub keywords: String,
}

impl EntryRecord {
    pub fn kind(&self) -> EntryKind {
        if self.is_app {
            return EntryKind::Application;
        }
        if self.is_dir {
            return EntryKind::Folder;
        }
        kind_for_ext(&self.ext)
    }
}

/// Extension (lowercase, no dot) → entry kind.
pub fn kind_for_ext(ext: &str) -> EntryKind {
    let ext = ext.to_ascii_lowercase();
    match ext.as_str() {
        "jpg" | "jpeg" | "png" | "webp" | "heic" | "heif" | "avif" | "tiff" | "tif" | "bmp"
        | "gif" | "ico" | "raw" | "cr2" | "cr3" | "nef" | "arw" | "dng" | "orf" | "rw2" => {
            EntryKind::Image
        }
        "mp3" | "wav" | "flac" | "m4a" | "aac" | "ogg" | "wma" | "opus" | "aiff" => EntryKind::Audio,
        "mp4" | "mkv" | "mov" | "avi" | "webm" | "wmv" | "m4v" | "mpg" | "mpeg" => EntryKind::Video,
        "pdf" | "doc" | "docx" | "odt" | "rtf" | "txt" | "md" | "xls" | "xlsx" | "ppt" | "pptx"
        | "csv" | "epub" | "mobi" | "one" | "pages" | "numbers" | "key" => EntryKind::Document,
        _ => EntryKind::File,
    }
}

impl EntryKind {
    pub fn hit_kind(self) -> HitKind {
        match self {
            EntryKind::Folder => HitKind::Folder,
            EntryKind::Application => HitKind::Application,
            EntryKind::Image => HitKind::Image,
            EntryKind::Screenshot => HitKind::Screenshot,
            EntryKind::Document => HitKind::Document,
            EntryKind::Audio => HitKind::Audio,
            EntryKind::Video => HitKind::Video,
            EntryKind::File => HitKind::File,
        }
    }
}

/// A retrieved hit, already ranked.
#[derive(Debug, Clone)]
pub struct SearchHit {
    pub entry_id: i64,
    pub kind: HitKind,
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    pub score: f64,
    /// Short human-readable match reason ("Name starts with query", ...).
    pub explanation: String,
}

/// Search request against the index (calculator and settings results are
/// synthesized by the engine, not stored).
#[derive(Debug, Clone, Default)]
pub struct SearchRequest {
    pub text: String,
    /// Restrict to applications (`Some(true)`) or to non-apps (`Some(false)`).
    pub apps_only: bool,
    pub files_only: bool,
    /// Restrict to these hit kinds; empty means all.
    pub kinds: Vec<HitKind>,
    /// Maximum hits returned after ranking.
    pub limit: usize,
}

/// Outcome of upserting one entry (used for counters and test assertions).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpsertOutcome {
    Inserted,
    Updated,
    Unchanged,
}

/// Kind string for name-embedding segments (Phase 3: one per entry).
/// Stored in `segments.kind` / `extract_jobs.kind`.
pub const SEGMENT_KIND_NAME: &str = "name";

/// Priority for `extract_jobs.priority`: new files first, LIFO within priority.
///
/// `claim_extract_job` orders by `priority DESC, id DESC`: higher priority
/// drains first, and within one priority the most recently enqueued job wins
/// (LIFO). Newly inserted entries (`UpsertOutcome::Inserted`) enqueue at
/// `EXTRACT_PRIORITY_NEW`; updates, re-embeds, and model-swap requeues use
/// `EXTRACT_PRIORITY_UPDATED` so fresh discoveries jump the queue.
pub const EXTRACT_PRIORITY_NEW: i64 = 10;
pub const EXTRACT_PRIORITY_UPDATED: i64 = 0;

/// How many name texts the winsearchd embed scheduler batches into one
/// worker round-trip (P3.5). Well under `MAX_TEXTS_PER_JOB` (256) so one
/// pipeline batch is always one worker job.
pub const EMBED_BATCH_SIZE: usize = 128;

/// States for `extract_jobs.state`.
pub const EXTRACT_STATE_PENDING: &str = "pending";
pub const EXTRACT_STATE_CLAIMED: &str = "claimed";
pub const EXTRACT_STATE_FAILED: &str = "failed";

/// One embedded segment: a chunk of an entry with its vector.
///
/// Vectors are stored in SQLite as FP16 little-endian bytes plus the L2 norm
/// of the original full-precision vector. In Rust they are exposed decoded as
/// normalized `f32` values; `norm` is `0.0` for zero vectors.
#[derive(Debug, Clone, PartialEq)]
pub struct Segment {
    pub id: i64,
    pub entry_id: i64,
    pub kind: String,
    pub chunk_index: i64,
    pub text: String,
    pub page: Option<i64>,
    pub start_ms: Option<i64>,
    pub end_ms: Option<i64>,
    pub line: Option<i64>,
    /// Normalized embedding (unit length unless `norm == 0.0`).
    pub vector: Vec<f32>,
    /// L2 norm of the original vector before normalization.
    pub norm: f32,
}

/// Input for inserting or replacing a segment. The vector is full-precision
/// `f32`; the store normalizes it, encodes FP16, and persists the norm.
#[derive(Debug, Clone)]
pub struct NewSegment<'a> {
    pub entry_id: i64,
    pub kind: &'a str,
    pub chunk_index: i64,
    pub text: &'a str,
    pub page: Option<i64>,
    pub start_ms: Option<i64>,
    pub end_ms: Option<i64>,
    pub line: Option<i64>,
    pub vector: &'a [f32],
}

/// One row of the `extract_jobs` queue (reused by Phase 4 extractors).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractJob {
    pub id: i64,
    pub entry_id: i64,
    pub kind: String,
    pub state: String,
    pub claimed: bool,
    pub attempts: i64,
    /// Scheduling priority: higher drains first (`priority DESC, id DESC`
    /// claim order gives new-files-first, LIFO within priority).
    pub priority: i64,
}

/// Filter for vector search. Empty lists mean "no restriction".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VectorFilter {
    /// Restrict to these segment kinds; empty means all kinds.
    pub kinds: Vec<String>,
    /// Restrict to these entries; empty means all entries.
    pub entry_ids: Vec<i64>,
}

impl VectorFilter {
    /// Convenience for a single-kind filter (e.g. `name` segments).
    pub fn for_kind(kind: &str) -> Self {
        Self {
            kinds: vec![kind.to_string()],
            entry_ids: Vec::new(),
        }
    }

    pub fn matches_kind(&self, kind: &str) -> bool {
        self.kinds.is_empty() || self.kinds.iter().any(|k| k == kind)
    }

    pub fn matches_entry(&self, entry_id: i64) -> bool {
        self.entry_ids.is_empty() || self.entry_ids.contains(&entry_id)
    }
}

/// A segment paired with its cosine similarity to the query.
#[derive(Debug, Clone, PartialEq)]
pub struct ScoredSegment {
    pub segment: Segment,
    pub cosine: f32,
}

/// Splits a path into its file name (lossy UTF-8).
pub fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

/// Lowercase extension without the dot.
pub fn extension_of(name: &str) -> String {
    Path::new(name)
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ext_mapping_covers_media_and_documents() {
        assert_eq!(kind_for_ext("PNG"), EntryKind::Image);
        assert_eq!(kind_for_ext("mp3"), EntryKind::Audio);
        assert_eq!(kind_for_ext("mkv"), EntryKind::Video);
        assert_eq!(kind_for_ext("docx"), EntryKind::Document);
        assert_eq!(kind_for_ext("rs"), EntryKind::File);
        assert_eq!(kind_for_ext(""), EntryKind::File);
    }

    #[test]
    fn entry_kind_priority() {
        let rec = EntryRecord {
            root_id: 1,
            parent_id: None,
            path: "C:\\x\\Code.lnk".into(),
            name: "Code.lnk".into(),
            ext: "lnk".into(),
            is_dir: false,
            size: 1,
            mtime: 0,
            volume_serial: 0,
            file_id: 0,
            is_app: true,
            app_target: String::new(),
            keywords: String::new(),
        };
        assert_eq!(rec.kind(), EntryKind::Application);
    }
}
