use std::path::PathBuf;
use std::time::Duration;

use cascade_core::{SessionEvent, SessionManager, SpawnOptions, UiAnswer};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, watch};
use tokio_tungstenite::tungstenite::Message;

use crate::Config;

#[derive(Debug, Deserialize)]
struct RegisterAck {
    id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RelayEnvelope {
    session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    req: Option<String>,
    payload: serde_json::Value,
}

impl RelayEnvelope {
    fn reply(req: &str, payload: serde_json::Value) -> Self {
        Self { session_id: None, req: Some(req.to_string()), payload }
    }
    fn event(session_id: &str, payload: serde_json::Value) -> Self {
        Self { session_id: Some(session_id.to_string()), req: None, payload }
    }
    fn text(&self) -> Option<Message> {
        serde_json::to_string(self).ok().map(|s| Message::Text(s.into()))
    }
}

/// Desktop→cloud announcements (payloads of outbound envelopes).
#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum DesktopAnnounce {
    /// A transcript file appeared/changed on disk — list it under this machine.
    SessionDiscovered { id: String, cwd: String, title: Option<String> },
    /// The file vanished.
    SessionGone { id: String },
}

/// Commands the cloud may send over `/relay`.
#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum RelayPayload {
    Prompt { message: String },
    Abort,
    AnswerUi { request_id: String, response: UiAnswer },
    SetModel { provider: String, model_id: String },
    SetThinking { level: String },
    GetState,
    Spawn { cwd: String, model: Option<String> },
    Shutdown,
    /// Late-joiner transcript replay: emit a Snapshot event for the session.
    GetSnapshot,
}

/// Outbound side of the relay connection, shared by every task on this socket.
type Out = mpsc::UnboundedSender<Message>;

/// session_id → transcript path for discovered (file-backed) sessions.
type DiscoveredMap = std::sync::Arc<tokio::sync::Mutex<std::collections::HashMap<String, PathBuf>>>;
pub async fn run_desktop(cfg: Config, mut shutdown: watch::Receiver<bool>) -> anyhow::Result<()> {
    // The outbound WSS needs a rustls CryptoProvider; install once, idempotently.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let registry = cascade_core::SessionRegistry::open(&cfg.db)?;
    let sessions = SessionManager::new(registry);

    // Process-independent session discovery: watch the omp session store.
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    let store = PathBuf::from(home).join(".omp/agent/sessions");
    let discovered: DiscoveredMap = Default::default();
    let watcher = cascade_core::watch::SessionWatcher::new(vec![store]);

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
        match connect_and_serve(
            &relay_url,
            &name,
            &token,
            sessions.clone(),
            &mut shutdown,
            watcher.clone(),
            &discovered,
        )
        .await
        {
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
    watcher: cascade_core::watch::SessionWatcher,
    discovered: &DiscoveredMap,
) -> anyhow::Result<()> {
    let (ws, _) = tokio_tungstenite::connect_async(url).await?;
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

    // All desktop→cloud frames funnel through this channel so spawned event
    // pumps can share the single relay socket.
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Message>();

    // Announce the session store contents, then stream changes.
    {
        let found = watcher.scan_once().await;
        let mut map = discovered.lock().await;
        for d in found {
            map.insert(d.session_id.clone(), d.path.clone());
            let ann = DesktopAnnounce::SessionDiscovered {
                id: d.session_id,
                cwd: d.cwd,
                title: d.title,
            };
            if let Ok(p) = serde_json::to_value(&ann) {
                if let Some(msg) = (RelayEnvelope { session_id: None, req: None, payload: p }).text() {
                    let _ = out_tx.send(msg);
                }
            }
        }
    }
    let out_watch = out_tx.clone();
    let discovered_watch = discovered.clone();
    let watcher2 = watcher;
    tokio::spawn(async move {
        use futures_util::StreamExt as _;
        let mut stream = std::pin::pin!(watcher2.watch().await);
        while let Some(ev) = stream.next().await {
            match ev {
                cascade_core::watch::WatchEvent::Changed(d) => {
                    discovered_watch
                        .lock()
                        .await
                        .insert(d.session_id.clone(), d.path.clone());
                    let ann = DesktopAnnounce::SessionDiscovered {
                        id: d.session_id,
                        cwd: d.cwd,
                        title: d.title,
                    };
                    if let Ok(p) = serde_json::to_value(&ann) {
                        if let Some(msg) = (RelayEnvelope { session_id: None, req: None, payload: p }).text() {
                            if out_watch.send(msg).is_err() {
                                return;
                            }
                        }
                    }
                }
                cascade_core::watch::WatchEvent::Removed(path) => {
                    let mut map = discovered_watch.lock().await;
                    let gone: Vec<String> = map
                        .iter()
                        .filter(|(_, p)| **p == path)
                        .map(|(id, _)| id.clone())
                        .collect();
                    for id in gone {
                        map.remove(&id);
                        let ann = DesktopAnnounce::SessionGone { id };
                        if let Ok(p) = serde_json::to_value(&ann) {
                            if let Some(msg) = (RelayEnvelope { session_id: None, req: None, payload: p }).text() {
                                let _ = out_watch.send(msg);
                            }
                        }
                    }
                }
            }
        }
    });

    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    let _ = sink.send(Message::Close(None)).await;
                    break;
                }
            }
            out = out_rx.recv() => {
                match out {
                    Some(msg) => {
                        if sink.send(msg).await.is_err() {
                            break;
                        }
                    }
                    None => break,
                }
            }
            frame = stream.next() => {
                match frame {
                    Some(Ok(Message::Text(text))) => {
                        dispatch_envelope(&sessions, &text, &out_tx, discovered).await;
                    }
                    Some(Ok(Message::Binary(b))) => {
                        if let Ok(text) = String::from_utf8(b.to_vec()) {
                            dispatch_envelope(&sessions, &text, &out_tx, discovered).await;
                        }
                    }
                    Some(Ok(Message::Ping(p))) => {
                        let _ = out_tx.send(Message::Pong(p));
                    }
                    Some(Ok(Message::Pong(_))) | Some(Ok(Message::Frame(_))) => {}
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Err(e)) => return Err(e.into()),
                }
            }
        }
    }
    Ok(())
}

