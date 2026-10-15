# WinSearch IPC Protocol

Transport: named pipe, one instance per user session.
Pipe name: `\\.\pipe\winsearch-<user SID>` (Rust side uses the full path;
the .NET client passes the bare name `winsearch-<SID>` to
`NamedPipeClientStream`).

Security: the pipe is created with an explicit DACL granting access to the
current user's SID only, plus `PIPE_REJECT_REMOTE_CLIENTS`. No network listener
exists. Protocol version: **1** (`PROTOCOL_VERSION` in `ws-protocol`,
`ProtocolJson.Version` in `WinSearch.Core`).

## Framing

Each message is a frame:

```
+-------------------+---------------------------+
| u32 LE length     | UTF-8 JSON payload        |
+-------------------+---------------------------+
```

- Maximum frame size: 16 MiB (`MAX_FRAME_BYTES`).
- A clean end-of-stream (0 bytes at a frame boundary) means the peer closed.
- A truncated frame is an error and tears down the connection.

The payload encoding is JSON for debuggability; a compact binary encoding is
only adopted if profiling shows a measurable benefit.

## Message envelope

All messages carry a `type` discriminator (snake_case). Requests carry `id`
(`u64`, monotonically increasing per connection). Responses echo the `id`.

### Client → engine (`ClientMessage`)

| type | fields | description |
|---|---|---|
| `hello` | `protocol_version`, `client`, `client_version` | Must be the first message. Version mismatch → `error` + disconnect. |
| `ping` | `id` | Heartbeat; answered by `pong`. |
| `search` | `id`, `query` | Execute a search; results stream as `results` frames. |
| `cancel` | `id` | Cancels the in-flight request with this id. |
| `get_status` | `id` | Snapshot request; answered by `status` with `id` set. |
| `model_download_start` | `id` | Begin (or resume) the embedding-model download; answered by `model_status` with `id` set. Idempotent while running. |
| `model_download_cancel` | `id` | Cancel the in-flight model download; partial files stay for resume (`paused`). |
| `model_import` | `id`, `path` | Import model files from a local file or directory (copied + verified). |
| `model_delete` | `id` | Delete the downloaded model files. |
| `get_model_status` | `id` | Snapshot request; answered by `model_status` with `id` set. |

### Engine → client (`ServerMessage`)

| type | fields | description |
|---|---|---|
| `hello_ack` | `protocol_version`, `engine_version`, `features[]` | Handshake reply; `features` lists capability tags (e.g. `search.exact`, `search.semantic`, `code`). |
| `pong` | `id` | Heartbeat reply. |
| `results` | `id`, `batch[]`, `done` | Streamed result batches; terminal frame has `done: true` (also sent when cancelled). |
| `status` | `id?`, `status` | `id: null` is a pushed update (indexing progress); `id` set answers `get_status`. |
| `model_status` | `id?`, `status` | `id: null` is a pushed update (download progress / completion); `id` set answers a model request. |
| `error` | `id?`, `code`, `message` | `id: null` is connection-scoped (e.g. handshake failure). |

### Payload models

- `SearchQuery`: `text`, `mode` (`auto|apps|files|code`), `limit`,
  `include_semantic`, `categories[]`.
- `SearchResult`: `id`, `kind`, `title`, `subtitle`, `match_explanation`,
  `score`, `path?`, `location?`, `thumbnail_path?`.
- `MatchLocation` (discriminator `kind`):
  `page {page}` | `timestamp {seconds, end_seconds}` | `line {line}`.
- `EngineStatus`: `state` (`idle|indexing|paused|error`), `current_activity`,
  `files_discovered`, `files_indexed`, `segments_indexed`, `queue_depth`,
  `model_loaded`, `index_size_bytes`.
- `ModelStatus`: `state` (`not_downloaded|downloading|paused|ready|error`),
  `bytes`, `total_bytes`, `error?`.
- `ErrorCode`: `protocol_version_mismatch`, `invalid_request`, `internal`,
  `not_ready`, `cancelled`, `unsupported`.

## Semantics

- **Progressive search**: exact matches stream first; semantic results append
  in later batches under the same `id` when `include_semantic` is set. The
  client discards batches from superseded requests (sequence guard) so older
  searches never overwrite newer ones.
- **Cancellation**: cancelling the client-side token sends `cancel`; the engine
  cooperatively stops and closes the stream with `done: true`.
- **Liveness**: clients ping periodically; a dropped pipe triggers automatic
  reconnect (with engine restart if needed) and a fresh handshake.
- **Engine independence**: `winsearchd` runs with or without any client
  attached; closing the UI never interrupts indexing.
- **Model lifecycle**: `model_download_start` resumes `<name>.part` files via
  HTTP `Range` and is idempotent while running; progress and completion
  arrive as pushed `model_status` frames (throttled to ~4 Hz server-side).
  A concurrent `model_import` is refused with `error: invalid_request`
  (like `model_delete`, which also refuses while a download/import is in
  flight). Every message above is additive: `PROTOCOL_VERSION` stays **1**.

