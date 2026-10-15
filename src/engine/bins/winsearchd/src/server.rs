//! Per-connection protocol loop: handshake, then request dispatch until the
//! client disconnects.

use std::io::Read;
use std::sync::{Arc, Mutex};

use tracing::{info, warn};
use ws_platform::pipe::PipeStream;
use ws_protocol::{read_message, ClientMessage, ErrorCode, ServerMessage, PROTOCOL_VERSION};

use crate::engine::SharedEngine;

pub const ENGINE_VERSION: &str = env!("CARGO_PKG_VERSION");

fn features() -> Vec<String> {
    // Capability tags advertised at handshake. Grows with the phases:
    // "search.semantic" appears in Phase 3, "code" in Phase 5.
    vec!["search.exact".to_string(), "search.semantic".to_string()]
}

pub fn handle_connection(stream: PipeStream, engine: SharedEngine) {
    let stream = Arc::new(Mutex::new(stream));
    let client = engine.register_client(stream.clone());

    let mut reader = ReaderGuard { stream: stream.clone() };
    let first = match read_message::<_, ClientMessage>(&mut reader) {
        Ok(Some(msg)) => msg,
        Ok(None) => {
            info!("client disconnected before handshake");
            engine.unregister_client(&client);
            return;
        }
        Err(e) => {
            warn!(error = %e, "handshake read failed");
            engine.unregister_client(&client);
            return;
        }
    };

    let ClientMessage::Hello {
        protocol_version,
        client: client_name,
        client_version,
    } = first
    else {
        let _ = client.send(&ServerMessage::Error {
            id: None,
            code: ErrorCode::InvalidRequest,
            message: "first message must be hello".into(),
        });
        engine.unregister_client(&client);
        return;
    };

    if protocol_version != PROTOCOL_VERSION {
        let _ = client.send(&ServerMessage::Error {
            id: None,
            code: ErrorCode::ProtocolVersionMismatch,
            message: format!(
                "engine speaks protocol v{PROTOCOL_VERSION}, client speaks v{protocol_version}"
            ),
        });
        engine.unregister_client(&client);
        return;
    }

    info!(client = %client_name, version = %client_version, "client connected");
    let _ = client.send(&ServerMessage::HelloAck {
        protocol_version: PROTOCOL_VERSION,
        engine_version: ENGINE_VERSION.to_string(),
        features: features(),
    });
    let _ = client.send(&ServerMessage::Status {
        id: None,
        status: engine.status(),
    });

    loop {
        let msg = match read_message::<_, ClientMessage>(&mut reader) {
            Ok(Some(msg)) => msg,
            Ok(None) => {
                info!(client = %client_name, "client disconnected");
                break;
            }
            Err(e) => {
                warn!(client = %client_name, error = %e, "connection read failed");
                break;
            }
        };

        match msg {
            ClientMessage::Hello { .. } => {
                let _ = client.send(&ServerMessage::Error {
                    id: None,
                    code: ErrorCode::InvalidRequest,
                    message: "hello already completed".into(),
                });
            }
            ClientMessage::Ping { id } => {
                let _ = client.send(&ServerMessage::Pong { id });
            }
            ClientMessage::Search { id, query } => {
                engine.search(&client, id, query);
            }
            ClientMessage::Cancel { id } => {
                engine.cancel(id);
            }
            ClientMessage::GetStatus { id } => {
                let _ = client.send(&ServerMessage::Status {
                    id: Some(id),
                    status: engine.status(),
                });
            }
            ClientMessage::ModelDownloadStart { id } => {
                engine.model_download_start(&client, id);
            }
            ClientMessage::ModelDownloadCancel { id } => {
                engine.model_download_cancel(&client, id);
            }
            ClientMessage::ModelImport { id, path } => {
                engine.model_import(&client, id, path);
            }
            ClientMessage::ModelDelete { id } => {
                engine.model_delete(&client, id);
            }
            ClientMessage::GetModelStatus { id } => {
                engine.model_status(&client, id);
            }
            ClientMessage::GetConfig { id } => {
                engine.get_config(&client, id);
            }
            ClientMessage::SetConfig { id, config } => {
                engine.set_config(&client, id, config);
            }
        }
    }

    engine.unregister_client(&client);
}

/// Adapter giving the framing codec read access to the shared pipe stream.
struct ReaderGuard {
    stream: Arc<Mutex<PipeStream>>,
}

impl Read for ReaderGuard {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let mut guard = self
            .stream
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.read(buf)
    }
}
