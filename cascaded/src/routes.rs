use std::path::PathBuf;

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Path, State,
    },
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    Json,
};
use cascade_core::{CloudCommand, SessionManager, SpawnOptions};
use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::auth::AuthUser;
use crate::{json_err, AppState};

#[derive(Debug, Serialize)]
pub struct CreateSessionResponse {
    pub id: String,
}

#[derive(Debug, Deserialize)]
pub struct CreateSessionRequest {
    pub machine: Option<String>,
    pub cwd: String,
    pub model: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct TokenResponse {
    pub token: String,
}

#[derive(Debug, Serialize)]
pub struct ShareResponse {
    pub token: String,
    pub url: String,
}

enum StreamAccess {
    Owner,
    ReadOnly,
}

pub fn init_share_tables(conn: &Connection) -> anyhow::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS shares (
            token TEXT PRIMARY KEY,
            session_id TEXT NOT NULL,
            created_at TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS shares_session ON shares(session_id);",
    )?;
    Ok(())
}

pub async fn list_machines(
    State(state): State<AppState>,
    user: AuthUser,
) -> Result<Json<Vec<cascade_core::MachineInfo>>, (StatusCode, Json<serde_json::Value>)> {
    let mut machines = vec![cascade_core::MachineInfo {
        id: "cloud".into(),
        name: "cloud".into(),
        online: true,
        is_cloud: true,
    }];
    let remote = state
        .relay
        .list_machines_async(&user.uid)
        .await
        .map_err(|e| json_err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?;
    machines.extend(remote);
    Ok(Json(machines))
}

pub async fn mint_machine_token(
    State(state): State<AppState>,
    user: AuthUser,
) -> Result<Json<TokenResponse>, (StatusCode, Json<serde_json::Value>)> {
    let db = state.db_path.clone();
    let uid = user.uid;
    let token = tokio::task::spawn_blocking(move || crate::relay::mint_machine_token(&db, &uid))
        .await
        .map_err(|e| json_err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?
        .map_err(|e| json_err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?;
    Ok(Json(TokenResponse { token }))
}

pub async fn delete_machine(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
) -> Result<StatusCode, (StatusCode, Json<serde_json::Value>)> {
    let deleted = state
        .relay
        .delete_owned_machine(&id, &user.uid)
        .await
        .map_err(|e| json_err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?;
    if !deleted {
        return Err(json_err(StatusCode::NOT_FOUND, "machine not found"));
    }
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Serialize)]
pub struct ListedSession {
    pub id: String,
    pub omp_session_id: Option<String>,
    pub name: Option<String>,
    pub cwd: String,
    pub session_file: Option<String>,
    pub machine: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub last_active: chrono::DateTime<chrono::Utc>,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub join_handle: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub view_handle: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<i64>,
}

pub async fn list_sessions(
    State(state): State<AppState>,
    user: AuthUser,
) -> Result<Json<Vec<ListedSession>>, (StatusCode, Json<serde_json::Value>)> {
    let uid = user.uid.clone();
    let mut out: Vec<ListedSession> = state
        .sessions
        .list()
        .await
        .into_iter()
        .filter(|m| m.owner == uid)
        .map(|m| ListedSession {
            id: m.id,
            omp_session_id: m.omp_session_id,
            name: m.name,
            cwd: m.cwd,
            session_file: m.session_file,
            machine: m.machine,
            created_at: m.created_at,
            last_active: m.last_active,
            kind: "managed".into(),
            join_handle: None,
            view_handle: None,
            pid: None,
        })
        .collect();
    let db = state.db_path.clone();
    let term_uid = uid.clone();
    let terminals = tokio::task::spawn_blocking(move || crate::terminal::list(&db, &term_uid))
        .await
        .map_err(|e| json_err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?
        .map_err(|e| json_err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?;
    for m in state.relay.list_machine_sessions(&uid).unwrap_or_default() {
        let created = chrono::DateTime::parse_from_rfc3339(&m.created_at)
            .map(|d| d.with_timezone(&chrono::Utc))
            .unwrap_or_else(|_| chrono::Utc::now());
        out.push(ListedSession {
            id: m.id,
            omp_session_id: None,
            name: None,
            cwd: m.cwd,
            session_file: None,
            machine: m.machine,
            created_at: created,
            last_active: created,
            kind: "managed".into(),
            join_handle: None,
            view_handle: None,
            pid: None,
        });
    }
    for t in terminals {
        let created = chrono::DateTime::parse_from_rfc3339(&t.created_at)
            .map(|d| d.with_timezone(&chrono::Utc))
            .unwrap_or_else(|_| chrono::Utc::now());
        out.push(ListedSession {
            id: t.session_id,
            omp_session_id: None,
            name: t.title,
            cwd: t.cwd,
            session_file: None,
            machine: t.machine,
            created_at: created,
            last_active: created,
            kind: "terminal".into(),
            join_handle: Some(t.join_handle),
            view_handle: Some(t.view_handle),
            pid: t.pid,
        });
    }
    Ok(Json(out))
}

pub async fn create_session(
    State(state): State<AppState>,
    user: AuthUser,
    Json(body): Json<CreateSessionRequest>,
) -> Result<Json<CreateSessionResponse>, (StatusCode, Json<serde_json::Value>)> {
    let cwd = if body.cwd.trim().is_empty() {
        std::env::var("HOME").unwrap_or_else(|_| "/".into())
    } else {
        body.cwd.clone()
    };
    let cwd = cwd.as_str();
    let machine = body.machine.as_deref().unwrap_or("cloud");
    if machine == "cloud" {
        if !user.is_admin {
            return Err(json_err(
                StatusCode::FORBIDDEN,
                "cloud spawn requires admin",
            ));
        }
    } else if !state.relay.owns_machine(machine, &user.uid) {
        return Err(json_err(StatusCode::FORBIDDEN, "machine not found"));
    }

    if machine != "cloud" {
        return state
            .relay
            .forward_spawn(machine, cwd, body.model, &user.uid)
            .await
            .map(|id| Json(CreateSessionResponse { id }))
            .map_err(Into::into);
    }

    let opts = SpawnOptions {
        cwd: PathBuf::from(cwd),
        model: body.model,
        ..SpawnOptions::default()
    };
    let id = state
        .sessions
        .spawn(opts)
        .await
        .map_err(|e| json_err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?;

    if let Some(mut meta) = state.sessions.list().await.into_iter().find(|m| m.id == id) {
        meta.machine = "cloud".into();
        meta.owner = user.uid;
        let db = state.db_path.clone();
        let _ = tokio::task::spawn_blocking(move || {
            cascade_core::SessionRegistry::open(&db).and_then(|reg| reg.upsert(&meta))
        })
        .await;
    }

    Ok(Json(CreateSessionResponse { id }))
}

pub async fn delete_session(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
) -> Result<StatusCode, (StatusCode, Json<serde_json::Value>)> {
    if !owns_session(&state, &user.uid, &id).await {
        return Err(json_err(StatusCode::NOT_FOUND, "session not found"));
    }
    if state.sessions.get(&id).await.is_none() && state.relay.machine_of(&id).is_some() {
        state
            .relay
            .forward_shutdown(&id)
            .await
            .map_err(|e| (e.status, Json(serde_json::json!({ "error": e.message }))))?;
        return Ok(StatusCode::NO_CONTENT);
    }
    state
        .sessions
        .shutdown(&id)
        .await
        .map_err(|e| json_err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?;
    let db = state.db_path.clone();
    let id_rm = id.clone();
    let _ = tokio::task::spawn_blocking(move || {
        cascade_core::SessionRegistry::open(&db).and_then(|reg| reg.remove(&id_rm))
    })
    .await;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn create_share(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
) -> Result<Json<ShareResponse>, (StatusCode, Json<serde_json::Value>)> {
    if !owns_session(&state, &user.uid, &id).await {
        return Err(json_err(StatusCode::NOT_FOUND, "session not found"));
    }
    let db = state.db_path.clone();
    let session_id = id.clone();
    let token = tokio::task::spawn_blocking(move || mint_share(&db, &session_id))
        .await
        .map_err(|e| json_err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?
        .map_err(|e| json_err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?;
    Ok(Json(ShareResponse {
        token,
        url: format!("/sessions/{id}/stream"),
    }))
}

pub async fn delete_share(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
) -> Result<StatusCode, (StatusCode, Json<serde_json::Value>)> {
    if !owns_session(&state, &user.uid, &id).await {
        return Err(json_err(StatusCode::NOT_FOUND, "session not found"));
    }
    let db = state.db_path.clone();
    let session_id = id.clone();
    tokio::task::spawn_blocking(move || revoke_shares(&db, &session_id))
        .await
        .map_err(|e| json_err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?
        .map_err(|e| json_err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn session_stream(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let token = crate::auth::bearer_from_headers(&headers)
        .ok_or_else(|| json_err(StatusCode::UNAUTHORIZED, "missing authorization"))?;
    let access = authorize_stream(&state, &id, token).await?;
    let read_only = match access {
        StreamAccess::Owner => false,
        StreamAccess::ReadOnly => true,
    };

    if let Some(session) = state.sessions.get(&id).await {
        return Ok(ws.on_upgrade(move |socket| {
            handle_stream(socket, session, state.sessions.clone(), read_only)
        }));
    }
    // Not cloud-local: a desktop-hosted session behind the machine relay.
    if let Some(machine) = state.relay.machine_of(&id) {
        if !state.relay.machine_online(&machine).await {
            return Err(json_err(
                StatusCode::SERVICE_UNAVAILABLE,
                &format!("machine {machine} is offline"),
            ));
        }
        let router = state.relay.clone();
        return Ok(ws.on_upgrade(move |socket| async move {
            router.proxy_stream(machine, id, socket, read_only).await;
        }));
    }
    Err(json_err(StatusCode::NOT_FOUND, "session not found"))
}

async fn authorize_stream(
    state: &AppState,
    session_id: &str,
    token: &str,
) -> Result<StreamAccess, (StatusCode, Json<serde_json::Value>)> {
    if let Ok(email) = crate::auth::verify_token(&state.jwt_secret, token) {
        let db = state.db_path.clone();
        let email_lookup = email.clone();
        let resolved = tokio::task::spawn_blocking(move || {
            let conn = Connection::open(&db)?;
            Ok::<_, anyhow::Error>(crate::auth::resolve_user(&conn, &email_lookup))
        })
        .await
        .map_err(|e| json_err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?
        .map_err(|e| json_err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?;
        let Some((uid, _)) = resolved else {
            return Err(json_err(StatusCode::UNAUTHORIZED, "invalid token"));
        };
        if !owns_session(state, &uid, session_id).await {
            return Err(json_err(StatusCode::NOT_FOUND, "session not found"));
        }
        return Ok(StreamAccess::Owner);
    }

    let db = state.db_path.clone();
    let tok = token.to_string();
    let sid = session_id.to_string();
    let found = tokio::task::spawn_blocking(move || share_matches(&db, &tok, &sid))
        .await
        .map_err(|e| json_err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?
        .map_err(|e| json_err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?;
    if found {
        Ok(StreamAccess::ReadOnly)
    } else {
        Err(json_err(StatusCode::UNAUTHORIZED, "invalid token"))
    }
}

async fn owns_session(state: &AppState, uid: &str, session_id: &str) -> bool {
    let db = state.db_path.clone();
    let sid = session_id.to_string();
    let uid_owned = uid.to_string();
    let registry_owner = tokio::task::spawn_blocking(move || -> anyhow::Result<Option<String>> {
        let reg = cascade_core::SessionRegistry::open(&db)?;
        Ok(reg.get(&sid)?.map(|m| m.owner))
    })
    .await
    .ok()
    .and_then(|r| r.ok())
    .flatten();
    if let Some(owner) = registry_owner {
        if owner == uid {
            return true;
        }
    }
    match state.relay.session_owner(session_id) {
        Some(owner) => owner == uid_owned,
        None => false,
    }
}

fn mint_share(db: &std::path::Path, session_id: &str) -> anyhow::Result<String> {
    let conn = Connection::open(db)?;
    conn.execute("DELETE FROM shares WHERE session_id = ?1", [session_id])?;
    let token = Uuid::new_v4().to_string();
    conn.execute(
        "INSERT INTO shares (token, session_id, created_at) VALUES (?1, ?2, ?3)",
        rusqlite::params![token, session_id, Utc::now().to_rfc3339()],
    )?;
    Ok(token)
}

fn revoke_shares(db: &std::path::Path, session_id: &str) -> anyhow::Result<()> {
    let conn = Connection::open(db)?;
    conn.execute("DELETE FROM shares WHERE session_id = ?1", [session_id])?;
    Ok(())
}

fn share_matches(db: &std::path::Path, token: &str, session_id: &str) -> anyhow::Result<bool> {
    let conn = Connection::open(db)?;
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM shares WHERE token = ?1 AND session_id = ?2",
        rusqlite::params![token, session_id],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

async fn handle_stream(
    socket: WebSocket,
    session: cascade_core::OmpSession,
    _manager: SessionManager,
    read_only: bool,
) {
    let (mut sink, mut stream) = socket.split();

    let snapshot = session.snapshot().await;
    let frame = cascade_core::SessionEvent::Snapshot(snapshot);
    if let Ok(text) = serde_json::to_string(&frame) {
        let _ = sink.send(Message::Text(text.into())).await;
    }

    let mut events = session.subscribe();

    loop {
        tokio::select! {
            incoming = stream.next() => {
                match incoming {
                    Some(Ok(Message::Text(text))) => {
                        if read_only {
                            continue;
                        }
                        match serde_json::from_str::<CloudCommand>(&text) {
                            Ok(CloudCommand::Prompt { message }) => {
                                if let Err(e) = session.prompt(message).await {
                                    tracing::warn!(%e, "prompt failed");
                                }
                            }
                            Ok(CloudCommand::Abort) => {
                                if let Err(e) = session.abort().await {
                                    tracing::warn!(%e, "abort failed");
                                }
                            }
                            Ok(CloudCommand::AnswerUi { request_id, response }) => {
                                if let Err(e) = session.answer_ui(request_id, response).await {
                                    tracing::warn!(%e, "answer_ui failed");
                                }
                            }
                            Ok(CloudCommand::SetModel { provider, model_id }) => {
                                if let Err(e) = session.set_model(provider, model_id).await {
                                    tracing::warn!(%e, "set_model failed");
                                }
                            }
                            Ok(CloudCommand::SetThinking { level }) => {
                                if let Err(e) = session.set_thinking_level(level).await {
                                    tracing::warn!(%e, "set_thinking failed");
                                }
                            }
                            Ok(CloudCommand::GetState) => match session.get_state().await {
                                Ok(state) => {
                                    let frame = cascade_core::SessionEvent::StateChanged;
                                    // Re-emit the full state payload alongside the event
                                    // (plus the model catalog) so thin clients don't need a
                                    // separate REST round-trip.
                                    let models = session.available_models().await.unwrap_or_default();
                                    let mut ev = serde_json::to_value(&frame)
                                        .unwrap_or(serde_json::Value::Null);
                                    if let Some(map) = ev.as_object_mut() {
                                        map.insert(
                                            "state".into(),
                                            serde_json::to_value(&state).unwrap_or(serde_json::Value::Null),
                                        );
                                        map.insert(
                                            "models".into(),
                                            serde_json::to_value(&models).unwrap_or(serde_json::Value::Null),
                                        );
                                    }
                                    if let Ok(text) = serde_json::to_string(&ev) {
                                        let _ = sink.send(Message::Text(text.into())).await;
                                    }
                                }
                                Err(e) => tracing::warn!(%e, "get_state failed"),
                            },
                            Err(e) => {
                                tracing::warn!(%e, "invalid CloudCommand");
                            }
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(_)) => {}
                    Some(Err(_)) => break,
                }
            }
            event = events.recv() => {
                match event {
                    Ok(ev) => {
                        match serde_json::to_string(&ev) {
                            Ok(json) => {
                                if sink.send(Message::Text(json.into())).await.is_err() {
                                    break;
                                }
                            }
                            Err(e) => tracing::warn!(%e, "serialize SessionEvent"),
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(skipped = n, "session event lag");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }
}
