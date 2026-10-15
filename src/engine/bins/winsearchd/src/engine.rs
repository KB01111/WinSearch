//! Engine service: shared state behind the IPC server, and the search
//! pipeline that turns a `SearchQuery` into streamed result batches.
//!
//! Search sources, in result order: calculator (if the query is arithmetic),
//! Windows settings pages, then index hits (applications and files/folders)
//! ranked by `ws-index`. Semantic retrieval joins in Phase 3 and appends its
//! results in later batches.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tracing::info;
use ws_index::model::{SearchRequest, HitKind};
use ws_index::{evaluate, search_settings};
use ws_platform::paths;
use ws_platform::pipe::PipeStream;
use ws_protocol::{
    write_message, EngineRunState, EngineStatus, FrameError, MatchLocation, RequestId, ResultKind,
    SearchMode, SearchQuery, SearchResult, ServerMessage,
};

use crate::indexer::Indexer;
use crate::model::ModelService;

/// A connected client that can receive pushed messages.
#[derive(Clone)]
pub struct ClientHandle {
    id: u64,
    stream: Arc<Mutex<PipeStream>>,
}

impl ClientHandle {
    pub fn new(id: u64, stream: Arc<Mutex<PipeStream>>) -> Self {
        Self { id, stream }
    }

    pub fn send(&self, msg: &ServerMessage) -> Result<(), FrameError> {
        let mut guard = self
            .stream
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        write_message(&mut *guard, msg)
    }
}

pub struct EngineService {
    status: Mutex<EngineStatus>,
    clients: Mutex<Vec<ClientHandle>>,
    /// Cancellation flags for in-flight search requests.
    cancels: Mutex<HashMap<RequestId, Arc<AtomicBool>>>,
    next_client_id: std::sync::atomic::AtomicU64,
    indexer: Arc<Indexer>,
    models: Arc<ModelService>,
}

pub type SharedEngine = Arc<EngineService>;

impl EngineService {
    pub fn new(indexer: Arc<Indexer>) -> SharedEngine {
        Self::new_with_model_dir(indexer, paths::model_dir())
    }

    /// Test hook: same engine with an isolated model directory.
    pub fn new_with_model_dir(indexer: Arc<Indexer>, model_dir: std::path::PathBuf) -> SharedEngine {
        let models = ModelService::new(model_dir, Arc::clone(&indexer));
        Arc::new(Self {
            status: Mutex::new(EngineStatus {
                state: EngineRunState::Idle,
                current_activity: String::new(),
                ..Default::default()
            }),
            clients: Mutex::new(Vec::new()),
            cancels: Mutex::new(HashMap::new()),
            next_client_id: std::sync::atomic::AtomicU64::new(1),
            indexer,
            models,
        })
    }

    pub fn model_service(&self) -> &Arc<ModelService> {
        &self.models
    }

    pub fn status(&self) -> EngineStatus {
        let mut status = self
            .status
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        // Indexing counters are live; the stored snapshot only carries the
        // broadcast state.
        if let Ok(s) = self.indexer.status() {
            status.files_discovered = s.files_discovered;
            status.files_indexed = s.files_indexed;
            status.queue_depth = s.queue_depth;
            status.index_size_bytes = s.stats.index_size_bytes;
            if status.state == EngineRunState::Idle && s.indexing {
                status.state = EngineRunState::Indexing;
                status.current_activity = "Indexing".into();
            }
        }
        status
    }

    pub fn set_status(&self, update: impl FnOnce(&mut EngineStatus)) {
        let mut guard = self.status.lock().unwrap_or_else(|p| p.into_inner());
        update(&mut guard);
        drop(guard);
        let snapshot = self.status();
        self.broadcast(&ServerMessage::Status {
            id: None,
            status: snapshot,
        });
    }

    pub fn register_client(&self, stream: Arc<Mutex<PipeStream>>) -> ClientHandle {
        let id = self.next_client_id.fetch_add(1, Ordering::Relaxed);
        let handle = ClientHandle::new(id, stream);
        self.clients
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(handle.clone());
        handle
    }

    pub fn unregister_client(&self, client: &ClientHandle) {
        self.clients
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .retain(|c| c.id != client.id);
    }

    fn broadcast(&self, msg: &ServerMessage) {
        self.push_to_all(msg);
    }

    /// Sends one message to every connected client (used by the model
    /// service's progress pushes as well as status broadcasts).
    pub fn push_to_all(&self, msg: &ServerMessage) {
        let clients = self.clients.lock().unwrap_or_else(|p| p.into_inner());
        for client in clients.iter() {
            if let Err(e) = client.send(msg) {
                info!(client = client.id, error = %e, "dropping unreachable client");
            }
        }
    }

    pub fn begin_request(&self, id: RequestId) -> Arc<AtomicBool> {
        let flag = Arc::new(AtomicBool::new(false));
        self.cancels
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(id, flag.clone());
        flag
    }

