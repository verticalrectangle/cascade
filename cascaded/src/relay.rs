use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    http::StatusCode,
    response::IntoResponse,
};
use cascade_core::MachineInfo;
use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, Mutex};
use uuid::Uuid;

use crate::{json_err, AppState};

#[derive(Debug)]
pub struct RelayError {
    pub status: StatusCode,
    pub message: String,
}

impl RelayError {
    pub fn not_implemented() -> Self {
        Self {
            status: StatusCode::NOT_IMPLEMENTED,
            message: "relay forwarding is not implemented (phase 2)".into(),
        }
    }
}

impl From<RelayError> for (StatusCode, axum::Json<serde_json::Value>) {
    fn from(e: RelayError) -> Self {
        json_err(e.status, &e.message)
    }
}

/// Routes client traffic to a desktop daemon over its outbound `/relay` socket.
///
/// `forward_spawn` / `forward_attach` return 501 until phase 2. The machines
/// table, WSS endpoint, and `machine_id → ws sender` map are real.
#[derive(Clone)]
pub struct RelayRouter {
    db_path: PathBuf,
    connections: Arc<Mutex<HashMap<String, mpsc::UnboundedSender<Message>>>>,
}

#[derive(Debug, Deserialize)]
struct RegisterMsg {
    name: String,
    token: String,
}

#[derive(Debug, Serialize)]
struct RegisterAck {
    id: String,
}

impl RelayRouter {
    pub fn new(db_path: PathBuf) -> anyhow::Result<Self> {
        let conn = Connection::open(&db_path)?;
        init_tables(&conn)?;
        Ok(Self {
            db_path,
            connections: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    pub async fn list_machines_async(&self) -> anyhow::Result<Vec<MachineInfo>> {
        let db_path = self.db_path.clone();
        let rows = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<(String, String)>> {
            let conn = Connection::open(&db_path)?;
            let mut stmt = conn.prepare("SELECT id, name FROM machines ORDER BY name")?;
            let mapped = stmt.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            let mut v = Vec::new();
            for row in mapped {
                v.push(row?);
            }
            Ok(v)
        })
        .await
        .map_err(|e| anyhow::anyhow!(e))??;

        let conns = self.connections.lock().await;
        Ok(rows
            .into_iter()
            .map(|(id, name)| {
                let online = conns.contains_key(&id);
                MachineInfo {
                    id,
                    name,
                    online,
                    is_cloud: false,
                }
            })
            .collect())
    }

    pub async fn forward_spawn(
        &self,
        _machine_id: &str,
        _cwd: &str,
        _model: Option<String>,
    ) -> Result<String, RelayError> {
        Err(RelayError::not_implemented())
    }

    pub async fn forward_attach(
        &self,
        _machine_id: &str,
        _session_id: &str,
    ) -> Result<(), RelayError> {
        Err(RelayError::not_implemented())
    }

    async fn register_connection(&self, machine_id: String, tx: mpsc::UnboundedSender<Message>) {
        self.connections.lock().await.insert(machine_id, tx);
    }

    async fn unregister(&self, machine_id: &str) {
        self.connections.lock().await.remove(machine_id);
    }
}

pub fn init_tables(conn: &Connection) -> anyhow::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS machines (
            id TEXT PRIMARY KEY,
            name TEXT NOT NULL,
            account TEXT NOT NULL DEFAULT '',
            last_seen TEXT NOT NULL,
            token TEXT NOT NULL UNIQUE
        );",
    )?;
    Ok(())
}

fn upsert_machine(db_path: &Path, name: &str, token: &str) -> anyhow::Result<String> {
    let conn = Connection::open(db_path)?;
    let existing: Option<String> = {
        let mut stmt = conn.prepare("SELECT id FROM machines WHERE token = ?1")?;
        let mut rows = stmt.query(rusqlite::params![token])?;
        match rows.next()? {
            Some(row) => Some(row.get(0)?),
            None => None,
        }
    };
    let now = Utc::now().to_rfc3339();
    if let Some(id) = existing {
        conn.execute(
            "UPDATE machines SET name = ?1, last_seen = ?2 WHERE id = ?3",
            rusqlite::params![name, now, id],
        )?;
        return Ok(id);
    }
    let id = Uuid::new_v4().to_string();
    conn.execute(
        "INSERT INTO machines (id, name, account, last_seen, token) VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![id, name, "", now, token],
    )?;
    Ok(id)
}

fn touch_machine(db_path: &Path, id: &str) -> anyhow::Result<()> {
    let conn = Connection::open(db_path)?;
    conn.execute(
        "UPDATE machines SET last_seen = ?1 WHERE id = ?2",
        rusqlite::params![Utc::now().to_rfc3339(), id],
    )?;
    Ok(())
}

pub async fn relay_ws(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_relay(socket, state))
}

async fn handle_relay(socket: WebSocket, state: AppState) {
    let (mut sink, mut stream) = socket.split();
    let first = match stream.next().await {
        Some(Ok(Message::Text(t))) => t.to_string(),
        Some(Ok(Message::Binary(b))) => String::from_utf8_lossy(&b).into_owned(),
        _ => {
            tracing::warn!("relay: expected registration text frame");
            return;
        }
    };
    let reg: RegisterMsg = match serde_json::from_str(&first) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(%e, "relay: bad registration");
            return;
        }
    };
    if reg.token.is_empty() || reg.name.is_empty() {
        tracing::warn!("relay: empty name/token");
        return;
    }
    let db_path = state.db_path.clone();
    let name = reg.name.clone();
    let token = reg.token.clone();
    let machine_id = match tokio::task::spawn_blocking(move || upsert_machine(&db_path, &name, &token))
        .await
    {
        Ok(Ok(id)) => id,
        Ok(Err(e)) => {
            tracing::error!(%e, "relay: upsert machine");
            return;
        }
        Err(e) => {
            tracing::error!(%e, "relay: upsert join");
            return;
        }
    };

    let ack = serde_json::to_string(&RegisterAck {
        id: machine_id.clone(),
    })
    .unwrap_or_else(|_| format!("{{\"id\":\"{machine_id}\"}}"));
    if sink.send(Message::Text(ack.into())).await.is_err() {
        return;
    }

    let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
    state
        .relay
        .register_connection(machine_id.clone(), tx)
        .await;
    tracing::info!(id = %machine_id, name = %reg.name, "machine registered on /relay");

    let write = async {
        while let Some(msg) = rx.recv().await {
            if sink.send(msg).await.is_err() {
                break;
            }
        }
    };
    let db_path = state.db_path.clone();
    let id_for_read = machine_id.clone();
    let read = async {
        while let Some(frame) = stream.next().await {
            match frame {
                Ok(Message::Close(_)) => break,
                Ok(Message::Ping(_)) | Ok(Message::Pong(_)) => {}
                Ok(Message::Text(_)) | Ok(Message::Binary(_)) => {
                    let path = db_path.clone();
                    let id = id_for_read.clone();
                    let _ = tokio::task::spawn_blocking(move || touch_machine(&path, &id)).await;
                }
                Err(_) => break,
            }
        }
    };

    tokio::select! {
        _ = write => {}
        _ = read => {}
    }

    state.relay.unregister(&machine_id).await;
    tracing::info!(id = %machine_id, "machine disconnected from /relay");
}
