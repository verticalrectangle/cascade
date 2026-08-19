use std::path::PathBuf;
use std::time::Duration;

use cascade_core::{CloudCommand, SessionManager, SpawnOptions, UiAnswer};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::sync::watch;
use tokio_tungstenite::{connect_async, tungstenite::Message};

use crate::Config;

#[derive(Debug, Deserialize)]
struct RegisterAck {
    id: String,
}

#[derive(Debug, Deserialize)]
struct RelayEnvelope {
    session_id: Option<String>,
    payload: serde_json::Value,
}

/// Commands the cloud may send over `/relay`. CloudCommand variants plus spawn/shutdown.
#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum RelayPayload {
    Prompt { message: String },
    Abort,
    AnswerUi { request_id: String, response: UiAnswer },
    Spawn { cwd: String, model: Option<String> },
    Shutdown,
}

pub async fn run_desktop(cfg: Config, mut shutdown: watch::Receiver<bool>) -> anyhow::Result<()> {
    let registry = cascade_core::SessionRegistry::open(&cfg.db)?;
    let sessions = SessionManager::new(registry);

    let cloud_url = cfg
        .cloud_url
        .clone()
        .ok_or_else(|| anyhow::anyhow!("CASCADE_CLOUD_URL is required for desktop role"))?;
    let name = cfg
        .machine_name
        .clone()
        .ok_or_else(|| anyhow::anyhow!("CASCADE_MACHINE_NAME is required for desktop role"))?;
    let token = cfg
        .machine_token
        .clone()
        .ok_or_else(|| anyhow::anyhow!("CASCADE_MACHINE_TOKEN is required for desktop role"))?;
    let relay_url = relay_url(&cloud_url);

    let mut delay = Duration::from_secs(1);
    loop {
        if *shutdown.borrow() {
            break;
        }
        tracing::info!(%relay_url, "connecting to cloud /relay");
        match connect_and_serve(&relay_url, &name, &token, sessions.clone(), &mut shutdown).await {
            Ok(()) => {
                tracing::info!("relay connection closed");
                delay = Duration::from_secs(1);
            }
            Err(e) => {
                tracing::warn!(%e, "relay connection failed");
            }
        }
        if *shutdown.borrow() {
            break;
        }
        tokio::select! {
            _ = tokio::time::sleep(delay) => {}
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    break;
                }
            }
        }
        delay = (delay * 2).min(Duration::from_secs(30));
    }

    sessions.shutdown_all().await;
    Ok(())
}

fn relay_url(cloud: &str) -> String {
    let u = cloud.trim_end_matches('/');
    if let Some(rest) = u.strip_prefix("https://") {
        format!("wss://{rest}/relay")
    } else if let Some(rest) = u.strip_prefix("http://") {
        format!("ws://{rest}/relay")
    } else if u.starts_with("wss://") || u.starts_with("ws://") {
        format!("{u}/relay")
    } else {
        format!("wss://{u}/relay")
    }
}

async fn connect_and_serve(
    url: &str,
    name: &str,
    token: &str,
    sessions: SessionManager,
    shutdown: &mut watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let (ws, _) = connect_async(url).await?;
    let (mut sink, mut stream) = ws.split();

    let reg = serde_json::json!({ "name": name, "token": token });
    sink.send(Message::Text(reg.to_string().into())).await?;

    let ack_raw = match stream.next().await {
        Some(Ok(Message::Text(t))) => t.to_string(),
        Some(Ok(Message::Binary(b))) => String::from_utf8_lossy(&b).into_owned(),
        Some(Ok(other)) => anyhow::bail!("unexpected ack frame: {other:?}"),
        Some(Err(e)) => return Err(e.into()),
        None => anyhow::bail!("relay closed before registration ack"),
    };
    let ack: RegisterAck = serde_json::from_str(&ack_raw)?;
    tracing::info!(machine_id = %ack.id, "registered with cloud");

    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    let _ = sink.send(Message::Close(None)).await;
                    break;
                }
            }
            frame = stream.next() => {
                match frame {
                    Some(Ok(Message::Text(text))) => {
                        dispatch_envelope(&sessions, &text).await;
                    }
                    Some(Ok(Message::Binary(b))) => {
                        if let Ok(text) = String::from_utf8(b.to_vec()) {
                            dispatch_envelope(&sessions, &text).await;
                        }
                    }
                    Some(Ok(Message::Ping(p))) => {
                        sink.send(Message::Pong(p)).await?;
                    }
                    Some(Ok(Message::Pong(_))) => {}
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(Message::Frame(_))) => {}
                    Some(Err(e)) => return Err(e.into()),
                }
            }
        }
    }
    Ok(())
}

async fn dispatch_envelope(sessions: &SessionManager, text: &str) {
    let env: RelayEnvelope = match serde_json::from_str(text) {
        Ok(e) => e,
        Err(_) => {
            // Bare CloudCommand without envelope — ignore (needs session_id).
            if serde_json::from_str::<CloudCommand>(text).is_ok() {
                tracing::warn!("relay frame missing session_id envelope");
            } else {
                tracing::warn!(payload = text, "unrecognized relay frame");
            }
            return;
        }
    };

    if let Ok(payload) = serde_json::from_value::<RelayPayload>(env.payload.clone()) {
        match payload {
            RelayPayload::Spawn { cwd, model } => {
                let opts = SpawnOptions {
                    cwd: PathBuf::from(cwd),
                    model,
                    ..SpawnOptions::default()
                };
                match sessions.spawn(opts).await {
                    Ok(id) => tracing::info!(session_id = %id, "relay spawn"),
                    Err(e) => tracing::error!(%e, "relay spawn failed"),
                }
                return;
            }
            RelayPayload::Shutdown => {
                if let Some(id) = env.session_id.as_deref() {
                    if let Err(e) = sessions.shutdown(id).await {
                        tracing::warn!(%e, session_id = id, "relay shutdown failed");
                    }
                }
                return;
            }
            other => {
                let Some(id) = env.session_id.as_deref() else {
                    tracing::warn!("relay command missing session_id");
                    return;
                };
                let Some(session) = sessions.get(id).await else {
                    tracing::warn!(session_id = id, "unknown session");
                    return;
                };
                let result = match other {
                    RelayPayload::Prompt { message } => session.prompt(message).await,
                    RelayPayload::Abort => session.abort().await,
                    RelayPayload::AnswerUi { request_id, response } => {
                        session.answer_ui(request_id, response).await
                    }
                    RelayPayload::Spawn { .. } | RelayPayload::Shutdown => unreachable!(),
                };
                if let Err(e) = result {
                    tracing::warn!(%e, session_id = id, "relay command failed");
                }
                return;
            }
        }
    }

    if let Ok(cmd) = serde_json::from_value::<CloudCommand>(env.payload) {
        let Some(id) = env.session_id.as_deref() else {
            tracing::warn!("relay CloudCommand missing session_id");
            return;
        };
        let Some(session) = sessions.get(id).await else {
            tracing::warn!(session_id = id, "unknown session");
            return;
        };
        let result = match cmd {
            CloudCommand::Prompt { message } => session.prompt(message).await,
            CloudCommand::Abort => session.abort().await,
            CloudCommand::AnswerUi { request_id, response } => {
                session.answer_ui(request_id, response).await
            }
        };
        if let Err(e) = result {
            tracing::warn!(%e, session_id = id, "relay command failed");
        }
    }
}