    pub fn end_request(&self, id: RequestId) {
        self.cancels
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&id);
    }

    pub fn cancel(&self, id: RequestId) {
        if let Some(flag) = self
            .cancels
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&id)
        {
            flag.store(true, Ordering::Relaxed);
        }
    }

    // ---- model management (P3.3) ----

    fn answer_model_status(&self, client: &ClientHandle, id: RequestId) {
        let _ = client.send(&ServerMessage::ModelStatus {
            id: Some(id),
            status: self.models.status(),
        });
    }

    pub fn model_download_start(&self, client: &ClientHandle, id: RequestId) {
        self.models.start_download();
        self.answer_model_status(client, id);
    }

    pub fn model_download_cancel(&self, client: &ClientHandle, id: RequestId) {
        self.models.cancel_download();
        self.answer_model_status(client, id);
    }

    pub fn model_import(&self, client: &ClientHandle, id: RequestId, path: String) {
        match self.models.start_import(std::path::PathBuf::from(path)) {
            Ok(_) => self.answer_model_status(client, id),
            Err(e) => {
                let _ = client.send(&ServerMessage::Error {
                    id: Some(id),
                    code: ws_protocol::ErrorCode::InvalidRequest,
                    message: e,
                });
            }
        }
    }

    pub fn model_delete(&self, client: &ClientHandle, id: RequestId) {
        match self.models.delete() {
            Ok(status) => {
                let _ = client.send(&ServerMessage::ModelStatus {
                    id: Some(id),
                    status,
                });
            }
            Err(e) => {
                let _ = client.send(&ServerMessage::Error {
                    id: Some(id),
                    code: ws_protocol::ErrorCode::InvalidRequest,
                    message: e,
                });
            }
        }
    }

    pub fn model_status(&self, client: &ClientHandle, id: RequestId) {
        self.answer_model_status(client, id);
    }

    /// Executes one search, streaming result batches to the client. Exact
    /// results stream first; a cancelled request closes immediately with an
    /// empty terminal batch so the client can retire it.
    pub fn search(&self, client: &ClientHandle, id: RequestId, query: SearchQuery) {
        let cancel = self.begin_request(id);
        let outcome = self.run_search(client, id, &query, &cancel);
        self.end_request(id);
        if let Err(e) = outcome {
            let _ = client.send(&ServerMessage::Error {
                id: Some(id),
                code: ws_protocol::ErrorCode::Internal,
                message: e.to_string(),
            });
            let _ = client.send(&ServerMessage::Results {
                id,
                batch: Vec::new(),
                done: true,
            });
        }
    }

    fn run_search(
        &self,
        client: &ClientHandle,
        id: RequestId,
        query: &SearchQuery,
        cancel: &AtomicBool,
    ) -> Result<(), FrameError> {
        let results = self.plan_results(query);

        if cancel.load(Ordering::Relaxed) {
            return client.send(&ServerMessage::Results {
                id,
                batch: Vec::new(),
                done: true,
            });
        }

        // Progressive response: the first batch is what the overlay shows;
        // overflow streams after so typing stays responsive.
        let limit = (query.limit.max(1) as usize).min(results.len());
        let (first, rest) = results.split_at(limit);
        client.send(&ServerMessage::Results {
            id,
            batch: first.to_vec(),
            done: rest.is_empty(),
        })?;
        if rest.is_empty() {
            return Ok(());
        }
        if cancel.load(Ordering::Relaxed) {
            return client.send(&ServerMessage::Results {
                id,
                batch: Vec::new(),
                done: true,
            });
        }
        client.send(&ServerMessage::Results {
            id,
            batch: rest.to_vec(),
            done: true,
        })
    }

    /// Computes the ranked result list for one query (no IPC). Split from
    /// `run_search` so the pipeline is directly testable.
    fn plan_results(&self, query: &SearchQuery) -> Vec<SearchResult> {
        let text = query.text.trim();
        if text.is_empty() {
            return Vec::new();
        }

        let mut results = Vec::new();
        let wants_index = query.mode != SearchMode::Code;
        let wants_calc = wants_index
            && (query.categories.is_empty() || query.categories.contains(&ResultKind::Calculator));
        let wants_settings = query.mode != SearchMode::Files
            && (query.categories.is_empty()
                || query.categories.contains(&ResultKind::Settings)
                || query.categories.contains(&ResultKind::Action));

        if wants_calc {
            match evaluate(text) {
                Ok(Some(calc)) => results.push(SearchResult {
                    id: "calc".into(),
                    kind: ResultKind::Calculator,
                    title: format!("{} = {}", calc.expression, calc.value_text),
                    subtitle: calc.value_text,
                    match_explanation: "Calculator".into(),
                    score: 1.0,
                    location: Some(MatchLocation::Line { line: 0 }),
                    ..Default::default()
                }),
                Ok(None) => {}
                Err(e) => {
                    // The query is arithmetic but invalid (e.g. 1/0): say so
                    // instead of pretending there are no matches.
                    results.push(SearchResult {
                        id: "calc".into(),
                        kind: ResultKind::Calculator,
                        title: format!("{text} — {e}"),
                        subtitle: String::new(),
                        match_explanation: "Calculator".into(),
                        score: 1.0,
                        ..Default::default()
                    });
                }
            }
        }

        if wants_settings {
            for (i, hit) in search_settings(text, 3).into_iter().enumerate() {
                results.push(SearchResult {
                    id: format!("settings-{i}"),
                    kind: ResultKind::Settings,
                    title: format!("{} — Settings", hit.name),
                    subtitle: hit.uri,
                    match_explanation: "Windows settings".into(),
                    score: 0.8 * hit.score,
                    ..Default::default()
                });
            }
        }

        if wants_index {
            let request = SearchRequest {
                text: text.to_string(),
                apps_only: query.mode == SearchMode::Apps,
                files_only: query.mode == SearchMode::Files,
                kinds: hit_kinds(&query.categories),
                limit: (query.limit.max(1) as usize).saturating_mul(3).clamp(20, 120),
            };
            if let Ok(hits) = self.indexer.search(&request) {
                for hit in hits {
                    results.push(SearchResult {
                        id: format!("e{}", hit.entry_id),
                        kind: to_result_kind(hit.kind),
                        title: hit.name,
                        subtitle: hit.path.clone(),
                        match_explanation: hit.explanation,
                        score: hit.score,
                        path: Some(hit.path),
                        location: None,
                        thumbnail_path: None,
                    });
                }
            }
        }
        results
    }
}

