//! Engine service: shared state behind the IPC server.
//!
//! Phase 1 establishes the request plumbing (handshake, ping, status, search
//! dispatch with cancellation). Search currently answers from an empty index —
//! real retrieval lands in Phase 2, semantic retrieval in Phase 3.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tracing::info;
use ws_platform::pipe::PipeStream;
use ws_protocol::{
    write_message, EngineRunState, EngineStatus, FrameError, RequestId, SearchQuery, ServerMessage,
};

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
}

pub type SharedEngine = Arc<EngineService>;

impl EngineService {
    pub fn new() -> SharedEngine {
        Arc::new(Self {
            status: Mutex::new(EngineStatus {
                state: EngineRunState::Idle,
                current_activity: String::new(),
                ..Default::default()
            }),
            clients: Mutex::new(Vec::new()),
            cancels: Mutex::new(HashMap::new()),
            next_client_id: std::sync::atomic::AtomicU64::new(1),
        })
    }

    pub fn status(&self) -> EngineStatus {
        self.status
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    pub fn set_status(&self, update: impl FnOnce(&mut EngineStatus)) {
        let mut guard = self.status.lock().unwrap_or_else(|p| p.into_inner());
        update(&mut guard);
        let snapshot = guard.clone();
        drop(guard);
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

    /// Executes one search, streaming result batches to the client. Cancellation
    /// is cooperative: the flag is checked before every batch write.
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
        _query: &SearchQuery,
        cancel: &AtomicBool,
    ) -> Result<(), FrameError> {
        // Phase 1: nothing is indexed yet, so the honest answer is "no matches".
        // Phase 2 fills this with application and filename retrieval. A request
        // cancelled before dispatch closes immediately with an empty terminal
        // batch so the client can retire it.
        let _ = cancel.load(Ordering::Relaxed);
        client.send(&ServerMessage::Results {
            id,
            batch: Vec::new(),
            done: true,
        })
    }
}
