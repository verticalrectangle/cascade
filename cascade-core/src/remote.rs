use anyhow::{anyhow, Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::header::AUTHORIZATION;
use tokio_tungstenite::tungstenite::Message;

use crate::registry::SessionMeta;
use crate::session::{SessionEvent, UiAnswer};

/// Client for the cloud API (used by cascade-gtk).
#[derive(Clone)]
pub struct CloudClient {
    base_url: String,
    token: String,
    http: reqwest::Client,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CloudCommand {
    Prompt { message: String },
    Abort,
    AnswerUi { request_id: String, response: UiAnswer },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MachineInfo {
    pub id: String,
    pub name: String,
    pub online: bool,
    pub is_cloud: bool,
}

impl CloudClient {
    pub async fn login(base_url: &str, email: &str, password: &str) -> Result<String> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let http = reqwest::Client::new();
        let url = join_url(base_url, "/auth/login");
        let resp = http
            .post(&url)
            .json(&serde_json::json!({ "email": email, "password": password }))
            .send()
            .await
            .context("POST /auth/login")?;
        let status = resp.status();
        let body: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);
        if !status.is_success() {
            anyhow::bail!("login failed ({status}): {body}");
        }
        body.get("token")
            .and_then(|t| t.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| anyhow!("login response missing token: {body}"))
    }

    pub async fn connect(base_url: &str, token: &str) -> Result<Self> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::AUTHORIZATION,
            format!("Bearer {token}")
                .parse()
                .context("invalid bearer token header")?,
        );
        let http = reqwest::Client::builder()
            .default_headers(headers)
            .build()?;
        Ok(Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            token: token.to_string(),
            http,
        })
    }

    pub async fn list_machines(&self) -> Result<Vec<MachineInfo>> {
        let url = join_url(&self.base_url, "/machines");
        let resp = self.http.get(&url).send().await.context("GET /machines")?;
        let status = resp.status();
        let body: serde_json::Value = resp.json().await.context("parse /machines")?;
        if !status.is_success() {
            anyhow::bail!("list_machines failed ({status}): {body}");
        }
        let arr = if let Some(a) = body.as_array() {
            a.clone()
        } else if let Some(a) = body.get("machines").and_then(|v| v.as_array()) {
            a.clone()
        } else {
            anyhow::bail!("unexpected /machines body: {body}");
        };
        let mut out = Vec::new();
        for v in arr {
            out.push(serde_json::from_value(v)?);
        }
        Ok(out)
    }

    pub async fn list_sessions(&self) -> Result<Vec<SessionMeta>> {
        let url = join_url(&self.base_url, "/sessions");
        let resp = self.http.get(&url).send().await.context("GET /sessions")?;
        let status = resp.status();
        let body: serde_json::Value = resp.json().await.context("parse /sessions")?;
        if !status.is_success() {
            anyhow::bail!("list_sessions failed ({status}): {body}");
        }
        let arr = if let Some(a) = body.as_array() {
            a.clone()
        } else if let Some(a) = body.get("sessions").and_then(|v| v.as_array()) {
            a.clone()
        } else {
            anyhow::bail!("unexpected /sessions body: {body}");
        };
        let mut out = Vec::new();
        for v in arr {
            out.push(serde_json::from_value(v)?);
        }
        Ok(out)
    }

    pub async fn create_session(
        &self,
        machine: Option<&str>,
        cwd: &str,
        model: Option<String>,
    ) -> Result<String> {
        let url = join_url(&self.base_url, "/sessions");
        let mut body = serde_json::json!({ "cwd": cwd });
        if let Some(m) = machine {
            body["machine"] = serde_json::json!(m);
        }
        if let Some(model) = model {
            body["model"] = serde_json::json!(model);
        }
        let resp = self
            .http
            .post(&url)
            .json(&body)
            .send()
            .await
            .context("POST /sessions")?;
        let status = resp.status();
        let v: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);
        if !status.is_success() {
            anyhow::bail!("create_session failed ({status}): {v}");
        }
        if let Some(id) = v.as_str() {
            return Ok(id.to_string());
        }
        v.get("id")
            .and_then(|x| x.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| anyhow!("create_session missing id: {v}"))
    }

    pub async fn delete_session(&self, id: &str) -> Result<()> {
        let url = join_url(&self.base_url, &format!("/sessions/{id}"));
        let resp = self
            .http
            .delete(&url)
            .send()
            .await
            .context("DELETE /sessions/:id")?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("delete_session failed ({status}): {body}");
        }
        Ok(())
    }

    pub async fn attach(
        &self,
        session_id: &str,
    ) -> Result<(
        mpsc::UnboundedReceiver<SessionEvent>,
        mpsc::UnboundedSender<CloudCommand>,
    )> {
        let ws_url = http_to_ws(&self.base_url, session_id);
        let mut req = ws_url
            .as_str()
            .into_client_request()
            .context("websocket request")?;
        let bearer = format!("Bearer {}", self.token);
        req.headers_mut().insert(
            AUTHORIZATION,
            bearer.parse().context("Authorization header")?,
        );
        let (ws, _) = tokio_tungstenite::connect_async(req)
            .await
            .with_context(|| format!("connect {ws_url}"))?;
        let (mut sink, mut stream) = ws.split();
        let (ev_tx, ev_rx) = mpsc::unbounded_channel();
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<CloudCommand>();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    incoming = stream.next() => {
                        match incoming {
                            Some(Ok(Message::Text(t))) => {
                                match serde_json::from_str::<SessionEvent>(t.as_str()) {
                                    Ok(ev) => {
                                        if ev_tx.send(ev).is_err() {
                                            break;
                                        }
                                    }
                                    Err(e) => {
                                        tracing::warn!(error = %e, "invalid SessionEvent on attach stream");
                                    }
                                }
                            }
                            Some(Ok(Message::Binary(b))) => {
                                if let Ok(s) = std::str::from_utf8(&b) {
                                    if let Ok(ev) = serde_json::from_str::<SessionEvent>(s) {
                                        if ev_tx.send(ev).is_err() {
                                            break;
                                        }
                                    }
                                }
                            }
                            Some(Ok(Message::Ping(p))) => {
                                let _ = sink.send(Message::Pong(p)).await;
                            }
                            Some(Ok(Message::Close(_))) | None => break,
                            Some(Ok(_)) => {}
                            Some(Err(e)) => {
                                tracing::warn!(error = %e, "attach websocket error");
                                break;
                            }
                        }
                    }
                    cmd = cmd_rx.recv() => {
                        match cmd {
                            Some(c) => {
                                match serde_json::to_string(&c) {
                                    Ok(s) => {
                                        if sink.send(Message::text(s)).await.is_err() {
                                            break;
                                        }
                                    }
                                    Err(e) => tracing::warn!(error = %e, "serialize CloudCommand"),
                                }
                            }
                            None => break,
                        }
                    }
                }
            }
        });
        Ok((ev_rx, cmd_tx))
    }
}

fn join_url(base: &str, path: &str) -> String {
    format!("{}{}", base.trim_end_matches('/'), path)
}

fn http_to_ws(base: &str, session_id: &str) -> String {
    let base = base.trim_end_matches('/');
    let ws = if let Some(rest) = base.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = base.strip_prefix("http://") {
        format!("ws://{rest}")
    } else if base.starts_with("wss://") || base.starts_with("ws://") {
        base.to_string()
    } else {
        format!("wss://{base}")
    };
    format!("{ws}/sessions/{session_id}/stream")
}
