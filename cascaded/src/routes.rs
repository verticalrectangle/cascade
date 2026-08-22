use std::path::PathBuf;

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Path, State,
    },
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use cascade_core::{CloudCommand, SessionManager, SpawnOptions};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};

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

pub async fn list_machines(
    State(state): State<AppState>,
    _user: AuthUser,
) -> Result<Json<Vec<cascade_core::MachineInfo>>, (StatusCode, Json<serde_json::Value>)> {
    let mut machines = vec![cascade_core::MachineInfo {
        id: "cloud".into(),
        name: "cloud".into(),
        online: true,
        is_cloud: true,
    }];
    let remote = state
        .relay
        .list_machines_async()
        .await
        .map_err(|e| json_err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?;
    machines.extend(remote);
    Ok(Json(machines))
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
    _user: AuthUser,
) -> Result<Json<Vec<ListedSession>>, (StatusCode, Json<serde_json::Value>)> {
    // Single-user phase: no per-account filter.
    let mut out: Vec<ListedSession> = state
        .sessions
        .list()
        .await
        .into_iter()
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
    let terminals = tokio::task::spawn_blocking(move || crate::terminal::list(&db))
        .await
        .map_err(|e| json_err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?
        .map_err(|e| json_err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?;
    // Desktop-hosted sessions (spawned via the machine relay).
    for m in state.relay.list_machine_sessions().unwrap_or_default() {
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
    _user: AuthUser,
    Json(body): Json<CreateSessionRequest>,
) -> Result<Json<CreateSessionResponse>, (StatusCode, Json<serde_json::Value>)> {
    if body.cwd.trim().is_empty() {
        return Err(json_err(StatusCode::BAD_REQUEST, "cwd is required"));
    }
    let machine = body.machine.as_deref().unwrap_or("cloud");
    if machine != "cloud" {
        return state
            .relay
            .forward_spawn(machine, &body.cwd, body.model)
            .await
            .map(|id| Json(CreateSessionResponse { id }))
            .map_err(Into::into);
    }

    let opts = SpawnOptions {
        cwd: PathBuf::from(&body.cwd),
        model: body.model,
        ..SpawnOptions::default()
    };
    let id = state
        .sessions
        .spawn(opts)
        .await
        .map_err(|e| json_err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?;

    if let Some(mut meta) = state
        .sessions
        .list()
        .await
        .into_iter()
        .find(|m| m.id == id)
    {
        meta.machine = "cloud".into();
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
    _user: AuthUser,
    Path(id): Path<String>,
) -> Result<StatusCode, (StatusCode, Json<serde_json::Value>)> {
    // Desktop-hosted session: forward the shutdown and drop the cached row.
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

pub async fn session_stream(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    Path(id): Path<String>,
    _user: AuthUser,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    if let Some(session) = state.sessions.get(&id).await {
        return Ok(ws.on_upgrade(move |socket| handle_stream(socket, session, state.sessions.clone())));
    }
    // Not cloud-local: a desktop-hosted session behind the machine relay.
    if let Some(machine) = state.relay.machine_of(&id) {
        if !state.relay.machine_online(&machine).await {
            return Err(json_err(StatusCode::SERVICE_UNAVAILABLE, &format!("machine {machine} is offline")));
        }
        let router = state.relay.clone();
        return Ok(ws.on_upgrade(move |socket| async move {
            router.proxy_stream(machine, id, socket).await;
        }));
    }
    Err(json_err(StatusCode::NOT_FOUND, "session not found"))
}

async fn handle_stream(
    socket: WebSocket,
    session: cascade_core::OmpSession,
    _manager: SessionManager,
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