/// Handle one cloud→desktop envelope. Replies and event frames flow back over
/// the shared `out` channel.
async fn dispatch_envelope(
    sessions: &SessionManager,
    text: &str,
    out: &Out,
    discovered: &DiscoveredMap,
) {
    let env: RelayEnvelope = match serde_json::from_str(text) {
        Ok(e) => e,
        Err(_) => {
            tracing::warn!(payload = text, "unrecognized relay frame");
            return;
        }
    };

    let payload = match serde_json::from_value::<RelayPayload>(env.payload.clone()) {
        Ok(p) => p,
        Err(_) => {
            tracing::warn!(payload = %env.payload, "unparseable relay payload");
            return;
        }
    };

    // Session-scoped commands need an id; spawn is machine-scoped.
    let session = match (&env.session_id, &payload) {
        (Some(id), _) => sessions.get(id).await,
        (None, RelayPayload::Spawn { .. }) => None,
        (None, _) => {
            tracing::warn!("relay command missing session_id");
            return;
        }
    };

    match payload {
        RelayPayload::Spawn { cwd, model } => {
            let opts = SpawnOptions {
                cwd: PathBuf::from(&cwd),
                model,
                ..SpawnOptions::default()
            };
            let reply = match sessions.spawn(opts).await {
                Ok(id) => {
                    tracing::info!(session_id = %id, %cwd, "relay spawn");
                    // Stream every session event back to the cloud for the
                    // session's lifetime; the cloud fans out to attached clients.
                    let sessions2 = sessions.clone();
                    let out2 = out.clone();
                    let id2 = id.clone();
                    let cwd2 = cwd.clone();
                    tokio::spawn(async move {
                        stream_session_events(sessions2, &id2, out2).await;
                    });
                    serde_json::json!({ "kind": "spawn_ok", "id": id, "cwd": cwd2 })
                }
                Err(e) => {
                    tracing::error!(%e, "relay spawn failed");
                    serde_json::json!({ "kind": "spawn_err", "message": e.to_string() })
                }
            };
            if let Some(req) = env.req.as_deref() {
                if let Some(msg) = RelayEnvelope::reply(req, reply).text() {
                    let _ = out.send(msg);
                }
            }
        }
        RelayPayload::Shutdown => {
            let id = env.session_id.clone().unwrap_or_default();
            if let Err(e) = sessions.shutdown(&id).await {
                tracing::warn!(%e, session_id = %id, "relay shutdown failed");
            }
        }
        RelayPayload::GetSnapshot => {
            let Some(id) = env.session_id.clone() else { return };
            if let Some(session) = session {
                let snap = session.snapshot().await;
                let payload = serde_json::to_value(&SessionEvent::Snapshot(snap))
                    .unwrap_or(serde_json::Value::Null);
                if let Some(msg) = RelayEnvelope::event(&id, payload).text() {
                    let _ = out.send(msg);
                }
                return;
            }
            // Discovered (file-backed) session: replay the transcript, then
            // live-tail appended entries as events. Read-only by definition.
            let path = discovered.lock().await.get(&id).cloned();
            let Some(path) = path else { return };
            match cascade_core::replay::parse_snapshot(&path) {
                Ok(snap) => {
                    let payload = serde_json::to_value(&SessionEvent::Snapshot(snap))
                        .unwrap_or(serde_json::Value::Null);
                    if let Some(msg) = RelayEnvelope::event(&id, payload).text() {
                        let _ = out.send(msg);
                    }
                    let out2 = out.clone();
                    tokio::spawn(async move {
                        tail_discovered(&id, path, out2).await;
                    });
                }
                Err(e) => tracing::warn!(%e, session_id = %id, "replay failed"),
            }
        }
        RelayPayload::GetState => {
            let Some(session) = session else { return };
            let id = env.session_id.clone().unwrap_or_default();
            let state = session.get_state().await;
            let models = session.available_models().await.unwrap_or_default();
            match state {
                Ok(st) => {
                    // Broadcast-style (no req): every attached client sees state.
                    let mut payload = serde_json::to_value(&SessionEvent::StateChanged)
                        .unwrap_or(serde_json::Value::Null);
                    if let Some(map) = payload.as_object_mut() {
                        map.insert("state".into(), serde_json::to_value(&st).unwrap_or(serde_json::Value::Null));
                        map.insert("models".into(), serde_json::to_value(&models).unwrap_or(serde_json::Value::Null));
                    }
                    if let Some(msg) = RelayEnvelope::event(&id, payload).text() {
                        let _ = out.send(msg);
                    }
                }
                Err(e) => tracing::warn!(%e, session_id = %id, "get_state failed"),
            }
        }
        RelayPayload::Prompt { message } => {
            if let Some(s) = session {
                if let Err(e) = s.prompt(message).await {
                    tracing::warn!(%e, "relay prompt failed");
                }
            }
        }
        RelayPayload::Abort => {
            if let Some(s) = session {
                if let Err(e) = s.abort().await {
                    tracing::warn!(%e, "relay abort failed");
                }
            }
        }
        RelayPayload::AnswerUi { request_id, response } => {
            if let Some(s) = session {
                if let Err(e) = s.answer_ui(request_id, response).await {
                    tracing::warn!(%e, "relay answer_ui failed");
                }
            }
        }
        RelayPayload::SetModel { provider, model_id } => {
            if let Some(s) = session {
                if let Err(e) = s.set_model(provider, model_id).await {
                    tracing::warn!(%e, "relay set_model failed");
                }
            }
        }
        RelayPayload::SetThinking { level } => {
            if let Some(s) = session {
                if let Err(e) = s.set_thinking_level(level).await {
                    tracing::warn!(%e, "relay set_thinking failed");
                }
            }
        }
    }
}

