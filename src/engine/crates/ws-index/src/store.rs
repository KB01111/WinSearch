//! SQLite storage for the index: schema, entry upserts with filesystem
//! identity, the crash-safe crawl queue, and checkpoints.
//!
//! Durability model: WAL journal with `synchronous=NORMAL`. All writes for one
//! logical step (a directory's entries, a queue pop) commit in one
//! transaction, so an abrupt crash at any point leaves the database
//! consistent and the crawl resumable.

use std::path::Path;

use rusqlite::{params, Connection, OptionalExtension};

use crate::model::{EntryRecord, UpsertOutcome};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
}

/// A configured indexing root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Root {
    pub id: i64,
    pub path: String,
    pub enabled: bool,
    pub current_generation: i64,
}

/// Aggregate index counters (also mirrored into engine status).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IndexStats {
    pub entries_total: u64,
    pub files_total: u64,
    pub dirs_total: u64,
    pub apps_total: u64,
    pub index_size_bytes: u64,
}

/// One stored entry as seen by search and identity lookups.
#[derive(Debug, Clone)]
pub struct EntryRow {
    pub id: i64,
    pub root_id: i64,
    pub parent_id: Option<i64>,
    pub path: String,
    pub name: String,
    pub name_lower: String,
    pub ext: String,
    pub is_dir: bool,
    pub is_app: bool,
    pub mtime: i64,
    pub missing: bool,
}

