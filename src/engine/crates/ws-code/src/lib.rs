//! `ws-code`: repository detection, `.gitignore`-aware source crawling, and the
//! standalone `code.db` SQLite store (files, chunks, symbols, FTS5, and FP16
//! chunk vectors).
//!
//! Isolated by design: `code.db` (`ws_platform::paths::code_db_path()`) never
//! shares tables or vectors with `index.db`.

pub mod crawl;
pub mod model;
pub mod store;

pub use crawl::{
    crawl_repo, detect_repos, detect_repos_with_depth, file_identity, has_git_marker,
    is_excluded_code_dir, is_excluded_code_file, is_probably_binary, record_for_code_path,
    EXCLUDED_CODE_DIRS, EXCLUDED_CODE_EXTS, EXCLUDED_CODE_FILES, EXCLUDED_CODE_SUFFIXES,
    MAX_CODE_FILE_BYTES,
};
pub use model::{
    language_for_file, CodeChunk, CodeFileRecord, CodeFileRow, CodeSymbol, DetectedRepo,
    FileUpsertOutcome, NewCodeChunk, NewCodeSymbol, RepoRow, ScoredCodeChunk, SymbolRole,
    CODE_SCHEMA_VERSION, EMBEDDING_DIM,
};
pub use store::{CodeIndexStats, CodeStore, CodeStoreError};
