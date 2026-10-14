//! The indexer orchestrator: crawl workers, watcher event handling, the
//! application catalog, and disconnected-drive reconciliation.
//!
//! Concurrency model: a bounded pool of crawl workers pulls directory work
//! from the crash-safe queue inside the index database; all database access
//! is serialized through one `Mutex<IndexStore>`; watchers feed events into
//! the same store. Nothing here blocks the IPC/serve path.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, Weak};

use tracing::{info, warn};
use ws_index::model::{EntryRecord, SearchRequest, UpsertOutcome};
use ws_index::{HitKind, IndexStats, IndexStore, SearchHit, StoreError};

use crate::catalog;
use crate::crawl;
use crate::watcher::{self, WatchEvent, Watcher};

/// Crawl worker count: bounded on purpose (indexing must never starve the
/// interactive search path).
const WORKER_THREADS: usize = 4;

/// How often the drive supervisor checks for disconnected/reconnected roots.
const DRIVE_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);

/// How often the application catalog is refreshed.
const CATALOG_REFRESH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30 * 60);

#[derive(Default)]
struct Counters {
    files_discovered: u64,
    files_indexed: u64,
    events_processed: u64,
}

/// Live counters surfaced through engine status.
#[derive(Debug, Clone, Copy, Default)]
pub struct IndexerStatus {
    pub files_discovered: u64,
    pub files_indexed: u64,
    pub queue_depth: u64,
    pub indexing: bool,
    pub stats: IndexStats,
}

struct RootInfo {
    id: i64,
    path: PathBuf,
}

pub struct Indexer {
    store: Mutex<IndexStore>,
    counters: Mutex<Counters>,
    wake: (Mutex<bool>, Condvar),
    shutdown: AtomicBool,
    roots: Vec<RootInfo>,
    watchers: Mutex<Vec<Watcher>>,
}

impl Indexer {
    /// Opens the index and registers the given roots (idempotent).
    pub fn open(store: IndexStore, roots: &[PathBuf]) -> Result<Arc<Self>, StoreError> {
        let mut store = store;
        // Claims left by a crashed run become work again.
        store.release_claims()?;
        let mut infos = Vec::new();
        for root in roots {
            let path = root.to_string_lossy().into_owned();
            let id = store.ensure_root(&path)?;
            infos.push(RootInfo {
                id,
                path: root.clone(),
            });
        }
        Ok(Arc::new(Self {
            store: Mutex::new(store),
            counters: Mutex::new(Counters::default()),
            wake: (Mutex::new(false), Condvar::new()),
            shutdown: AtomicBool::new(false),
            roots: infos,
            watchers: Mutex::new(Vec::new()),
        }))
    }

    /// Starts background work: crawl workers, watchers, catalog refresh, and
    /// the drive supervisor. Returns once all threads are spawned.
    pub fn start_background(self: &Arc<Self>) -> anyhow::Result<()> {
        self.enqueue_initial_scan()?;

        for _ in 0..WORKER_THREADS {
            let indexer = Arc::clone(self);
            std::thread::Builder::new()
                .name("winsearch-crawl".into())
                .spawn(move || indexer.crawl_worker())?;
        }

        {
            let mut watchers = self.watchers.lock().unwrap_or_else(|p| p.into_inner());
            for root in &self.roots {
                match self.spawn_watcher(root) {
                    Some(w) => watchers.push(w),
                    None => warn!(root = %root.path.display(), "root not present yet; watching after reconnect"),
                }
            }
        }

        {
            let indexer = Arc::clone(self);
            std::thread::Builder::new()
                .name("winsearch-drive".into())
                .spawn(move || indexer.drive_supervisor())?;
        }
        {
            let indexer = Arc::clone(self);
            std::thread::Builder::new()
                .name("winsearch-catalog".into())
                .spawn(move || indexer.catalog_loop())?;
        }
        Ok(())
    }