pub struct IndexStore {
    conn: Connection,
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS roots (
    id                 INTEGER PRIMARY KEY,
    path               TEXT NOT NULL UNIQUE,
    enabled            INTEGER NOT NULL DEFAULT 1,
    current_generation INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS entries (
    id              INTEGER PRIMARY KEY,
    root_id         INTEGER NOT NULL REFERENCES roots(id) ON DELETE CASCADE,
    parent_id       INTEGER,
    path            TEXT NOT NULL UNIQUE,
    name            TEXT NOT NULL,
    name_lower      TEXT NOT NULL,
    ext             TEXT NOT NULL DEFAULT '',
    is_dir          INTEGER NOT NULL DEFAULT 0,
    size            INTEGER NOT NULL DEFAULT 0,
    mtime           INTEGER NOT NULL DEFAULT 0,
    volume_serial   INTEGER NOT NULL DEFAULT 0,
    file_id         INTEGER NOT NULL DEFAULT 0,
    is_app          INTEGER NOT NULL DEFAULT 0,
    app_target      TEXT NOT NULL DEFAULT '',
    missing         INTEGER NOT NULL DEFAULT 0,
    scan_generation INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_entries_parent ON entries(parent_id);
CREATE INDEX IF NOT EXISTS idx_entries_file   ON entries(volume_serial, file_id);
CREATE INDEX IF NOT EXISTS idx_entries_root   ON entries(root_id, scan_generation);
CREATE INDEX IF NOT EXISTS idx_entries_name   ON entries(name_lower);

CREATE VIRTUAL TABLE IF NOT EXISTS entries_fts USING fts5(
    name,
    keywords,
    tokenize = 'unicode61 remove_diacritics 2'
);

-- Trigram index for whole-string substring matching ("code" -> "VSCode").
-- Also accelerates LIKE on name_lower for callers that need it.
CREATE VIRTUAL TABLE IF NOT EXISTS entries_tri USING fts5(
    name,
    tokenize = 'trigram case_sensitive 0'
);

CREATE TABLE IF NOT EXISTS crawl_queue (
    id      INTEGER PRIMARY KEY AUTOINCREMENT,
    root_id INTEGER NOT NULL,
    dir_path TEXT NOT NULL,
    claimed INTEGER NOT NULL DEFAULT 0,
    UNIQUE(root_id, dir_path)
);

CREATE TABLE IF NOT EXISTS checkpoints (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
"#;

impl IndexStore {
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        Self::init(Connection::open(path)?)
    }

    pub fn open_in_memory() -> Result<Self, StoreError> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self, StoreError> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        // Names are stored and queried lowercased; making LIKE case-sensitive
        // lets the prefix query use the name index instead of scanning.
        conn.pragma_update(None, "case_sensitive_like", "ON")?;
        conn.execute_batch(SCHEMA)?;
        conn.execute(
            "INSERT INTO meta(key, value) VALUES('schema_version', '1')
             ON CONFLICT(key) DO NOTHING",
            [],
        )?;
        Ok(Self { conn })
    }

    // ---- roots ----

    /// Registers (or fetches) a root and returns its id.
    pub fn ensure_root(&mut self, path: &str) -> Result<i64, StoreError> {
        self.conn.execute(
            "INSERT INTO roots(path) VALUES(?1) ON CONFLICT(path) DO NOTHING",
            params![path],
        )?;
        let id: i64 = self.conn.query_row(
            "SELECT id FROM roots WHERE path = ?1",
            params![path],
            |row| row.get(0),
        )?;
        Ok(id)
    }

    pub fn roots(&self) -> Result<Vec<Root>, StoreError> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, path, enabled, current_generation FROM roots ORDER BY id")?;
        let rows = stmt.query_map([], |row| {
            Ok(Root {
                id: row.get(0)?,
                path: row.get(1)?,
                enabled: row.get::<_, i64>(2)? != 0,
                current_generation: row.get(3)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Starts a new scan generation for `root_id` and returns it.
    pub fn begin_generation(&mut self, root_id: i64) -> Result<i64, StoreError> {
        let gen: i64 = self.conn.query_row(
            "UPDATE roots SET current_generation = current_generation + 1
             WHERE id = ?1 RETURNING current_generation",
            params![root_id],
            |row| row.get(0),
        )?;
        Ok(gen)
    }

    /// Deletes entries of `root_id` that were not touched in generation `gen`.
    /// Only call while the root's volume is present (disconnected volumes keep
    /// their entries, flagged `missing`).
    pub fn sweep_generation(&mut self, root_id: i64, gen: i64) -> Result<u64, StoreError> {
        let tx = self.conn.transaction()?;
        let stale = "SELECT id FROM entries WHERE root_id = ?1 AND scan_generation < ?2";
        tx.execute(
            &format!("DELETE FROM entries_fts WHERE rowid IN ({stale})"),
            params![root_id, gen],
        )?;
        tx.execute(
            &format!("DELETE FROM entries_tri WHERE rowid IN ({stale})"),
            params![root_id, gen],
        )?;
        let deleted_rows = tx.execute(
            "DELETE FROM entries WHERE root_id = ?1 AND scan_generation < ?2",
            params![root_id, gen],
        )?;
        tx.commit()?;
        Ok(deleted_rows as u64)
    }

    /// Marks all of a root's entries as missing (volume gone) or present again.
    pub fn set_missing_for_root(&mut self, root_id: i64, missing: bool) -> Result<u64, StoreError> {
        let n = self.conn.execute(
            "UPDATE entries SET missing = ?2 WHERE root_id = ?1 AND missing != ?2",
            params![root_id, missing as i64],
        )?;
        Ok(n as u64)
    }

    // ---- entries ----

    /// Upserts a batch of entries in one transaction. `scan_generation` is
    /// stamped onto every row so [`Self::sweep_generation`] can remove stale
    /// ones after a completed scan.
    pub fn apply_batch(
        &mut self,
        root_id: i64,
        scan_generation: i64,
        records: &[EntryRecord],
    ) -> Result<Vec<UpsertOutcome>, StoreError> {
        let tx = self.conn.transaction()?;
        let mut outcomes = Vec::with_capacity(records.len());
        {
            let mut select = tx.prepare_cached("SELECT id, size, mtime, name_lower FROM entries WHERE path = ?1")?;
            let mut insert = tx.prepare_cached(
                "INSERT INTO entries(root_id, parent_id, path, name, name_lower, ext, is_dir,
                                     size, mtime, volume_serial, file_id, is_app, app_target,
                                     missing, scan_generation)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,0,?14)",
            )?;
            let mut update = tx.prepare_cached(
                "UPDATE entries SET parent_id=?2, name=?3, name_lower=?4, ext=?5, is_dir=?6,
                                    size=?7, mtime=?8, volume_serial=?9, file_id=?10, is_app=?11,
                                    app_target=?12, missing=0, scan_generation=?13
                 WHERE id=?1",
            )?;
            let mut fts_insert = tx.prepare_cached(
                "INSERT INTO entries_fts(rowid, name, keywords) VALUES(?1, ?2, ?3)",
            )?;
            let mut fts_delete =
                tx.prepare_cached("DELETE FROM entries_fts WHERE rowid = ?1")?;
            let mut tri_insert =
                tx.prepare_cached("INSERT INTO entries_tri(rowid, name) VALUES(?1, ?2)")?;
            let mut tri_delete =
                tx.prepare_cached("DELETE FROM entries_tri WHERE rowid = ?1")?;

            for rec in records {
                let name_lower = rec.name.to_lowercase();
                let keywords = fts_keywords(rec);
                let existing: Option<(i64, i64, i64, String)> = select
                    .query_row(params![rec.path], |row| {
                        Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
                    })
                    .optional()?;

                match existing {
                    Some((id, size, mtime, old_name_lower))
                        if size == rec.size as i64
                            && mtime == rec.mtime
                            && old_name_lower == name_lower =>
                    {
                        // Unchanged: keep it alive for the sweep and clear a
                        // stale missing flag (the row was seen on disk).
                        tx.execute(
                            "UPDATE entries SET scan_generation = ?2, missing = 0 WHERE id = ?1",
                            params![id, scan_generation],
                        )?;
                        outcomes.push(UpsertOutcome::Unchanged);
                    }
                    Some((id, ..)) => {
                        update.execute(params![
                            id,
                            rec.parent_id,
                            rec.name,
                            name_lower,
                            rec.ext,
                            rec.is_dir as i64,
                            rec.size as i64,
                            rec.mtime,
                            rec.volume_serial as i64,
                            rec.file_id as i64,
                            rec.is_app as i64,
                            rec.app_target,
                            scan_generation,
                        ])?;
                        fts_delete.execute(params![id])?;
                        fts_insert.execute(params![id, rec.name, keywords])?;
                        tri_delete.execute(params![id])?;
                        tri_insert.execute(params![id, name_lower])?;
                        outcomes.push(UpsertOutcome::Updated);
                    }
                    None => {
                        insert.execute(params![
                            root_id,
                            rec.parent_id,
                            rec.path,
                            rec.name,
                            name_lower,
                            rec.ext,
                            rec.is_dir as i64,
                            rec.size as i64,
                            rec.mtime,
                            rec.volume_serial as i64,
                            rec.file_id as i64,
                            rec.is_app as i64,
                            rec.app_target,
                            scan_generation,
                        ])?;
                        let id = tx.last_insert_rowid();
                        fts_insert.execute(params![id, rec.name, keywords])?;
                        tri_insert.execute(params![id, name_lower])?;
                        outcomes.push(UpsertOutcome::Inserted);
                    }
                }
            }
        }
        tx.commit()?;
        Ok(outcomes)
    }

    /// Applies a rename/move while preserving the entry's identity (and with
    /// it history, result ids and the FTS rowid).
    pub fn move_entry(
        &mut self,
        id: i64,
        parent_id: Option<i64>,
        new_path: &str,
        new_name: &str,
        new_ext: &str,
    ) -> Result<(), StoreError> {
        let tx = self.conn.transaction()?;
        {
            let name_lower = new_name.to_lowercase();
            tx.execute(
                "UPDATE entries SET parent_id=?2, path=?3, name=?4, name_lower=?5, ext=?6
                 WHERE id=?1",
                params![id, parent_id, new_path, new_name, name_lower, new_ext],
            )?;
            let keywords: String = tx.query_row(
                "SELECT app_target FROM entries WHERE id = ?1",
                params![id],
                |row| row.get(0),
            )?;
            let keywords = fts_keywords(&EntryRecord {
                root_id: 0,
                parent_id,
                path: new_path.to_string(),
                name: new_name.to_string(),
                ext: new_ext.to_string(),
                keywords,
                ..Default::default()
            });
            tx.execute("DELETE FROM entries_fts WHERE rowid = ?1", params![id])?;
            tx.execute(
                "INSERT INTO entries_fts(rowid, name, keywords) VALUES(?1, ?2, ?3)",
                params![id, new_name, keywords],
            )?;
            tx.execute("DELETE FROM entries_tri WHERE rowid = ?1", params![id])?;
            tx.execute(
                "INSERT INTO entries_tri(rowid, name) VALUES(?1, ?2)",
                params![id, name_lower],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn find_by_path(&self, path: &str) -> Result<Option<EntryRow>, StoreError> {
        self.conn
            .query_row(
                "SELECT id, root_id, parent_id, path, name, name_lower, ext, is_dir, is_app,
                        mtime, missing
                 FROM entries WHERE path = ?1",
                params![path],
                map_entry_row,
            )
            .optional()
            .map_err(Into::into)
    }

    /// Finds an entry by filesystem identity (volume serial + file id) —
    /// the backbone of rename/move detection.
    pub fn find_by_file_id(
        &self,
        volume_serial: u32,
        file_id: u64,
    ) -> Result<Option<EntryRow>, StoreError> {
        self.conn
            .query_row(
                "SELECT id, root_id, parent_id, path, name, name_lower, ext, is_dir, is_app,
                        mtime, missing
                 FROM entries WHERE volume_serial = ?1 AND file_id = ?2 LIMIT 1",
                params![volume_serial as i64, file_id as i64],
                map_entry_row,
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn remove_by_path(&mut self, path: &str) -> Result<bool, StoreError> {
        let tx = self.conn.transaction()?;
        let id: Option<i64> = tx
            .query_row("SELECT id FROM entries WHERE path = ?1", params![path], |r| {
                r.get(0)
            })
            .optional()?;
        let removed = match id {
            Some(id) => {
                tx.execute("DELETE FROM entries_fts WHERE rowid = ?1", params![id])?;
                tx.execute("DELETE FROM entries_tri WHERE rowid = ?1", params![id])?;
                tx.execute("DELETE FROM entries WHERE id = ?1", params![id])? > 0
            }
            None => false,
        };
        tx.commit()?;
        Ok(removed)
    }

    /// Removes an entry and its whole subtree (a directory disappeared).
    pub fn remove_tree(&mut self, path: &str) -> Result<u64, StoreError> {
        let tx = self.conn.transaction()?;
        let ids: Vec<i64> = {
            let mut stmt = tx.prepare(
                "WITH RECURSIVE sub(id) AS (
                     SELECT id FROM entries WHERE path = ?1
                     UNION ALL
                     SELECT e.id FROM entries e JOIN sub ON e.parent_id = sub.id
                 )
                 SELECT id FROM sub",
            )?;
            let rows = stmt.query_map(params![path], |r| r.get::<_, i64>(0))?;
            rows.collect::<Result<_, _>>()?
        };
        let mut deleted = 0u64;
        for id in ids {
            tx.execute("DELETE FROM entries_fts WHERE rowid = ?1", params![id])?;
            tx.execute("DELETE FROM entries_tri WHERE rowid = ?1", params![id])?;
            deleted += tx.execute("DELETE FROM entries WHERE id = ?1", params![id])? as u64;
        }
        tx.commit()?;
        Ok(deleted)
    }

    // ---- crawl queue (crash-safe work list) ----

    pub fn enqueue_dir(&mut self, root_id: i64, dir_path: &str) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO crawl_queue(root_id, dir_path) VALUES(?1, ?2)
             ON CONFLICT(root_id, dir_path) DO NOTHING",
            params![root_id, dir_path],
        )?;
        Ok(())
    }

    /// Atomically claims the oldest unclaimed directory for a worker. The row
    /// stays in the queue until [`Self::complete_dir`] runs, so a crash at any
    /// point re-runs the (idempotent) scan of that directory on restart.
    pub fn claim_dir(&mut self) -> Result<Option<(i64, i64, String)>, StoreError> {
        self.conn
            .query_row(
                "UPDATE crawl_queue SET claimed = 1
                 WHERE id = (SELECT id FROM crawl_queue WHERE claimed = 0 ORDER BY id LIMIT 1)
                 RETURNING id, root_id, dir_path",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
            .map_err(Into::into)
    }

    /// Requeues directories claimed by a previous process that never finished.
    pub fn release_claims(&mut self) -> Result<(), StoreError> {
        self.conn
            .execute("UPDATE crawl_queue SET claimed = 0 WHERE claimed = 1", [])?;
        Ok(())
    }

    /// Marks one queued directory as processed.
    pub fn complete_dir(&mut self, queue_id: i64) -> Result<(), StoreError> {
        self.conn
            .execute("DELETE FROM crawl_queue WHERE id = ?1", params![queue_id])?;
        Ok(())
    }

    pub fn queue_depth(&self) -> Result<u64, StoreError> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM crawl_queue", [], |r| r.get(0))?;
        Ok(n as u64)
    }

    pub fn clear_queue(&mut self) -> Result<(), StoreError> {
        self.conn.execute("DELETE FROM crawl_queue", [])?;
        Ok(())
    }

    /// Rewrites descendant paths after a directory rename. The renamed row
    /// itself is moved via [`Self::move_entry`]; only subtree paths change
    /// here (names are untouched).
    pub fn rename_tree_paths(&mut self, old_prefix: &str, new_prefix: &str) -> Result<u64, StoreError> {
        let old_len = old_prefix.chars().count() as i64;
        let n = self.conn.execute(
            "UPDATE entries SET path = ?1 || substr(path, ?3 + 1)
             WHERE length(path) > ?3
               AND substr(path, 1, ?3) = ?2
               AND substr(path, ?3 + 1, 1) IN ('\\', '/')",
            params![new_prefix, old_prefix, old_len],
        )?;
        Ok(n as u64)
    }

    /// Number of queued (unfinished) directories for a root.
    pub fn pending_for_root(&self, root_id: i64) -> Result<u64, StoreError> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM crawl_queue WHERE root_id = ?1",
            params![root_id],
            |r| r.get(0),
        )?;
        Ok(n as u64)
    }

    /// True when any of the root's entries are flagged missing (disconnected
    /// volume state).
    pub fn has_missing_entries(&self, root_id: i64) -> Result<bool, StoreError> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM entries WHERE root_id = ?1 AND missing = 1 LIMIT 1",
            params![root_id],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    // ---- checkpoints ----

    pub fn set_checkpoint(&mut self, key: &str, value: &str) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO checkpoints(key, value) VALUES(?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn checkpoint(&self, key: &str) -> Result<Option<String>, StoreError> {
        self.conn
            .query_row("SELECT value FROM checkpoints WHERE key = ?1", params![key], |r| {
                r.get(0)
            })
            .optional()
            .map_err(Into::into)
    }

    // ---- stats ----

    pub fn stats(&self) -> Result<IndexStats, StoreError> {
        let entries_total: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM entries", [], |r| r.get(0))?;
        let files_total: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM entries WHERE is_dir = 0",
            [],
            |r| r.get(0),
        )?;
        let dirs_total: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM entries WHERE is_dir = 1",
            [],
            |r| r.get(0),
        )?;
        let apps_total: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM entries WHERE is_app = 1",
            [],
            |r| r.get(0),
        )?;
        let page_count: i64 = self
            .conn
            .query_row("PRAGMA page_count", [], |r| r.get(0))?;
        let page_size: i64 = self
            .conn
            .query_row("PRAGMA page_size", [], |r| r.get(0))?;
        Ok(IndexStats {
            entries_total: entries_total as u64,
            files_total: files_total as u64,
            dirs_total: dirs_total as u64,
            apps_total: apps_total as u64,
            index_size_bytes: (page_count * page_size) as u64,
        })
    }

    /// Raw connection access for the search module (same crate only).
    pub(crate) fn conn(&self) -> &Connection {
        &self.conn
    }
}

fn map_entry_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<EntryRow> {
    Ok(EntryRow {
        id: row.get(0)?,
        root_id: row.get(1)?,
        parent_id: row.get(2)?,
        path: row.get(3)?,
        name: row.get(4)?,
        name_lower: row.get(5)?,
        ext: row.get(6)?,
        is_dir: row.get::<_, i64>(7)? != 0,
        is_app: row.get::<_, i64>(8)? != 0,
        mtime: row.get(9)?,
        missing: row.get::<_, i64>(10)? != 0,
    })
}

/// The searchable text for an entry: its name is the primary column; the
/// keywords column carries path words and app keywords so token matches can
/// hit them at lower rank.
fn fts_keywords(rec: &EntryRecord) -> String {
    let mut kw = String::with_capacity(rec.path.len() + rec.keywords.len());
    for word in rec
        .path
        .split(|c: char| c == '\\' || c == '/' || c == '.' || c == '-' || c == '_')
    {
        let word = word.trim();
        if !word.is_empty() && !word.eq_ignore_ascii_case(&rec.name) {
            kw.push_str(word);
            kw.push(' ');
        }
    }
    kw.push_str(&rec.keywords);
    kw.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(path: &str, name: &str, size: u64, mtime: i64) -> EntryRecord {
        EntryRecord {
            root_id: 0,
            parent_id: None,
            path: path.into(),
            name: name.into(),
            ext: crate::model::extension_of(name),
            is_dir: false,
            size,
            mtime,
            volume_serial: 1,
            file_id: size, // unique per record in tests
            is_app: false,
            app_target: String::new(),
            keywords: String::new(),
        }
    }

    fn setup() -> (IndexStore, i64) {
        let mut store = IndexStore::open_in_memory().unwrap();
        let root = store.ensure_root("C:\\data").unwrap();
        (store, root)
    }

    #[test]
    fn upsert_insert_update_unchanged() {
        let (mut store, root) = setup();
        let gen = store.begin_generation(root).unwrap();

        let outcomes =
            store.apply_batch(root, gen, &[rec("C:\\data\\a.txt", "a.txt", 10, 100)]).unwrap();
        assert_eq!(outcomes, vec![UpsertOutcome::Inserted]);

        // Same size/mtime → unchanged.
        let outcomes =
            store.apply_batch(root, gen, &[rec("C:\\data\\a.txt", "a.txt", 10, 100)]).unwrap();
        assert_eq!(outcomes, vec![UpsertOutcome::Unchanged]);

        // New mtime → updated.
        let outcomes =
            store.apply_batch(root, gen, &[rec("C:\\data\\a.txt", "a.txt", 10, 200)]).unwrap();
        assert_eq!(outcomes, vec![UpsertOutcome::Updated]);

        assert_eq!(store.stats().unwrap().entries_total, 1);
    }

    #[test]
    fn generation_sweep_removes_stale_entries() {
        let (mut store, root) = setup();
        let g1 = store.begin_generation(root).unwrap();
        store
            .apply_batch(root, g1, &[rec("C:\\data\\a.txt", "a.txt", 1, 1), rec("C:\\data\\b.txt", "b.txt", 2, 2)])
            .unwrap();

        let g2 = store.begin_generation(root).unwrap();
        store.apply_batch(root, g2, &[rec("C:\\data\\a.txt", "a.txt", 1, 1)]).unwrap();
        let swept = store.sweep_generation(root, g2).unwrap();
        assert_eq!(swept, 1);
        assert!(store.find_by_path("C:\\data\\a.txt").unwrap().is_some());
        assert!(store.find_by_path("C:\\data\\b.txt").unwrap().is_none());
    }

    #[test]
    fn missing_entries_are_preserved() {
        let (mut store, root) = setup();
        let gen = store.begin_generation(root).unwrap();
        store
            .apply_batch(root, gen, &[rec("D:\\usb\\x.txt", "x.txt", 1, 1)])
            .unwrap();
        store.set_missing_for_root(root, true).unwrap();
        assert_eq!(store.stats().unwrap().entries_total, 1); // preserved
        store.set_missing_for_root(root, false).unwrap();
        assert!(!store.find_by_path("D:\\usb\\x.txt").unwrap().unwrap().missing);
    }

    #[test]
    fn file_id_lookup_finds_moved_entry() {
        let (mut store, root) = setup();
        let gen = store.begin_generation(root).unwrap();
        let mut r = rec("C:\\data\\old.txt", "old.txt", 7, 1);
        r.file_id = 4242;
        store.apply_batch(root, gen, &[r]).unwrap();

        let found = store.find_by_file_id(1, 4242).unwrap().unwrap();
        assert_eq!(found.name, "old.txt");

        store
            .move_entry(found.id, None, "C:\\data\\new.txt", "new.txt", "txt")
            .unwrap();
        assert!(store.find_by_path("C:\\data\\old.txt").unwrap().is_none());
        let moved = store.find_by_path("C:\\data\\new.txt").unwrap().unwrap();
        assert_eq!(moved.id, found.id); // identity preserved
    }

    #[test]
    fn remove_tree_removes_subtree() {
        let (mut store, root) = setup();
        let gen = store.begin_generation(root).unwrap();
        let mut dir = rec("C:\\data\\sub", "sub", 0, 0);
        dir.is_dir = true;
        store.apply_batch(root, gen, &[dir]).unwrap();
        let parent = store.find_by_path("C:\\data\\sub").unwrap().unwrap().id;
        let mut child = rec("C:\\data\\sub\\c.txt", "c.txt", 1, 1);
        child.parent_id = Some(parent);
        store.apply_batch(root, gen, &[child]).unwrap();

        assert_eq!(store.remove_tree("C:\\data\\sub").unwrap(), 2);
        assert_eq!(store.stats().unwrap().entries_total, 0);
    }

    #[test]
    fn crawl_queue_claim_and_resume() {
        let (mut store, root) = setup();
        store.enqueue_dir(root, "C:\\data\\a").unwrap();
        store.enqueue_dir(root, "C:\\data\\a").unwrap(); // idempotent
        store.enqueue_dir(root, "C:\\data\\b").unwrap();
        assert_eq!(store.queue_depth().unwrap(), 2);

        let first = store.claim_dir().unwrap().unwrap();
        assert_eq!(first.2, "C:\\data\\a");
        // A claimed row cannot be claimed twice (single worker owns it).
        assert_eq!(store.claim_dir().unwrap().unwrap().2, "C:\\data\\b");
        assert!(store.claim_dir().unwrap().is_none());

        // Simulated crash: release_claims makes everything claimable again.
        store.release_claims().unwrap();
        assert!(store.claim_dir().unwrap().is_some());

        store.complete_dir(first.0).unwrap();
        assert_eq!(store.queue_depth().unwrap(), 1);
    }

    #[test]
    fn checkpoints_round_trip() {
        let (mut store, _root) = setup();
        assert_eq!(store.checkpoint("phase").unwrap(), None);
        store.set_checkpoint("phase", "crawling").unwrap();
        store.set_checkpoint("phase", "done").unwrap();
        assert_eq!(store.checkpoint("phase").unwrap().as_deref(), Some("done"));
    }

    #[test]
    fn stats_counts_apps_and_dirs() {
        let (mut store, root) = setup();
        let gen = store.begin_generation(root).unwrap();
        let mut d = rec("C:\\data\\d", "d", 0, 0);
        d.is_dir = true;
        let mut app = rec("C:\\data\\Code.lnk", "Code.lnk", 1, 1);
        app.is_app = true;
        app.app_target = "C:\\Program Files\\Code\\Code.exe".into();
        store.apply_batch(root, gen, &[d, app, rec("C:\\data\\f.txt", "f.txt", 1, 1)]).unwrap();

        let stats = store.stats().unwrap();
        assert_eq!(stats.entries_total, 3);
        assert_eq!(stats.dirs_total, 1);
        assert_eq!(stats.apps_total, 1);
        assert!(stats.index_size_bytes > 0);
    }
}
