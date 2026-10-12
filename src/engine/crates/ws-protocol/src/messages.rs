use serde::{Deserialize, Serialize};

/// Bumped on breaking wire changes. A client and engine with different
/// versions refuse to work together (the engine answers `Error` with
/// `ErrorCode::ProtocolVersionMismatch` and closes the connection).
pub const PROTOCOL_VERSION: u32 = 1;

pub type RequestId = u64;

/// Messages sent by a client (WinUI frontend, CLI) to the engine.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    /// Must be the first message on a connection.
    Hello {
        protocol_version: u32,
        client: String,
        client_version: String,
    },
    /// Heartbeat; answered with `Pong`.
    Ping { id: RequestId },
    /// Execute a search. Results stream back as `Results` frames with the same
    /// id; the last frame has `done: true`. Superseded/cancelled requests are
    /// closed early with `done: true`.
    Search { id: RequestId, query: SearchQuery },
    /// Cancel the in-flight request with this id.
    Cancel { id: RequestId },
    /// Snapshot of engine/indexing status.
    GetStatus { id: RequestId },
}

/// Messages sent by the engine to a client.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    HelloAck {
        protocol_version: u32,
        engine_version: String,
        /// Capability tags, e.g. "search.exact", "search.semantic", "code".
        features: Vec<String>,
    },
    Pong { id: RequestId },
    Results {
        id: RequestId,
        batch: Vec<SearchResult>,
        done: bool,
    },
    /// `id: Some(_)` answers a `GetStatus` request; `id: None` is a pushed
    /// update (e.g. indexing progress) sent proactively to all clients.
    Status {
        id: Option<RequestId>,
        status: EngineStatus,
    },
    Error {
        /// `None` means the error is connection-scoped (e.g. handshake).
        id: Option<RequestId>,
        code: ErrorCode,
        message: String,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    ProtocolVersionMismatch,
    InvalidRequest,
    Internal,
    NotReady,
    Cancelled,
    Unsupported,
}

/// What a search is targeting. `Auto` blends all sources; explicit modes
/// restrict to one source (e.g. `Code` is only ever searched when requested).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum SearchMode {
    #[default]
    Auto,
    Apps,
    Files,
    Code,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct SearchQuery {
    pub text: String,
    pub mode: SearchMode,
    /// Maximum number of results the client wants to see (engine may stream
    /// more for the expanded window).
    pub limit: u32,
    /// Ask for semantic (embedding) retrieval in addition to exact matching.
    /// The engine never blocks exact results on this; semantic results arrive
    /// in later `Results` frames.
    pub include_semantic: bool,
    /// Restrict to these result kinds; empty means all.
    pub categories: Vec<ResultKind>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ResultKind {
    Application,
    File,
    Folder,
    Image,
    Screenshot,
    Document,
    Audio,
    Video,
    Code,
    Calculator,
    Settings,
    Action,
}

/// Where inside a result the match lives (page, timestamp, line). Drives
/// "open at the match" behaviour and the match-explanation text.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MatchLocation {
    Page { page: u32 },
    Timestamp { seconds: f64, end_seconds: f64 },
    Line { line: u32 },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct SearchResult {
    /// Stable within one search; used for selection tracking across updates.
    pub id: String,
    pub kind: ResultKind,
    pub title: String,
    /// Path, URI, or short description depending on kind.
    pub subtitle: String,
    /// Human-readable reason this result matched ("Matches PDF page 14").
    pub match_explanation: String,
    /// Ranking score in [0, 1]; results stream in descending order.
    pub score: f64,
    pub path: Option<String>,
    pub location: Option<MatchLocation>,
    /// Absolute path of a cached thumbnail, when one exists.
    pub thumbnail_path: Option<String>,
}

impl Default for SearchResult {
    fn default() -> Self {
        Self {
            id: String::new(),
            kind: ResultKind::File,
            title: String::new(),
            subtitle: String::new(),
            match_explanation: String::new(),
            score: 0.0,
            path: None,
            location: None,
            thumbnail_path: None,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum EngineRunState {
    /// No indexing running and nothing pending.
    #[default]
    Idle,
    Indexing,
    Paused,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct EngineStatus {
    pub state: EngineRunState,
    /// Short human-readable activity line ("Indexing Pictures", "Paused").
    pub current_activity: String,
    pub files_discovered: u64,
    pub files_indexed: u64,
    pub segments_indexed: u64,
    pub queue_depth: u64,
    /// Whether the embedding model is loaded in the inference worker.
    pub model_loaded: bool,
    pub index_size_bytes: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_message_json_round_trip() {
        let msg = ClientMessage::Search {
            id: 42,
            query: SearchQuery {
                text: "a dog on the beach".into(),
                mode: SearchMode::Auto,
                limit: 20,
                include_semantic: true,
                categories: vec![ResultKind::Image, ResultKind::Video],
            },
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"type\":\"search\""));
        let back: ClientMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(msg, back);
    }

    #[test]
    fn server_message_json_round_trip() {
        let msg = ServerMessage::Results {
            id: 7,
            batch: vec![SearchResult {
                id: "r1".into(),
                kind: ResultKind::Document,
                title: "rental-agreement.pdf".into(),
                subtitle: "C:\\Docs\\rental-agreement.pdf".into(),
                match_explanation: "Matches PDF page 14".into(),
                score: 0.91,
                path: Some("C:\\Docs\\rental-agreement.pdf".into()),
                location: Some(MatchLocation::Page { page: 14 }),
                thumbnail_path: None,
            }],
            done: true,
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"type\":\"results\""));
        assert!(json.contains("\"kind\":\"page\""));
        let back: ServerMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(msg, back);
    }

    #[test]
    fn timestamp_location_round_trip() {
        let msg = ServerMessage::Results {
            id: 1,
            batch: vec![SearchResult {
                location: Some(MatchLocation::Timestamp {
                    seconds: 155.5,
                    end_seconds: 185.5,
                }),
                ..Default::default()
            }],
            done: false,
        };
        let json = serde_json::to_string(&msg).unwrap();
        let back: ServerMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(msg, back);
    }
}
