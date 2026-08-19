//! Axum `/r/<roomId>` WebSocket relay. Never decrypts envelopes.

use std::net::SocketAddr;
use std::path::PathBuf;

use axum::extract::ws::{CloseFrame, Message, Utf8Bytes, WebSocket};
use axum::extract::{Path, Query, State, WebSocketUpgrade};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::protocol::{
    valid_room_id, CLOSE_HOST_CONFLICT, CLOSE_NO_SUCH_ROOM, CLOSE_ROOM_FULL,
};
use crate::rooms::{Hub, JoinErr, PeerMsg, RoomRecord};

#[derive(Clone)]
pub struct RelayConfig {
    pub bind: SocketAddr,
    pub data_dir: Option<PathBuf>,
    pub max_guests: usize,
    pub public_url: String,
    pub tls_cert: Option<PathBuf>,
    pub tls_key: Option<PathBuf>,
}

impl RelayConfig {
    pub fn from_env() -> anyhow::Result<Self> {
        let bind: SocketAddr = std::env::var("CASCADE_RELAY_BIND")
            .unwrap_or_else(|_| "127.0.0.1:8788".into())
            .parse()?;
        let data_dir = std::env::var("CASCADE_RELAY_DATA_DIR")
            .ok()
            .filter(|s| !s.is_empty())
            .map(PathBuf::from);
        let max_guests = std::env::var("CASCADE_RELAY_MAX_GUESTS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(64);
        let public_url = std::env::var("CASCADE_RELAY_PUBLIC_URL").unwrap_or_else(|_| {
            let scheme = if std::env::var("CASCADE_RELAY_TLS_CERT").is_ok() {
                "wss"
            } else {
                "ws"
            };
            format!("{scheme}://{bind}")
        });
        let tls_cert = std::env::var("CASCADE_RELAY_TLS_CERT").ok().map(PathBuf::from);
        let tls_key = std::env::var("CASCADE_RELAY_TLS_KEY").ok().map(PathBuf::from);
        Ok(Self {
            bind,
            data_dir,
            max_guests,
            public_url,
            tls_cert,
            tls_key,
        })
    }
}

#[derive(Clone)]
struct AppState {
    hub: Hub,
}

#[derive(Debug, Deserialize)]
struct RoleQuery {
    role: Option<String>,
}

pub async fn serve(cfg: RelayConfig) -> anyhow::Result<()> {
    let hub = Hub::new(
        cfg.data_dir.clone(),
        cfg.max_guests,
        cfg.public_url.clone(),
    );
    let app = Router::new()
        .route("/r/{room_id}", get(ws_handler))
        .route("/healthz", get(|| async { "ok" }))
        .with_state(AppState { hub });

    info!(
        bind = %cfg.bind,
        public_url = %cfg.public_url,
        max_guests = cfg.max_guests,
        "cascade-relay listening"
    );

    match (cfg.tls_cert, cfg.tls_key) {
        (Some(cert), Some(key)) => {
            let tls = axum_server::tls_rustls::RustlsConfig::from_pem_file(&cert, &key).await?;
            axum_server::bind_rustls(cfg.bind, tls)
                .serve(app.into_make_service())
                .await?;
        }
        _ => {
            let listener = tokio::net::TcpListener::bind(cfg.bind).await?;
            axum::serve(listener, app).await?;
        }
    }
    Ok(())
}

async fn ws_handler(
    State(st): State<AppState>,
    Path(room_id): Path<String>,
    Query(q): Query<RoleQuery>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    if !valid_room_id(&room_id) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let role = q.role.unwrap_or_default();
    if role != "host" && role != "guest" {
        // CollabSocket always sets `?role=`; refuse ambiguous upgrades.
        return StatusCode::BAD_REQUEST.into_response();
    }
    let is_host = role == "host";
    ws.on_upgrade(move |socket| peer_session(st.hub, room_id, is_host, socket))
        .into_response()
}

async fn peer_session(hub: Hub, room_id: String, is_host: bool, socket: WebSocket) {
    let (mut sink, mut stream) = socket.split();
    let (tx, mut rx) = mpsc::unbounded_channel::<PeerMsg>();

    let joined = if is_host {
        match hub.join_host(room_id.clone(), tx.clone()).await {
            Ok(j) => Ok((j.room_id, None)),
            Err(e) => Err(e),
        }
    } else {
        match hub.join_guest(&room_id, tx.clone()).await {
            Ok(j) => Ok((j.room_id, Some(j.peer_id))),
            Err(e) => Err(e),
        }
    };

    let (room_id, guest_id) = match joined {
        Ok(v) => v,
        Err(e) => {
            let (code, reason) = match e {
                JoinErr::HostConflict => (
                    CLOSE_HOST_CONFLICT,
                    "a host is already connected for this room",
                ),
                JoinErr::NoSuchRoom | JoinErr::BadRoomId => (CLOSE_NO_SUCH_ROOM, "no such room"),
                JoinErr::RoomFull => (CLOSE_ROOM_FULL, "room is full"),
            };
            let _ = sink
                .send(close_msg(code, reason))
                .await;
            return;
        }
    };

    let writer = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            let send = match msg {
                PeerMsg::Binary(b) => sink.send(Message::Binary(b.into())).await,
                PeerMsg::Text(t) => sink.send(Message::Text(t.into())).await,
                PeerMsg::Close { code, reason } => {
                    let _ = sink.send(close_msg(code, &reason)).await;
                    break;
                }
            };
            if send.is_err() {
                break;
            }
        }
    });

    while let Some(Ok(msg)) = stream.next().await {
        match msg {
            Message::Binary(bytes) => {
                let buf = bytes.to_vec();
                if guest_id.is_none() {
                    hub.route_from_host(&room_id, buf).await;
                } else if let Some(pid) = guest_id {
                    hub.route_from_guest(&room_id, pid, buf).await;
                }
            }
            Message::Text(text) => {
                // Optional host metadata (not in the omp client; see README).
                if guest_id.is_none() {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                        if v.get("t").and_then(|t| t.as_str()) == Some("room-meta") {
                            let patch = RoomRecord {
                                room_id: room_id.clone(),
                                relay_url: v
                                    .get("relayUrl")
                                    .and_then(|x| x.as_str())
                                    .map(str::to_string),
                                link: v.get("link").and_then(|x| x.as_str()).map(str::to_string),
                                view_link: v
                                    .get("viewLink")
                                    .and_then(|x| x.as_str())
                                    .map(str::to_string),
                                token: v.get("token").and_then(|x| x.as_str()).map(str::to_string),
                            };
                            hub.update_meta(&room_id, patch).await;
                        }
                    }
                }
            }
            Message::Ping(_) | Message::Pong(_) => {}
            Message::Close(_) => break,
        }
    }

    if let Some(pid) = guest_id {
        hub.guest_leave(&room_id, pid).await;
    } else {
        hub.host_leave(&room_id).await;
    }
    writer.abort();
    let _ = writer.await;
    warn!(room_id = %room_id, is_host, "peer socket ended");
}

fn close_msg(code: u16, reason: &str) -> Message {
    Message::Close(Some(CloseFrame {
        code: code.into(),
        reason: Utf8Bytes::from(reason.to_string()),
    }))
}
