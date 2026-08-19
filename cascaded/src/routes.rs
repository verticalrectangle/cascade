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

pub async fn list_sessions(
    State(state): State<AppState>,
    _user: AuthUser,
) -> Result<Json<Vec<cascade_core::SessionMeta>>, (StatusCode, Json<serde_json::Value>)> {
    // Single-user phase: no per-account filter.
    Ok(Json(state.sessions.list().await))
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
    if state.sessions.get(&id).await.is_none() {
        let listed = state.sessions.list().await;
        if listed.iter().any(|m| m.id == id && m.machine != "cloud") {
            return Err(crate::relay::RelayError::not_implemented().into());
        }
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
    let Some(session) = state.sessions.get(&id).await else {
        let listed = state.sessions.list().await;
        if listed.iter().any(|m| m.id == id && m.machine != "cloud") {
            return Err(crate::relay::RelayError::not_implemented().into());
        }
        return Err(json_err(StatusCode::NOT_FOUND, "session not found"));
    };
    Ok(ws.on_upgrade(move |socket| handle_stream(socket, session, state.sessions.clone())))
}

async fn handle_stream(
    socket: WebSocket,
    session: cascade_core::OmpSession,
    _manager: SessionManager,
) {
    let (mut sink, mut stream) = socket.split();

    let snapshot = session.snapshot().await;
    if let Ok(mut value) = serde_json::to_value(&snapshot) {
        if let Some(obj) = value.as_object_mut() {
            obj.insert("kind".into(), serde_json::Value::String("snapshot".into()));
        }
        // One-off frame: SessionSnapshot as {"kind":"snapshot", ...} without
        // extending cascade-core's SessionEvent enum.
        let _ = sink.send(Message::Text(value.to_string().into())).await;
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
