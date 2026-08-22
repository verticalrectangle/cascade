use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket};
use axum::extract::{State, WebSocketUpgrade};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use cascade_core::MachineInfo;
use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot, Mutex};
use uuid::Uuid;

use crate::{json_err, AppState};

/// Machine-relay failure surfaced to HTTP clients.
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
    fn offline(machine: &str) -> Self {
        Self {
            status: StatusCode::SERVICE_UNAVAILABLE,
            message: format!("machine {machine} is offline"),
        }
    }
    fn timeout(machine: &str) -> Self {
        Self {
            status: StatusCode::GATEWAY_TIMEOUT,
            message: format!("machine {machine} did not answer in time"),
        }
    }
    fn spawn(message: String) -> Self {
        Self {
            status: StatusCode::BAD_GATEWAY,
            message,
        }
    }
}

impl From<RelayError> for (StatusCode, axum::Json<serde_json::Value>) {
    fn from(e: RelayError) -> Self {
        json_err(e.status, &e.message)
    }
}

/// One row of the cloud's cache of desktop-hosted sessions.
#[derive(Debug, Clone, Serialize)]
pub struct MachineSession {
    pub id: String,
    pub machine: String,
    pub cwd: String,
    pub created_at: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct RelayEnvelope {
    session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    req: Option<String>,
    payload: serde_json::Value,
}

impl RelayEnvelope {
    fn text(&self) -> Option<Message> {
        serde_json::to_string(self)
            .ok()
            .map(|s| Message::Text(s.into()))
    }
}

/// Routes client traffic to desktop daemons over their outbound `/relay`
/// sockets. Spawn uses correlated request/reply (`req`); session events from
/// a desktop fan out to every attached client stream.
#[derive(Clone)]
pub struct RelayRouter {
    db_path: PathBuf,
    connections: Arc<Mutex<HashMap<String, mpsc::UnboundedSender<Message>>>>,
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<serde_json::Value>>>>,
    attached: Arc<Mutex<HashMap<String, Vec<mpsc::UnboundedSender<Message>>>>>,
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

const REPLY_TIMEOUT: Duration = Duration::from_secs(35);

impl RelayRouter {
    pub fn new(db_path: PathBuf) -> anyhow::Result<Self> {
        let conn = Connection::open(&db_path)?;
        init_tables(&conn)?;
        Ok(Self {
            db_path,
            connections: Arc::new(Mutex::new(HashMap::new())),
            pending: Arc::new(Mutex::new(HashMap::new())),
            attached: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    pub async fn list_machines_async(&self, owner: &str) -> anyhow::Result<Vec<MachineInfo>> {
        let db_path = self.db_path.clone();
        let owner = owner.to_string();
        let rows = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<(String, String)>> {
            let conn = Connection::open(&db_path)?;
            let mut stmt =
                conn.prepare("SELECT id, name FROM machines WHERE owner = ?1 ORDER BY name")?;
            let mapped = stmt.query_map(rusqlite::params![owner], |row| {
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

    /// Desktop-hosted sessions known to the cloud (cache of desktop spawns).
    pub fn list_machine_sessions(&self, owner: &str) -> anyhow::Result<Vec<MachineSession>> {
        let conn = Connection::open(&self.db_path)?;
        let mut stmt = conn.prepare(
            "SELECT id, machine, cwd, created_at FROM machine_sessions WHERE owner = ?1 ORDER BY created_at DESC",
        )?;
        let mapped = stmt.query_map([owner], |row| {
            Ok(MachineSession {
                id: row.get(0)?,
                machine: row.get(1)?,
                cwd: row.get(2)?,
                created_at: row.get(3)?,
            })
        })?;
        let mut v = Vec::new();
        for row in mapped {
            v.push(row?);
        }
        Ok(v)
    }

    pub fn machine_of(&self, session_id: &str) -> Option<String> {
        let conn = Connection::open(&self.db_path).ok()?;
        conn.query_row(
            "SELECT machine FROM machine_sessions WHERE id = ?1",
            [session_id],
            |row| row.get::<_, String>(0),
        )
        .ok()
    }

    pub fn session_owner(&self, session_id: &str) -> Option<String> {
        let conn = Connection::open(&self.db_path).ok()?;
        conn.query_row(
            "SELECT owner FROM machine_sessions WHERE id = ?1",
            [session_id],
            |row| row.get::<_, String>(0),
        )
        .ok()
        .filter(|s| !s.is_empty())
    }

    pub fn owns_machine(&self, machine_id: &str, owner: &str) -> bool {
        let Ok(conn) = Connection::open(&self.db_path) else {
            return false;
        };
        conn.query_row(
            "SELECT 1 FROM machines WHERE id = ?1 AND owner = ?2",
            rusqlite::params![machine_id, owner],
            |_| Ok(()),
        )
        .is_ok()
    }

    pub async fn delete_owned_machine(&self, id: &str, owner: &str) -> anyhow::Result<bool> {
        let db_path = self.db_path.clone();
        let id_owned = id.to_string();
        let owner_owned = owner.to_string();
        let deleted = tokio::task::spawn_blocking(move || -> anyhow::Result<bool> {
            let conn = Connection::open(&db_path)?;
            let n = conn.execute(
                "DELETE FROM machines WHERE id = ?1 AND owner = ?2",
                rusqlite::params![id_owned, owner_owned],
            )?;
            if n > 0 {
                let _ = conn.execute(
                    "DELETE FROM machine_sessions WHERE machine = ?1",
                    rusqlite::params![id_owned],
                );
            }
            Ok(n > 0)
        })
        .await
        .map_err(|e| anyhow::anyhow!(e))??;
        if deleted {
            self.unregister(id).await;
        }
        Ok(deleted)
    }

    fn remove_machine_session(&self, session_id: &str) {
        if let Ok(conn) = Connection::open(&self.db_path) {
            let _ = conn.execute("DELETE FROM machine_sessions WHERE id = ?1", [session_id]);
        }
    }

    async fn send_to_machine(
        &self,
        machine_id: &str,
        env: RelayEnvelope,
    ) -> Result<(), RelayError> {
        let msg = env.text().ok_or_else(|| RelayError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: "envelope serialization failed".into(),
        })?;
        let conns = self.connections.lock().await;
        let Some(tx) = conns.get(machine_id) else {
            return Err(RelayError::offline(machine_id));
        };
        tx.send(msg).map_err(|_| RelayError::offline(machine_id))
    }

    /// Request/reply over the relay socket: register a oneshot under `req`,
    /// ship the envelope, await the desktop's reply.
    async fn request(
        &self,
        machine_id: &str,
        payload: serde_json::Value,
    ) -> Result<serde_json::Value, RelayError> {
        let req = Uuid::new_v4().to_string();
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(req.clone(), tx);
        let env = RelayEnvelope {
            session_id: None,
            req: Some(req.clone()),
            payload,
        };
        if let Err(e) = self.send_to_machine(machine_id, env).await {
            self.pending.lock().await.remove(&req);
            return Err(e);
        }
        match tokio::time::timeout(REPLY_TIMEOUT, rx).await {
            Ok(Ok(reply)) => Ok(reply),
            Ok(Err(_)) => Err(RelayError::offline(machine_id)),
            Err(_) => {
                self.pending.lock().await.remove(&req);
                Err(RelayError::timeout(machine_id))
            }
        }
    }

    /// Forward a spawn to a desktop daemon. On success the desktop's session
    /// id becomes the cloud-visible id and is cached for listing.
    pub async fn forward_spawn(
        &self,
        machine_id: &str,
        cwd: &str,
        model: Option<String>,
        owner: &str,
    ) -> Result<String, RelayError> {
        let payload = serde_json::json!({ "kind": "spawn", "cwd": cwd, "model": model });
        let reply = self.request(machine_id, payload).await?;
        match reply.get("kind").and_then(|k| k.as_str()) {
            Some("spawn_ok") => {
                let Some(id) = reply.get("id").and_then(|i| i.as_str()) else {
                    return Err(RelayError::spawn("machine reply missing session id".into()));
                };
                let row = MachineSession {
                    id: id.to_string(),
                    machine: machine_id.to_string(),
                    cwd: cwd.to_string(),
                    created_at: Utc::now().to_rfc3339(),
                };
                let db_path = self.db_path.clone();
                let row2 = row.clone();
                let owner = owner.to_string();
                let _ = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
                    let conn = Connection::open(&db_path)?;
                    conn.execute(
                        "INSERT OR REPLACE INTO machine_sessions (id, machine, cwd, created_at, owner) VALUES (?1, ?2, ?3, ?4, ?5)",
                        rusqlite::params![row2.id, row2.machine, row2.cwd, row2.created_at, owner],
                    )?;
                    Ok(())
                })
                .await;
                Ok(id.to_string())
            }
            Some("spawn_err") => Err(RelayError::spawn(
                reply
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("machine spawn failed")
                    .to_string(),
            )),
            _ => Err(RelayError::spawn("unexpected machine reply".into())),
        }
    }

    /// Fan a desktop event out to every attached client stream. Clients speak
    /// bare SessionEvent JSON — the envelope wrapper is machine-relay routing
    /// only, so unwrap before sending.
    async fn fan_out(&self, session_id: &str, payload: serde_json::Value) {
        let Some(text) = serde_json::to_string(&payload).ok() else {
            return;
        };
        let msg = Message::Text(text.into());
        let mut map = self.attached.lock().await;
        if let Some(subs) = map.get_mut(session_id) {
            subs.retain(|tx| tx.send(msg.clone()).is_ok());
        }
    }

    /// Wire a client stream socket into a desktop-hosted session: request the
    /// snapshot, then proxy commands out and events in until either side ends.
    pub async fn proxy_stream(
        self,
        machine_id: String,
        session_id: String,
        mut socket: WebSocket,
        read_only: bool,
    ) {
        // Ask the desktop to start streaming this session (idempotent for
        // already-spawned sessions) and to send a snapshot.
        let env = RelayEnvelope {
            session_id: Some(session_id.clone()),
            req: None,
            payload: serde_json::json!({ "kind": "get_snapshot" }),
        };
        let _ = self.send_to_machine(&machine_id, env).await;

        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        self.attached
            .lock()
            .await
            .entry(session_id.clone())
            .or_default()
            .push(tx);

        loop {
            tokio::select! {
                out = rx.recv() => {
                    match out {
                        Some(msg) => {
                            if socket.send(msg).await.is_err() {
                                break;
                            }
                        }
                        None => break,
                    }
                }
                incoming = socket.recv() => {
                    match incoming {
                        Some(Ok(Message::Text(text))) => {
                            if read_only {
                                continue;
                            }
                            // Wrap the client CloudCommand in an envelope.
                            let payload: serde_json::Value = match serde_json::from_str(&text) {
                                Ok(v) => v,
                                Err(_) => continue,
                            };
                            let env = RelayEnvelope {
                                session_id: Some(session_id.clone()),
                                req: None,
                                payload,
                            };
                            if self.send_to_machine(&machine_id, env).await.is_err() {
                                break;
                            }
                        }
                        Some(Ok(_)) => {}
                        Some(Err(_)) | None => break,
                    }
                }
            }
        }

        // Detach this client.
        let mut map = self.attached.lock().await;
        if let Some(subs) = map.get_mut(&session_id) {
            subs.retain(|tx| !tx.is_closed());
            if subs.is_empty() {
                map.remove(&session_id);
            }
        }
    }

    /// Forward a session shutdown to the owning machine and drop the cached row.
    pub async fn forward_shutdown(&self, session_id: &str) -> Result<(), RelayError> {
        let Some(machine_id) = self.machine_of(session_id) else {
            return Ok(()); // not a machine session; caller handles locally
        };
        let env = RelayEnvelope {
            session_id: Some(session_id.to_string()),
            req: None,
            payload: serde_json::json!({ "kind": "shutdown" }),
        };
        let result = self.send_to_machine(&machine_id, env).await;
        self.remove_machine_session(session_id);
        // Events stream ends with ProcessExited; clients see the session die.
        result.map(|_| ())
    }

    /// Route one desktop→cloud frame: replies resolve pending requests;
    /// session-scoped payloads fan out to attached clients.
    async fn route_from_machine(&self, text: &str) {
        let Ok(env) = serde_json::from_str::<RelayEnvelope>(text) else {
            tracing::warn!(payload = text, "relay: bad envelope from machine");
            return;
        };
        if let Some(req) = env.req.clone() {
            let mut pending = self.pending.lock().await;
            if let Some(tx) = pending.remove(&req) {
                let _ = tx.send(env.payload);
            }
            return;
        }
        if let Some(sid) = env.session_id.clone() {
            self.fan_out(&sid, env.payload).await;
        }
    }

    /// Whether a machine currently holds an outbound relay connection.
    pub async fn machine_online(&self, machine_id: &str) -> bool {
        self.connections.lock().await.contains_key(machine_id)
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
            token TEXT NOT NULL UNIQUE,
            owner TEXT NOT NULL DEFAULT ''
        );
        CREATE TABLE IF NOT EXISTS machine_sessions (
            id TEXT PRIMARY KEY,
            machine TEXT NOT NULL,
            cwd TEXT NOT NULL,
            created_at TEXT NOT NULL,
            owner TEXT NOT NULL DEFAULT ''
        );
        CREATE TABLE IF NOT EXISTS machine_tokens (
            token TEXT PRIMARY KEY,
            owner TEXT NOT NULL,
            created_at TEXT NOT NULL
        );",
    )?;
    crate::auth::ensure_column(conn, "machines", "owner", "TEXT NOT NULL DEFAULT ''")?;
    crate::auth::ensure_column(
        conn,
        "machine_sessions",
        "owner",
        "TEXT NOT NULL DEFAULT ''",
    )?;
    Ok(())
}

pub fn mint_machine_token(db_path: &Path, owner: &str) -> anyhow::Result<String> {
    let token = Uuid::new_v4().to_string();
    let conn = Connection::open(db_path)?;
    conn.execute(
        "INSERT INTO machine_tokens (token, owner, created_at) VALUES (?1, ?2, ?3)",
        rusqlite::params![token, owner, Utc::now().to_rfc3339()],
    )?;
    Ok(token)
}

pub fn owner_for_token(db_path: &Path, token: &str) -> Option<String> {
    let conn = Connection::open(db_path).ok()?;
    conn.query_row(
        "SELECT owner FROM machine_tokens WHERE token = ?1",
        [token],
        |row| row.get::<_, String>(0),
    )
    .ok()
}

pub fn backfill_machine_tokens(conn: &Connection) -> anyhow::Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO machine_tokens (token, owner, created_at)
         SELECT token, owner, last_seen FROM machines
         WHERE token != '' AND owner != ''",
        [],
    )?;
    Ok(())
}

fn upsert_machine(db_path: &Path, name: &str, token: &str) -> anyhow::Result<String> {
    let conn = Connection::open(db_path)?;
    let owner: String = conn
        .query_row(
            "SELECT owner FROM machine_tokens WHERE token = ?1",
            [token],
            |row| row.get(0),
        )
        .map_err(|_| anyhow::anyhow!("unknown machine token"))?;
    // Re-register with a known token reuses the machine id.
    let existing: Option<String> = conn
        .query_row("SELECT id FROM machines WHERE token = ?1", [token], |row| {
            row.get::<_, String>(0)
        })
        .ok();
    let id = match existing {
        Some(id) => {
            conn.execute(
                "UPDATE machines SET name = ?1, last_seen = ?2, owner = ?3 WHERE id = ?4",
                rusqlite::params![name, Utc::now().to_rfc3339(), owner, id],
            )?;
            id
        }
        None => {
            let id = Uuid::new_v4().to_string();
            conn.execute(
                "INSERT INTO machines (id, name, account, last_seen, token, owner) VALUES (?1, ?2, '', ?3, ?4, ?5)",
                rusqlite::params![id, name, Utc::now().to_rfc3339(), token, owner],
            )?;
            id
        }
    };
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

pub async fn relay_ws(ws: WebSocketUpgrade, State(state): State<AppState>) -> impl IntoResponse {
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
    let machine_id =
        match tokio::task::spawn_blocking(move || upsert_machine(&db_path, &name, &token)).await {
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
    let router = state.relay.clone();
    let db_path = state.db_path.clone();
    let id_for_read = machine_id.clone();
    let read = async {
        while let Some(frame) = stream.next().await {
            match frame {
                Ok(Message::Close(_)) => break,
                Ok(Message::Ping(_)) | Ok(Message::Pong(_)) => {}
                Ok(Message::Text(t)) => {
                    router.route_from_machine(&t).await;
                    let path = db_path.clone();
                    let id = id_for_read.clone();
                    let _ = tokio::task::spawn_blocking(move || touch_machine(&path, &id)).await;
                }
                Ok(Message::Binary(b)) => {
                    if let Ok(text) = String::from_utf8(b.to_vec()) {
                        router.route_from_machine(&text).await;
                    }
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
