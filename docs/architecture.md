# WinSearch Architecture

WinSearch is a native Windows launcher + universal search app, ported from
DigUp (macOS/Swift, MIT) and rebuilt on Windows-native stacks: a Rust
indexing/search engine (`winsearchd`) and a WinUI 3 (.NET 8) frontend, talking
over a per-user named pipe.

```
+---------------------------------------------------------------+
|  WinSearch.App (WinUI 3)                                      |
|  OverlayWindow · SettingsWindow · TrayIcon · HotkeyManager     |
+------------------------------+--------------------------------+
                               | WinSearch.ViewModels (MVVM)
                               | WinSearch.Core (IPC + protocol + settings)
+------------------------------v--------------------------------+
|  \\.\pipe\winsearch-<SID>   (per-user DACL, no network)         |
+------------------------------+--------------------------------+
                               | ws-protocol (framed JSON, v1)
+------------------------------v--------------------------------+
|  winsearchd (Rust)                                            |
|  ┌────────────┐ ┌──────────┐ ┌──────────────┐ ┌────────────┐  |
|  │ crawl.rs   │ │watcher.rs│ │ catalog.rs   │ │ engine.rs  │  |
|  │ dir scan + │ │ReadDir.. │ │ Start Menu + │ │ search     │  |
|  │ exclusions │ │ ChangesW │ │ .lnk resolve │ │ pipeline   │  |
|  └─────┬──────┘ └────┬─────┘ └──────┬───────┘ └─────┬──────┘  |
|        └────────── indexer.rs ──────┘               │         |
|                  (queue, workers, drives)           │         |
+--------------------------+--------------------------+---------+
                           | ws-index (rusqlite, FTS5)
                           v
              %LOCALAPPDATA%\WinSearch\db\index.db
```

## Components

### `ws-protocol` (crates/ws-protocol)

Typed IPC messages (`ClientMessage`/`ServerMessage`) and the framing codec
(4-byte LE length + UTF-8 JSON, 16 MiB cap). Protocol version is **1**; see
`ipc-protocol.md` for the wire contract.

### `ws-platform` (crates/ws-platform)

Win32 plumbing: named pipe with a per-user DACL and
`PIPE_REJECT_REMOTE_CLIENTS`, current-user SID, data paths under
`%LOCALAPPDATA%\WinSearch`, and a named-mutex single-instance guard.

### `ws-index` (crates/ws-index)

The search index — no Windows dependencies, fully unit-tested anywhere.

- **Storage**: SQLite (WAL, `synchronous=NORMAL`) with an FTS5 table over
  names + path keywords. Schema in `store.rs`; upserts are batched per
  directory in one transaction (crash-safe: any interruption leaves the DB
  consistent and the crawl resumable).
- **Identity**: every entry stores `(volume_serial, file_id)`, so renames and
  moves can be matched to their previous row (`find_by_file_id`).
- **Crawl queue**: durable work list with atomic claims; claims from a
  crashed run are released on startup and re-processed (idempotent upserts).
- **Retrieval** (`search.rs`): FTS5 prefix candidates + a bounded substring
  fallback (for mid-word matches like `code` → `VSCode`), merged and ranked
  deterministically (exact name > prefix > substring > path; small boosts for
  apps and recency; small penalty for deep paths). Every hit carries its match
  explanation.
- **Calculator** (`calc.rs`): recursive-descent arithmetic evaluator
  (+ - * / % ^, functions, constants). Only arithmetic syntax is accepted —
  nothing is ever executed. Queries like `report-2.pdf` are recognized as
  ordinary text, not math.
- **Settings catalog** (`settings.rs`): `ms-settings:` URI pages with
  name/keyword matching.

### `winsearchd` (bins/winsearchd)

- `crawl.rs` — per-directory scan with the exclusion policy: hidden/system  items, dev trees (`.git`, `node_modules`, `target`, …), and sensitive files
  (keys, certs, credential stores). Metadata comes from directory enumeration;
  identity lookups open files with `FILE_READ_ATTRIBUTES` only, so cloud
  placeholders (OneDrive) are never hydrated. Reparse points are indexed as
  files but never recursed into (no junction cycles).
- `watcher.rs` — `ReadDirectoryChangesW` per root (64 KiB buffer, subtree).
  Rename OLD/NEW pairs are fused into one `Renamed` event so the index
  preserves entry identity across renames and moves; buffer overflow triggers
  a subtree rescan.
- `indexer.rs` — orchestrator: bounded crawl worker pool (4 threads) draining
  the crash-safe queue, watcher event handling, the application catalog
  refresh, and a drive supervisor that preserves the index of disconnected
  volumes (`missing` flag) and reconciles them on reconnect. Plus the
  semantic pipeline (P3.5): `UpsertOutcome != Unchanged` (from `process_dir`
  batches, `point_upsert`, renames, and catalog refreshes) enqueues
  `extract_jobs(kind='name')` — `Inserted` at high priority (new files first),
  `Updated`/requeues low; claim order is `priority DESC, id DESC` (LIFO
  within priority). One scheduler thread (`winsearch-embed-sched`) drains the
  queue in batches of 128 names through the supervisor's bounded call
  (**120 s** job-IO watchdog: a hung worker fails the batch as `Transport`,
  claims are released for retry, the scheduler never stalls). Results upsert
  `segments(kind='name', chunk 0)` idempotently (stable id, replace prior
  vector). Deletes cascade via FK (`sweep`/`remove_tree`); claims left by a
  crash are released on open (`release_extract_claims`). A model swap
  (`check_compat == Incompatible`) purges `segments(kind='name')` and
  requeues one name job per entry at startup.