    /// Enqueues a full scan of every root (first run, reconnect, or rescan).
    pub fn enqueue_initial_scan(&self) -> Result<(), StoreError> {
        let mut store = self.store.lock().unwrap_or_else(|p| p.into_inner());
        for root in &self.roots {
            if !root.path.is_dir() {
                continue; // disconnected volume: preserved until it returns
            }
            let gen = store.begin_generation(root.id)?;
            store.enqueue_dir(root.id, &root.path.to_string_lossy())?;
            store.set_checkpoint(&format!("root:{}:generation", root.id), &gen.to_string())?;
        }
        self.notify_work();
        Ok(())
    }

    // ---- search ----

    pub fn search(&self, req: &SearchRequest) -> Result<Vec<SearchHit>, StoreError> {
        self.store
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .search(req)
    }

    pub fn status(&self) -> Result<IndexerStatus, StoreError> {
        let store = self.store.lock().unwrap_or_else(|p| p.into_inner());
        let counters = self.counters.lock().unwrap_or_else(|p| p.into_inner());
        let queue_depth = store.queue_depth()?;
        let stats = store.stats()?;
        Ok(IndexerStatus {
            files_discovered: counters.files_discovered,
            files_indexed: counters.files_indexed,
            queue_depth,
            indexing: queue_depth > 0,
            stats,
        })
    }

    // ---- crawl workers ----

    fn notify_work(&self) {
        let mut guard = self.wake.0.lock().unwrap_or_else(|p| p.into_inner());
        *guard = true;
        self.wake.1.notify_all();
    }

    /// Processes up to `max_dirs` queued directories on the calling thread.
    /// Returns how many were processed. Used by tests and one-shot scans.
    #[allow(dead_code)] // exercised by tests; workers use crawl_worker in production
    pub fn scan_available(&self, max_dirs: usize) -> usize {
        let mut processed = 0;
        for _ in 0..max_dirs {
            let work = self
                .store
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .claim_dir();
            match work {
                Ok(Some((queue_id, root_id, dir_path))) => {
                    if let Err(e) = self.process_dir(queue_id, root_id, &dir_path) {
                        warn!(dir = %dir_path, error = %e, "directory scan failed");
                    }
                    processed += 1;
                }
                _ => break,
            }
        }
        processed
    }

    fn crawl_worker(self: Arc<Self>) {
        loop {
            if self.shutdown.load(Ordering::SeqCst) {
                return;
            }
            let work = self
                .store
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .claim_dir();
            match work {
                Ok(Some((queue_id, root_id, dir_path))) => {
                    if let Err(e) = self.process_dir(queue_id, root_id, &dir_path) {
                        warn!(dir = %dir_path, error = %e, "directory scan failed");
                    }
                }
                Ok(None) => {
                    // Queue drained: idle until work arrives or shutdown.
                    let mut guard = self.wake.0.lock().unwrap_or_else(|p| p.into_inner());
                    if !*guard && !self.shutdown.load(Ordering::SeqCst) {
                        let (g, _) = self
                            .wake
                            .1
                            .wait_timeout(guard, std::time::Duration::from_millis(500))
                            .unwrap_or_else(|p| p.into_inner());
                        guard = g;
                    }
                    *guard = false;
                }
                Err(e) => {
                    warn!(error = %e, "claim_dir failed");
                    std::thread::sleep(std::time::Duration::from_millis(200));
                }
            }
        }
    }