/// Maps protocol category filters to index hit kinds.
fn hit_kinds(categories: &[ResultKind]) -> Vec<HitKind> {
    categories
        .iter()
        .filter_map(|k| match k {
            ResultKind::Application => Some(HitKind::Application),
            ResultKind::File => Some(HitKind::File),
            ResultKind::Folder => Some(HitKind::Folder),
            ResultKind::Image => Some(HitKind::Image),
            ResultKind::Screenshot => Some(HitKind::Screenshot),
            ResultKind::Document => Some(HitKind::Document),
            ResultKind::Audio => Some(HitKind::Audio),
            ResultKind::Video => Some(HitKind::Video),
            _ => None,
        })
        .collect()
}

fn to_result_kind(kind: HitKind) -> ResultKind {
    match kind {
        HitKind::Application => ResultKind::Application,
        HitKind::File => ResultKind::File,
        HitKind::Folder => ResultKind::Folder,
        HitKind::Image => ResultKind::Image,
        HitKind::Screenshot => ResultKind::Screenshot,
        HitKind::Document => ResultKind::Document,
        HitKind::Audio => ResultKind::Audio,
        HitKind::Video => ResultKind::Video,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use ws_index::IndexStore;

    struct TempTree(PathBuf);

    impl TempTree {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!(
                "winsearch-engine-test-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for TempTree {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn service_with(tree: &TempTree, files: &[&str]) -> SharedEngine {
        for f in files {
            fs::write(tree.0.join(f), b"x").unwrap();
        }
        let store = IndexStore::open_in_memory().unwrap();
        let indexer = Indexer::open(store, &[tree.0.clone()]).unwrap();
        indexer.enqueue_initial_scan().unwrap();
        while indexer.scan_available(64) > 0 {}
        EngineService::new(indexer)
    }

    fn query(text: &str) -> SearchQuery {
        SearchQuery {
            text: text.into(),
            limit: 20,
            ..Default::default()
        }
    }

    #[test]
    fn calculator_results_are_synthesized() {
        let tree = TempTree::new();
        let engine = service_with(&tree, &[]);
        let results = engine.plan_results(&query("2 + 2 * 3"));
        assert_eq!(results[0].kind, ResultKind::Calculator);
        assert_eq!(results[0].subtitle, "8");
    }

    #[test]
    fn calculator_errors_are_reported_honestly() {
        let tree = TempTree::new();
        let engine = service_with(&tree, &[]);
        let results = engine.plan_results(&query("1 / 0"));
        assert_eq!(results[0].kind, ResultKind::Calculator);
        assert!(results[0].title.contains("division by zero"));
    }

    #[test]
    fn settings_results_are_synthesized() {
        let tree = TempTree::new();
        let engine = service_with(&tree, &[]);
        let results = engine.plan_results(&query("windows update"));
        assert!(results.iter().any(|r| r.kind == ResultKind::Settings));
    }

    #[test]
    fn file_hits_come_from_the_index() {
        let tree = TempTree::new();
        let engine = service_with(&tree, &["meeting-notes.txt"]);
        let results = engine.plan_results(&query("meeting"));
        assert!(
            results.iter().any(|r| r.title == "meeting-notes.txt"
                && matches!(r.kind, ResultKind::File | ResultKind::Document)),
            "got {results:?}"
        );
    }

    #[test]
    fn code_mode_returns_no_file_results() {
        let tree = TempTree::new();
        let engine = service_with(&tree, &["main.rs"]);
        let mut q = query("main");
        q.mode = SearchMode::Code;
        let results = engine.plan_results(&q);
        assert!(results.iter().all(|r| r.kind != ResultKind::File));
    }

    #[test]
    fn category_filters_restrict_kinds() {
        let tree = TempTree::new();
        let engine = service_with(&tree, &["report.pdf"]);
        let mut q = query("report");
        q.categories = vec![ResultKind::Image];
        let results = engine.plan_results(&q);
        assert!(results.iter().all(|r| r.kind != ResultKind::Document));
    }
}