## Versioning

Breaking wire changes bump `PROTOCOL_VERSION`. Both sides refuse mismatched
versions at handshake with `error: protocol_version_mismatch` and disconnect.
Additive fields use `#[serde(default)]` semantics and do not bump the version.

## Internal embed-worker protocol (P3.2)

`winsearchd` talks to its inference worker `winsearch-embed` over a **second**
per-user pipe, `\\.\pipe\winsearch-embed-<SID>`, built on the same
`ws-platform` DACL machinery (`PIPE_REJECT_REMOTE_CLIENTS`, current user
only). Framing is the same length-prefixed JSON codec (16 MiB cap), but the
message set and version are independent: `EMBED_PROTOCOL_VERSION` (currently
**1**) lives in the `ws-embed-protocol` crate, which is the single definition
both sides code against. This protocol is **internal**: it is never mirrored
to `WinSearch.Core` and `PROTOCOL_VERSION` above is untouched by it.

`winsearchd` is the only client (one connection at a time; concurrent
connections serialize). The semantic pipeline (P3.5) batches **128 names**
per scheduler round-trip through the supervisor's bounded call; larger
`embed_texts` inputs are still split into one `embed_texts` job per 256-text
chunk. The `extract_jobs` claim order is `priority DESC, id DESC` (new files
first, LIFO within priority). Every scheduler batch is bounded by
**`EMBED_JOB_TIMEOUT` = 120 s** (`embed_texts_bounded`: helper thread +
`recv_timeout`; timeout kills the owned worker to break hung pipe IO and
fails the batch as `Transport` so claims release for retry instead of
stalling the scheduler).

| direction | `type` | fields | description |
|---|---|---|---|
| → worker | `hello` | `protocol_version`, `client`, `client_version` | First message per connection; doubles as the health ping. Mismatch → `error{version_mismatch}` + close. |
| → worker | `embed_texts` | `id`, `texts[1–256]` | Embed a batch; answered by `embeddings`. |
| → worker | `embed_audio` | `id`, `path`, `window_ms?` | Plumbing lands in P3.2; vectors in Phase 4 (real worker answers `unsupported` until mtmd is wired). |
| → worker | `embed_image` | `id`, `path` | Same Phase-4 note as audio. |
| → worker | `status` | `id` | Health probe; answered by `worker_status`. |
| → worker | `shutdown` | `id` | Acknowledge with `shutdown_ack`, then exit (idle-exit stays the normal path). |
| worker → | `hello_ack` | `protocol_version`, `worker_version`, `features[]`, `model_loaded`, `stub` | `features` always has `embed_texts`; audio/image tags appear only when servable (stub mode, or mtmd from Phase 4). |
| worker → | `embeddings` | `id`, `vecs[]`, `dim`, `backend`, `stub` | One L2-normalized vector per input, in order; plain JSON f32 arrays (binary encoding only if profiling demands it). |
| worker → | `worker_status` | `id`, `model_loaded`, `backend?`, `stub`, `dim`, `uptime_secs`, `jobs_served` | Self-reported liveness. |
| worker → | `error` | `id?`, `code`, `message` | `code`: `version_mismatch` \| `invalid_request` \| `model_not_available` \| `embed_failed` \| `unsupported`. Only transport failures trigger a supervisor restart; `error` answers are definitive for the job. |
| worker → | `shutdown_ack` | `id` | Shutdown accepted. |

Worker lifecycle: single-instance mutex `Local\WinSearch.Embed.v1`; model
loads lazily on the first job (missing file → `model_not_available`, worker
stays alive for a later retry after download); idle exit after 5 minutes
without work — process exit returns all GPU memory, which is the full GPU
release. Supervisor: spawn on first request (never before), handshake every
connection (refreshes the cached `model_loaded`/`stub` behind
`EngineStatus.model_loaded`), restart with backoff (`250 ms · 2ⁿ`, 5 s cap)
after a transport failure and retry the in-flight job exactly once.

Test/ops knobs (CLI flags win over env): `--pipe/--idle-secs/--stub/
--model/--backend/--stub-delay-ms`, else `WS_EMBED_PIPE/WS_EMBED_IDLE_SECS/
WS_EMBED_STUB/WS_EMBED_MODEL/WS_EMBED_BACKEND/WS_EMBED_STUB_DELAY_MS`, else
compiled defaults. Supervisor side: `WS_EMBED_BIN` (worker exe override),
`WS_EMBED_PIPE`, `WS_EMBED_BACKOFF_MS`. `WS_EMBED_STUB=1` serves
deterministic stub vectors (FNV-1a seeded, 768-d, unit-norm) with no model
file; `WS_EMBED_TEST_MODEL` gates the real-model tests (never committed).