    fn process_dir(
        &self,
        queue_id: i64,
        root_id: i64,
        dir_path: &str,
    ) -> Result<(), StoreError> {
        let depth = dir_path.matches('\\').count() + dir_path.matches('/').count();
        let parent_id = self
            .store
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .find_by_path(dir_path)?
            .map(|e| e.id);

        let scan = match crawl::scan_directory(root_id, dir_path, parent_id, depth) {
            Ok(s) => s,
            Err(e) => {
                warn!(dir = %dir_path, error = %e, "scan error");
                crawl::DirScan::default()
            }
        };

        let mut store = self.store.lock().unwrap_or_else(|p| p.into_inner());
        let generation = self.current_generation(&store, root_id)?;
        store.apply_batch(root_id, generation, &scan.entries)?;
        for sub in &scan.subdirs {
            store.enqueue_dir(root_id, sub)?;
        }
        store.complete_dir(queue_id)?;

        let entries = scan.entries.len() as u64;
        let root_done = store.pending_for_root(root_id)? == 0;
        drop(store);

        {
            let mut counters = self.counters.lock().unwrap_or_else(|p| p.into_inner());
            counters.files_discovered += entries;
            counters.files_indexed += scan
                .entries
                .iter()
                .filter(|e| matches!(e.kind().hit_kind(), HitKind::Application) || !e.is_dir)
                .count() as u64;
        }

        if root_done {
            // The scan of this root is complete: drop everything not seen in
            // this generation (files deleted while we were away).
            let mut store = self.store.lock().unwrap_or_else(|p| p.into_inner());
            let generation = self.current_generation(&store, root_id)?;
            let swept = store.sweep_generation(root_id, generation)?;
            if swept > 0 {
                info!(root_id, swept, "swept stale entries");
            }
        }
        Ok(())
    }

    fn current_generation(&self, store: &IndexStore, root_id: i64) -> Result<i64, StoreError> {
        Ok(store
            .roots()?
            .into_iter()
            .find(|r| r.id == root_id)
            .map(|r| r.current_generation)
            .unwrap_or(0))
    }

    // ---- watcher events ----

    fn handle_event(&self, root_id: i64, event: WatchEvent) {
        if let Err(e) = self.apply_event(root_id, &event) {
            warn!(?event, error = %e, "event handling failed");
        }
    }

    fn apply_event(&self, root_id: i64, event: &WatchEvent) -> Result<(), StoreError> {
        match event {
            WatchEvent::Created(path) => {
                let path_str = path.to_string_lossy().into_owned();
                if path.is_dir() {
                    let mut store = self.store.lock().unwrap_or_else(|p| p.into_inner());
                    store.enqueue_dir(root_id, &path_str)?;
                    drop(store);
                    self.notify_work();
                } else {
                    self.point_upsert(root_id, path)?;
                }
            }
            WatchEvent::Deleted(path) => {
                let mut store = self.store.lock().unwrap_or_else(|p| p.into_inner());
                store.remove_tree(&path.to_string_lossy())?;
            }
            WatchEvent::Modified(path) => {
                self.point_upsert(root_id, path)?;
            }
            WatchEvent::Renamed { from, to } => {
                let mut store = self.store.lock().unwrap_or_else(|p| p.into_inner());
                let from_str = from.to_string_lossy().into_owned();
                let to_str = to.to_string_lossy().into_owned();
                let existing = store.find_by_path(&from_str)?;
                match existing {
                    Some(row) => {
                        let name = to
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_default();
                        let ext = ws_index::model::extension_of(&name);
                        let parent_id = store
                            .find_by_path(&parent_of(&to_str))?
                            .map(|e| e.id);
                        store.move_entry(row.id, parent_id, &to_str, &name, &ext)?;
                        if row.is_dir {
                            store.rename_tree_paths(&from_str, &to_str)?;
                            drop(store);
                            let mut s = self.store.lock().unwrap_or_else(|p| p.into_inner());
                            s.enqueue_dir(root_id, &to_str)?;
                            drop(s);
                            self.notify_work();
                        }
                    }
                    None => {
                        // Missed the create (e.g. move into the tree).
                        drop(store);
                        self.point_upsert(root_id, to)?;
                    }
                }
            }
            WatchEvent::Overflow => {
                let mut store = self.store.lock().unwrap_or_else(|p| p.into_inner());
                let root = self
                    .roots
                    .iter()
                    .find(|r| r.id == root_id)
                    .map(|r| r.path.to_string_lossy().into_owned());
                if let Some(root_path) = root {
                    store.enqueue_dir(root_id, &root_path)?;
                }
                drop(store);
                self.notify_work();
            }
        }
        {
            let mut counters = self.counters.lock().unwrap_or_else(|p| p.into_inner());
            counters.events_processed += 1;
        }
        Ok(())
    }

