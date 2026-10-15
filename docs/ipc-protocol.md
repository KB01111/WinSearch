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
  HTTP `Range`; progress and completion arrive as pushed `model_status`
  frames (throttled to ~4 Hz server-side). `model_delete` refuses to run
  while a download/import is in flight. Every message above is additive:
  `PROTOCOL_VERSION` stays **1**.

## Versioning

Breaking wire changes bump `PROTOCOL_VERSION`. Both sides refuse mismatched
versions at handshake with `error: protocol_version_mismatch` and disconnect.
Additive fields use `#[serde(default)]` semantics and do not bump the version.
