//! SQLite/FTS5-backed search index for WinSearch.
//!
//! This crate owns everything that touches the index database: schema and
//! migrations, entry upserts with filesystem identity, exact (name/path)
//! retrieval with launcher-style ranking, the calculator evaluator, and the
//! Windows settings URI catalog. It has no Windows dependencies so the whole
//! retrieval path is testable on any machine; filesystem walking and watching
//! live in the `winsearchd` binary.

pub mod calc;
pub mod model;
pub mod search;
pub mod settings;
pub mod store;
pub mod vectors;

pub use calc::{evaluate, CalcError, CalcResult};
pub use model::{
    EntryKind, EntryRecord, ExtractJob, HitKind, NewSegment, ScoredSegment, SearchHit, SearchRequest,
    UpsertOutcome, VectorFilter, Segment, EXTRACT_STATE_CLAIMED, EXTRACT_STATE_FAILED,
    EXTRACT_STATE_PENDING, SEGMENT_KIND_NAME,
};
pub use settings::{search_settings, SettingsHit};
pub use store::{IndexStats, IndexStore, Root, StoreError};
pub use vectors::{VectorError, VectorHit, VectorIndex};