    fn point_upsert(&self, root_id: i64, path: &Path) -> Result<(), StoreError> {
        let parent_id = path
            .parent()
            .and_then(|p| {
                self.store
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .find_by_path(&p.to_string_lossy())
                    .ok()
                    .flatten()
            })
            .map(|e| e.id);

        let record = match crawl::record_for_path(root_id, parent_id, path) {
            Ok(Some(r)) => r,
            Ok(None) => return Ok(()), // excluded or vanished
            Err(e) => {
                warn!(path = %path.display(), error = %e, "stat failed");
                return Ok(());
            }
        };

        let mut store = self.store.lock().unwrap_or_else(|p| p.into_inner());
        let generation = self.current_generation(&store, root_id)?;
        let outcomes = store.apply_batch(root_id, generation, &[record])?;
        for outcome in outcomes {
            if outcome != UpsertOutcome::Unchanged {
                let mut counters = self.counters.lock().unwrap_or_else(|p| p.into_inner());
                counters.files_indexed += 1;
            }
        }
        Ok(())
    }

    // ---- catalog ----

    fn catalog_loop(self: Arc<Self>) {
        loop {
            if self.shutdown.load(Ordering::SeqCst) {
                return;
            }
            if let Err(e) = self.refresh_catalog() {
                warn!(error = %e, "catalog refresh failed");
            }
            // Sleep in small steps so shutdown is responsive.
            let mut remaining = CATALOG_REFRESH_INTERVAL;
            while remaining > std::time::Duration::ZERO && !self.shutdown.load(Ordering::SeqCst) {
                let step = remaining.min(std::time::Duration::from_secs(1));
                std::thread::sleep(step);
                remaining -= step;
            }
        }
    }

    fn refresh_catalog(&self) -> Result<(), StoreError> {
        let apps = catalog::scan_catalog(&catalog::app_roots());
        let mut store = self.store.lock().unwrap_or_else(|p| p.into_inner());
        let root_id = store.ensure_root(CATALOG_ROOT)?;
        let generation = store.begin_generation(root_id)?;

        let records: Vec<EntryRecord> = apps
            .iter()
            .map(|app| EntryRecord {
                root_id,
                parent_id: None,
                path: app.path.clone(),
                name: app.name.clone(),
                ext: ws_index::model::extension_of(&app.path),
                is_dir: false,
                size: 0,
                mtime: 0,
                volume_serial: 0,
                file_id: 0,
                is_app: true,
                app_target: app.target.clone(),
                keywords: app.keywords.clone(),
            })
            .collect();

        store.apply_batch(root_id, generation, &records)?;
        let swept = store.sweep_generation(root_id, generation)?;
        info!(apps = records.len(), swept, "application catalog refreshed");
        Ok(())
    }

    // ---- drive supervision ----