- `catalog.rs` — Start Menu/Desktop scan; `.lnk` targets resolved via the
  shell's `IShellLinkW`.
- `engine.rs` — the search pipeline: calculator → settings → index hits,
  streamed as progressive batches with cooperative cancellation. `code:`
  queries map to the code mode (index lands in Phase 5) and return nothing
  rather than mixing in normal results. `status()` reports `model_loaded`
  from the embed supervisor's cache (real weights resident, or stub vectors
  flowing in test setups); `segments_indexed` counts `segments(kind='name')`;
  `current_activity` is `"Indexing"` while the crawl queue drains and
  `"Embedding names…"` while only the embed queue remains. The engine reaches
  the supervisor through `embed_supervisor()` (an `Arc` shared with the
  indexer's scheduler); the pipeline embeds through its bounded call.
- `embed_client.rs` — supervisor for the inference worker: spawns
  `winsearch-embed` on the first embed request (never before), handshakes
  every connection (health ping + version check + flag refresh), restarts
  with backoff after a transport failure and retries the in-flight job once.
  Definitive worker errors are never retried. Links no native inference code.
  The pipeline never calls `embed_texts` directly: `embed_texts_bounded`
  (helper thread + `recv_timeout`, **`EMBED_JOB_TIMEOUT` = 120 s**) bounds
  every scheduler batch; on timeout the owned child is killed to break the
  hung pipe IO and the batch fails for retry.

### `winsearch-embed` (bins/winsearch-embed) + `ws-embed-protocol`

- `winsearch-embed` — crash-isolated inference process serving the internal
  embed-worker protocol on `\\.\pipe\winsearch-embed-<SID>` (one client,
  `winsearchd`). Loads the model lazily on the first job (stays alive on
  `model_not_available` so a later download can succeed), warms up via
  `EmbedRuntime`, and exits after 5 minutes idle — process exit is the full
  GPU release. Single-instance mutex `Local\WinSearch.Embed.v1`.
  `--stub` serves deterministic 768-d unit vectors with no model file.
- `ws-embed-protocol` — the single definition of that protocol (own version
  `EMBED_PROTOCOL_VERSION`, never mirrored to the frontend): `hello`,
  `embed_texts`/`embed_audio`/`embed_image` jobs, `status`/`shutdown`, and
  the deterministic stub-vector math. See `ipc-protocol.md`.

### Frontend (src/app)

- `WinSearch.Core` — `EngineClient` (async pipe client: correlation ids,
  streamed batches, cancellation, auto-reconnect with engine restart),
  protocol models, `AppSettings`.
- `WinSearch.ViewModels` — `MainOverlayViewModel` (debounce, superseded-query
  cancellation, stable updates), settings/result item VMs.
- `WinSearch.App` — overlay window (one instance, reused; DPI/monitor-aware
  placement, acrylic, composition animations respecting reduced motion),
  tray icon, global hotkey with conflict detection and fallback, settings.

## Data locations

| Path | Contents |
|---|---|
| `%LOCALAPPDATA%\WinSearch\db\index.db` | entries + FTS + crawl queue + checkpoints + vector segments + embed queue |
| `%LOCALAPPDATA%\WinSearch\db\code.db` | code index (Phase 5) |
| `%LOCALAPPDATA%\WinSearch\models` | embedding models (Phase 3) |
| `%LOCALAPPDATA%\WinSearch\cache\thumbs` | thumbnail cache |
| `%LOCALAPPDATA%\WinSearch\logs` | engine logs (rolling daily) |
| `%LOCALAPPDATA%\WinSearch\settings.json` | UI-owned settings |

## Indexing lifecycle

1. Startup: open DB, release claims left by a crash (crawl + embed queues),
   enqueue every present root, start workers/watchers/catalog/drive
   supervisor and the embed scheduler. A model swap (`check_compat ==
   Incompatible`) first purges `segments(kind='name')` and requeues one name
   job per entry.
2. Crawl workers claim directories, scan them, upsert children and enqueue
   subdirectories in per-directory batches, then mark the directory done.
   Every changed entry also enqueues a name-embed job (new files first, LIFO
   within priority).
3. The embed scheduler claims up to 128 name jobs, embeds the current entry
   names through the bounded supervisor call, and upserts one segment per
   entry (replace prior vector, stable id); failures release claims for retry.
4. When a root's queue drains, a generation sweep deletes entries that were
   not seen in this scan (files removed while indexing was away) — segments
   and pending jobs cascade via FK.
5. Watchers keep the index fresh between scans; renames keep row identity
   (and requeue the new name); deletes cascade to segments.
6. Disconnected volumes: entries are flagged `missing` (hidden from search,
   never deleted). On reconnect the root is rescanned and unflagged.

## Performance posture

- Bounded everything: 4 crawl workers, per-directory transactions, capped
  candidate sets (500 FTS + 500 substring) per query, capped results.
- The UI thread never touches the index; searches run on the connection thread
  and stream batches.
- Measurements live in `benchmarks.md` (kept up to date with the code).

## Privacy

Local-only by design: per-user named pipe, no network listener, no telemetry
anywhere in the codebase. The engine never reads file contents in Phase 2 and
never hydrates cloud placeholders. Sensitive files are excluded at the index
level.
