//! Guest `CollabSocket`: connect, hello, backoff reconnect, fatal close codes.

use std::time::Duration;

use anyhow::{anyhow, Result};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, info, warn};

use crate::crypto::{open, seal};
use crate::link::ParsedCollabLink;
use crate::protocol::{is_fatal_close, pack_envelope, unpack_envelope, COLLAB_PROTO};

const BACKOFF_BASE_MS: u64 = 1_000;
const BACKOFF_MAX_MS: u64 = 30_000;

pub enum SocketEvent {
    Open,
    Frame { from_peer: u32, json: Value },
    Control(Value),
    Closed { reason: String, will_reconnect: bool },
}

pub struct CollabSocket {
    pub events: mpsc::UnboundedReceiver<SocketEvent>,
    pub send: mpsc::UnboundedSender<Outgoing>,
}

pub struct Outgoing {
    pub frame: Value,
    pub target_peer: u32,
}

impl CollabSocket {
    pub fn connect_guest(parsed: ParsedCollabLink, display_name: String) -> Self {
        let (ev_tx, ev_rx) = mpsc::unbounded_channel();
        let (send_tx, send_rx) = mpsc::unbounded_channel::<Outgoing>();
        tokio::spawn(run_guest(parsed, display_name, ev_tx, send_rx));
        Self {
            events: ev_rx,
            send: send_tx,
        }
    }
}

async fn run_guest(
    parsed: ParsedCollabLink,
    display_name: String,
    ev_tx: mpsc::UnboundedSender<SocketEvent>,
    mut send_rx: mpsc::UnboundedReceiver<Outgoing>,
) {
    let mut attempt: u32 = 0;
    let write_token = parsed
        .write_token
        .as_ref()
        .map(|t| URL_SAFE_NO_PAD.encode(t));

    loop {
        match connect_once(&parsed, &display_name, write_token.as_deref(), &ev_tx, &mut send_rx)
            .await
        {
            ConnectEnd::Fatal(reason) => {
                let _ = ev_tx.send(SocketEvent::Closed {
                    reason,
                    will_reconnect: false,
                });
                return;
            }
            ConnectEnd::Dropped(reason) => {
                let _ = ev_tx.send(SocketEvent::Closed {
                    reason: reason.clone(),
                    will_reconnect: true,
                });
                let base = BACKOFF_BASE_MS.saturating_mul(2u64.saturating_pow(attempt.min(16)));
                let cap = base.min(BACKOFF_MAX_MS);
                attempt = attempt.saturating_add(1);
                let jitter = (rand::random::<f64>() * 0.5 + 0.75) * cap as f64;
                tokio::time::sleep(Duration::from_millis(jitter as u64)).await;
            }
            ConnectEnd::Stopped => return,
        }
    }
}

enum ConnectEnd {
    Fatal(String),
    Dropped(String),
    Stopped,
}

async fn connect_once(
    parsed: &ParsedCollabLink,
    display_name: &str,
    write_token: Option<&str>,
    ev_tx: &mpsc::UnboundedSender<SocketEvent>,
    send_rx: &mut mpsc::UnboundedReceiver<Outgoing>,
) -> ConnectEnd {
    let url = format!("{}?role=guest", parsed.ws_url);
    info!(url = %url, "collab guest connecting");
    let req = match url.into_client_request() {
        Ok(r) => r,
        Err(e) => return ConnectEnd::Fatal(e.to_string()),
    };
    let (ws, _) = match tokio_tungstenite::connect_async(req).await {
        Ok(p) => p,
        Err(e) => return ConnectEnd::Dropped(e.to_string()),
    };
    let (mut sink, mut stream) = ws.split();
    let _ = ev_tx.send(SocketEvent::Open);

    let hello = {
        let mut h = json!({
            "t": "hello",
            "proto": COLLAB_PROTO,
            "name": display_name,
        });
        if let Some(tok) = write_token {
            h["writeToken"] = json!(tok);
        }
        h
    };
    match seal_send(&parsed.key, hello, 0) {
        Ok(env) => {
            if sink.send(Message::Binary(env.into())).await.is_err() {
                return ConnectEnd::Dropped("send hello failed".into());
            }
        }
        Err(e) => return ConnectEnd::Fatal(e.to_string()),
    }

    loop {
        tokio::select! {
            outgoing = send_rx.recv() => {
                let Some(out) = outgoing else {
                    return ConnectEnd::Stopped;
                };
                match seal_send(&parsed.key, out.frame, out.target_peer) {
                    Ok(env) => {
                        if sink.send(Message::Binary(env.into())).await.is_err() {
                            return ConnectEnd::Dropped("send failed".into());
                        }
                    }
                    Err(e) => {
                        warn!("seal failed: {e}");
                    }
                }
            }
            incoming = stream.next() => {
                match incoming {
                    None => return ConnectEnd::Dropped("connection lost".into()),
                    Some(Err(e)) => return ConnectEnd::Dropped(e.to_string()),
                    Some(Ok(Message::Text(t))) => {
                        if let Ok(v) = serde_json::from_str::<Value>(&t) {
                            let _ = ev_tx.send(SocketEvent::Control(v));
                        }
                    }
                    Some(Ok(Message::Binary(b))) => {
                        let Some((from_peer, payload)) = unpack_envelope(&b) else {
                            continue;
                        };
                        match open(&parsed.key, payload) {
                            Ok(pt) => {
                                match serde_json::from_slice::<Value>(&pt) {
                                    Ok(json) => {
                                        let _ = ev_tx.send(SocketEvent::Frame { from_peer, json });
                                    }
                                    Err(e) => debug!("collab frame json: {e}"),
                                }
                            }
                            Err(_) => {
                                return ConnectEnd::Fatal("bad key or corrupted frame".into());
                            }
                        }
                    }
                    Some(Ok(Message::Ping(p))) => {
                        let _ = sink.send(Message::Pong(p)).await;
                    }
                    Some(Ok(Message::Pong(_))) | Some(Ok(Message::Frame(_))) => {}
                    Some(Ok(Message::Close(frame))) => {
                        let code = frame
                            .as_ref()
                            .map(|f| u16::from(f.code))
                            .unwrap_or(1000);
                        let reason = frame
                            .as_ref()
                            .map(|f| f.reason.to_string())
                            .unwrap_or_default();
                        if is_fatal_close(code) {
                            let why = crate::protocol::fatal_reason(code)
                                .unwrap_or("fatal close")
                                .to_string();
                            return ConnectEnd::Fatal(why);
                        }
                        let _ = CloseCode::from(code);
                        return ConnectEnd::Dropped(if reason.is_empty() {
                            format!("connection lost (code {code})")
                        } else {
                            reason
                        });
                    }
                }
            }
        }
    }
}

fn seal_send(key: &[u8], frame: Value, target_peer: u32) -> Result<Vec<u8>> {
    let pt = serde_json::to_vec(&frame).map_err(|e| anyhow!(e))?;
    let sealed = seal(key, &pt)?;
    Ok(pack_envelope(target_peer, &sealed))
}