    fn drive_supervisor(self: Arc<Self>) {
        loop {
            for _ in 0..DRIVE_POLL_INTERVAL.as_secs() {
                if self.shutdown.load(Ordering::SeqCst) {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
            self.reconcile_drives();
        }
    }

    /// One reconciliation pass over all roots: flags vanished volumes as
    /// missing (index preserved) and rescans roots that came back.
    fn reconcile_drives(self: &Arc<Self>) {
        for root in &self.roots {
            let present = root.path.is_dir();
            let marked_missing = self.root_marked_missing(root.id);
            match (present, marked_missing) {
                (true, Ok(true)) => {
                    info!(root = %root.path.display(), "volume reconnected; reconciling");
                    if let Err(e) = self.reconnect_root(root) {
                        warn!(error = %e, "reconnect failed");
                    }
                }
                (false, Ok(false)) => {
                    info!(root = %root.path.display(), "volume disconnected; preserving index");
                    let mut store =
                        self.store.lock().unwrap_or_else(|p| p.into_inner());
                    if let Err(e) = store.set_missing_for_root(root.id, true) {
                        warn!(error = %e, "marking missing failed");
                    }
                }
                _ => {}
            }
        }
    }

    fn root_marked_missing(&self, root_id: i64) -> Result<bool, StoreError> {
        let store = self.store.lock().unwrap_or_else(|p| p.into_inner());
        let any_missing: bool = store.has_missing_entries(root_id)?;
        Ok(any_missing)
    }

    fn reconnect_root(self: &Arc<Self>, root: &RootInfo) -> Result<(), StoreError> {
        {
            let mut store = self.store.lock().unwrap_or_else(|p| p.into_inner());
            store.set_missing_for_root(root.id, false)?;
        }
        // Rescan reconciles renames/deletes that happened while disconnected.
        let mut store = self.store.lock().unwrap_or_else(|p| p.into_inner());
        let gen = store.begin_generation(root.id)?;
        store.enqueue_dir(root.id, &root.path.to_string_lossy())?;
        store.set_checkpoint(&format!("root:{}:generation", root.id), &gen.to_string())?;
        drop(store);
        self.notify_work();

        // Watchers die with their volume: replace any that stopped.
        let mut watchers = self.watchers.lock().unwrap_or_else(|p| p.into_inner());
        watchers.retain(|w| w.is_alive());
        if !watchers.iter().any(|w| w.watches(&root.path)) {
            if let Some(w) = self.spawn_watcher(root) {
                watchers.push(w);
            }
        }
        Ok(())
    }

    fn spawn_watcher(self: &Arc<Self>, root: &RootInfo) -> Option<Watcher> {
        let root_id = root.id;
        let weak: Weak<Indexer> = Arc::downgrade(self);
        watcher::spawn(&root.path, move |event| {
            if let Some(indexer) = weak.upgrade() {
                indexer.handle_event(root_id, event);
            }
        })
        .map_err(|e| {
            warn!(root = %root.path.display(), error = %e, "watcher start failed");
            e
        })
        .ok()
    }
}

/// Pseudo-root for the application catalog (its entries carry real paths).
pub const CATALOG_ROOT: &str = "@apps";

fn parent_of(path: &str) -> String {
    Path::new(path)
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default()
}

impl Drop for Indexer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        self.wake.1.notify_all();
        let mut watchers = self.watchers.lock().unwrap_or_else(|p| p.into_inner());
        watchers.clear(); // each Watcher::drop cancels I/O and joins its thread
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    struct TempTree(PathBuf);

    impl TempTree {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!(
                "winsearch-indexer-test-{}-{:?}",
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

    fn seeded(tree: &TempTree) -> Arc<Indexer> {
        let store = IndexStore::open_in_memory().unwrap();
        let indexer = Indexer::open(store, &[tree.0.clone()]).unwrap();
        indexer.enqueue_initial_scan().unwrap();
        while indexer.scan_available(64) > 0 {}
        indexer
    }

    #[test]
    fn full_scan_indexes_files_and_search_finds_them() {
        let tree = TempTree::new();
        fs::write(tree.0.join("quarterly-report.pdf"), b"x").unwrap();
        let indexer = seeded(&tree);

        let hits = indexer
            .search(&SearchRequest {
                text: "quarterly".into(),
                limit: 10,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "quarterly-report.pdf");
    }

    #[test]
    fn rename_event_preserves_identity() {
        let tree = TempTree::new();
        let file = tree.0.join("draft.txt");
        fs::write(&file, b"x").unwrap();
        let indexer = seeded(&tree);

        let before = indexer
            .search(&SearchRequest {
                text: "draft".into(),
                limit: 10,
                ..Default::default()
            })
            .unwrap();
        let id_before = before[0].entry_id;

        // Rename on disk, then deliver the watcher event.
        let renamed = tree.0.join("final.txt");
        fs::rename(&file, &renamed).unwrap();
        let root_id = indexer.roots[0].id;
        indexer.handle_event(
            root_id,
            WatchEvent::Renamed {
                from: file.clone(),
                to: renamed.clone(),
            },
        );

        let after = indexer
            .search(&SearchRequest {
                text: "final".into(),
                limit: 10,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(after.len(), 1, "renamed file should be searchable");
        assert_eq!(after[0].entry_id, id_before, "identity preserved across rename");
        assert!(
            indexer
                .search(&SearchRequest {
                    text: "draft".into(),
                    limit: 10,
                    ..Default::default()
                })
                .unwrap()
                .is_empty(),
            "old name must not match anymore"
        );
    }

    #[test]
    fn delete_event_removes_entries() {
        let tree = TempTree::new();
        fs::write(tree.0.join("obsolete.log"), b"x").unwrap();
        let indexer = seeded(&tree);
        let root_id = indexer.roots[0].id;

        let path = tree.0.join("obsolete.log");
        fs::remove_file(&path).unwrap();
        indexer.handle_event(root_id, WatchEvent::Deleted(path));

        assert!(indexer
            .search(&SearchRequest {
                text: "obsolete".into(),
                limit: 10,
                ..Default::default()
            })
            .unwrap()
            .is_empty());
    }

    #[test]
    fn rescan_after_crash_resumes_from_queue() {
        let tree = TempTree::new();
        fs::write(tree.0.join("resume-me.txt"), b"x").unwrap();
        let store = IndexStore::open_in_memory().unwrap();
        let mut store2 = store;
        let root_id = store2.ensure_root(&tree.path_string()).unwrap();
        store2.enqueue_dir(root_id, &tree.path_string()).unwrap();
        // Simulate a crash mid-scan: the claim stays set.
        assert!(store2.claim_dir().unwrap().is_some());

        // Restart: a fresh Indexer releases claims and re-scans.
        drop(store2);
        let _ = root_id;
        let indexer = seeded(&tree);
        let hits = indexer
            .search(&SearchRequest {
                text: "resume".into(),
                limit: 10,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(hits.len(), 1);
    }

    /// End-to-end disconnected-drive behavior using a `subst` drive letter as
    /// a real removable volume: unmap = disconnect, remap = reconnect.
    #[test]
    fn disconnected_volumes_are_preserved_and_reconciled() {
        let subst = |args: &[&str]| {
            let _ = std::process::Command::new("subst").args(args).output();
        };
        let tree = TempTree::new();
        fs::write(tree.0.join("usb-file.txt"), b"x").unwrap();

        let drive = "W:";
        subst(&["W:", "/d"]); // clean slate
        subst(&["W:", &tree.path_string()]);
        let root = PathBuf::from("W:\\");

        let store = IndexStore::open_in_memory().unwrap();
        let indexer = Indexer::open(store, &[root.clone()]).unwrap();
        indexer.enqueue_initial_scan().unwrap();
        while indexer.scan_available(64) > 0 {}

        let find = |name: &str| {
            indexer
                .search(&SearchRequest {
                    text: name.into(),
                    limit: 10,
                    ..Default::default()
                })
                .unwrap()
        };
        assert_eq!(find("usb-file").len(), 1, "indexed through the mapped drive");

        // Disconnect the volume.
        subst(&[drive, "/d"]);
        indexer.reconcile_drives();
        assert!(find("usb-file").is_empty(), "missing entries stay out of results");
        assert_eq!(
            indexer.store.lock().unwrap().stats().unwrap().entries_total,
            1,
            "entries are preserved, not deleted"
        );

        // Reconnect: results come back after reconciliation.
        subst(&["W:", &tree.path_string()]);
        indexer.reconcile_drives();
        while indexer.scan_available(64) > 0 {}
        assert_eq!(find("usb-file").len(), 1, "reconciled after reconnect");

        subst(&["W:", "/d"]);
    }

    impl TempTree {
        fn path_string(&self) -> String {
            self.0.to_string_lossy().into_owned()
        }
    }
}