/// Live-tail a discovered transcript: poll for appended entries and forward
/// them as session events. Stops when the relay socket drops or the file is
/// deleted.
async fn tail_discovered(id: &str, path: PathBuf, out: Out) {
    let mut tailer = cascade_core::replay::FileTailer::new(path.clone());
    loop {
        tokio::time::sleep(Duration::from_millis(700)).await;
        if !path.exists() {
            break;
        }
        match tailer.next_events().await {
            Ok(events) if !events.is_empty() => {
                for ev in events {
                    let payload =
                        serde_json::to_value(&ev).unwrap_or(serde_json::Value::Null);
                    if let Some(msg) = RelayEnvelope::event(id, payload).text() {
                        if out.send(msg).is_err() {
                            return;
                        }
                    }
                }
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
}

/// Subscribe to a session's event stream and forward each event to the cloud
/// as a session-scoped envelope. Ends when the session exits or the relay
/// connection (shared `out`) drops.
async fn stream_session_events(sessions: SessionManager, id: &str, out: Out) {
    let Some(session) = sessions.get(id).await else { return };
    let mut rx = session.subscribe();
    loop {
        match rx.recv().await {
            Ok(ev) => {
                let payload = serde_json::to_value(&ev).unwrap_or(serde_json::Value::Null);
                if let Some(msg) = RelayEnvelope::event(id, payload).text() {
                    if out.send(msg).is_err() {
                        break; // relay socket gone
                    }
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                tracing::warn!(session_id = %id, skipped = n, "event fan-out lagged");
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
        }
    }
}
