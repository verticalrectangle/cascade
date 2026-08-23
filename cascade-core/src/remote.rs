use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Utc};
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
    /// Switch the session's model. `model_id` excludes the provider.
    SetModel { provider: String, model_id: String },
    /// Switch the thinking level (e.g. "off" | "minimal" | "low" | "medium" | "high").
    SetThinking { level: String },
    /// Ask the daemon to re-emit session state (model/thinking/etc.) as a
    /// `state_changed` event on this stream.
    GetState,
    /// Request a transcript page. `limit` bounds the page size; `before` is
    /// the exclusive absolute upper index (`None` = tail page). The daemon
    /// answers with a `snapshot` event carrying `oldest_index`/`has_more`.
    GetSnapshot {
        #[serde(default)]
        limit: Option<u32>,
        #[serde(default)]
        before: Option<u64>,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MachineInfo {
    pub id: String,
    pub name: String,
    pub online: bool,
    pub is_cloud: bool,
}

/// Cloud `GET /sessions` row. Matches cascaded `ListedSession` JSON, including
/// optional process flags that are not stored on registry `SessionMeta`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ListedSession {
    pub id: String,
    pub omp_session_id: Option<String>,
    pub name: Option<String>,
    pub cwd: String,
    pub session_file: Option<String>,
    pub machine: String,
    pub created_at: DateTime<Utc>,
    pub last_active: DateTime<Utc>,
    #[serde(default)]
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub join_handle: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub view_handle: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<i64>,
    /// Process exists. `None` = unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live: Option<bool>,
    /// Actively streaming. `None` = unknown; `None` with `live == true` → IDLE.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working: Option<bool>,
    /// True when the session has no content — hide from default lists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub empty: Option<bool>,
    /// "spawned" | "discovered" | "terminal".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
}

impl ListedSession {
    /// Lift a registry row into a list view, attaching process flags.
    pub fn from_meta(m: SessionMeta, live: Option<bool>, working: Option<bool>) -> Self {
        Self {
            id: m.id,
            omp_session_id: m.omp_session_id,
            name: m.name,
            cwd: m.cwd,
            session_file: m.session_file,
            machine: m.machine,
            created_at: m.created_at,
            last_active: m.last_active,
            kind: m.kind,
            join_handle: m.join_handle,
            view_handle: m.view_handle,
            pid: m.pid,
            live,
            working,
            empty: None,
            origin: None,
        }
    }
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

    pub async fn register(
        base_url: &str,
        email: &str,
        password: &str,
        invite: &str,
    ) -> Result<String> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let http = reqwest::Client::new();
        let url = join_url(base_url, "/auth/register");
        let resp = http
            .post(&url)
            .json(&serde_json::json!({
                "email": email,
                "password": password,
                "invite": invite,
            }))
            .send()
            .await
            .context("POST /auth/register")?;
        let status = resp.status();
        let body: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);
        if !status.is_success() {
            anyhow::bail!("{}", json_error_message(&body));
        }
        body.get("token")
            .and_then(|t| t.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| anyhow!("register response missing token: {body}"))
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

    pub async fn list_sessions(&self) -> Result<Vec<ListedSession>> {
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
            out.push(serde_json::from_value::<ListedSession>(v)?);
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

    /// Resolve a pasted view-share URL (`https://host/s/<token>` or a bare token)
    /// via `GET <base>/s/<token>` → `{session_id, read_only}`.
    pub async fn resolve_share(&self, url: &str) -> Result<ResolvedShare> {
        let token = parse_share_token(url)
            .ok_or_else(|| anyhow!("that doesn't look like a view link"))?;
        let req_url = join_url(&self.base_url, &format!("/s/{token}"));
        let resp = self
            .http
            .get(&req_url)
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await
            .context("GET /s/:token")?;
        let status = resp.status();
        let body: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);
        if !status.is_success() {
            anyhow::bail!("{}", json_error_message(&body));
        }
        let session_id = body
            .get("session_id")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("share lookup missing session_id: {body}"))?
            .to_string();
        let read_only = body
            .get("read_only")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        Ok(ResolvedShare {
            session_id,
            read_only,
            token,
        })
    }

    pub async fn attach(
        &self,
        session_id: &str,
    ) -> Result<(
        mpsc::UnboundedReceiver<SessionEvent>,
        mpsc::UnboundedSender<CloudCommand>,
    )> {
        self.attach_with_bearer(session_id, &self.token, None).await
    }

    /// Attach and ask the daemon for a tail-only initial snapshot of
    /// `tail` messages; older pages are fetched with
    /// [`CloudCommand::GetSnapshot`] on scroll-up.
    pub async fn attach_paged(
        &self,
        session_id: &str,
        tail: u32,
    ) -> Result<(
        mpsc::UnboundedReceiver<SessionEvent>,
        mpsc::UnboundedSender<CloudCommand>,
    )> {
        self.attach_with_bearer(session_id, &self.token, Some(tail)).await
    }

    /// Same stream as [`Self::attach`], but the share token is the Bearer.
    pub async fn attach_shared(
        &self,
        session_id: &str,
        share_token: &str,
    ) -> Result<(
        mpsc::UnboundedReceiver<SessionEvent>,
        mpsc::UnboundedSender<CloudCommand>,
    )> {
        self.attach_with_bearer(session_id, share_token, None).await
    }

    async fn attach_with_bearer(
        &self,
        session_id: &str,
        bearer_token: &str,
        tail: Option<u32>,
    ) -> Result<(
        mpsc::UnboundedReceiver<SessionEvent>,
        mpsc::UnboundedSender<CloudCommand>,
    )> {
        let mut ws_url = http_to_ws(&self.base_url, session_id);
        if let Some(t) = tail {
            ws_url = format!("{ws_url}?tail={t}");
        }
        let mut req = ws_url
            .as_str()
            .into_client_request()
            .context("websocket request")?;
        let bearer = format!("Bearer {bearer_token}");
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

    pub async fn share_session(&self, session_id: &str) -> Result<String> {
        let url = join_url(&self.base_url, &format!("/sessions/{session_id}/share"));
        let resp = self
            .http
            .post(&url)
            .send()
            .await
            .context("POST /sessions/:id/share")?;
        let status = resp.status();
        let body: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);
        if !status.is_success() {
            anyhow::bail!("{}", json_error_message(&body));
        }
        let raw = body
            .get("url")
            .and_then(|u| u.as_str())
            .ok_or_else(|| anyhow!("share response missing url: {body}"))?;
        Ok(absolute_url(&self.base_url, raw))
    }

    pub async fn unshare_session(&self, session_id: &str) -> Result<()> {
        let url = join_url(&self.base_url, &format!("/sessions/{session_id}/share"));
        let resp = self
            .http
            .delete(&url)
            .send()
            .await
            .context("DELETE /sessions/:id/share")?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&body) {
                anyhow::bail!("{}", json_error_message(&v));
            }
            anyhow::bail!("unshare failed ({status}): {body}");
        }
        Ok(())
    }
}

fn json_error_message(body: &serde_json::Value) -> String {
    body.get("error")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| body.to_string())
}

fn absolute_url(base: &str, url: &str) -> String {
    if url.starts_with("http://") || url.starts_with("https://") {
        url.to_string()
    } else if url.starts_with('/') {
        format!("{}{url}", base.trim_end_matches('/'))
    } else {
        format!("{}/{url}", base.trim_end_matches('/'))
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

/// Parsed `GET /s/<token>` response plus the token taken from the pasted URL.
#[derive(Clone, Debug)]
pub struct ResolvedShare {
    pub session_id: String,
    pub read_only: bool,
    pub token: String,
}

/// Token from a view-share URL tail (`…/s/<token>`), or a bare token string.
fn parse_share_token(url: &str) -> Option<String> {
    let s = url.trim();
    if s.is_empty() {
        return None;
    }
    let s = s.split(['?', '#']).next().unwrap_or(s).trim_end_matches('/');
    let parts: Vec<&str> = s.split('/').filter(|p| !p.is_empty()).collect();
    if let Some(i) = parts.iter().position(|p| *p == "s") {
        if let Some(&tok) = parts.get(i + 1) {
            if !tok.is_empty() {
                return Some(tok.to_string());
            }
        }
    }
    if !s.contains("://") && !s.contains('/') {
        return Some(s.to_string());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_share_token_from_url_tail() {
        assert_eq!(
            parse_share_token("https://wickrunner.com:7701/s/tok").as_deref(),
            Some("tok")
        );
        assert_eq!(
            parse_share_token("  https://host/s/abc-def_ghi/?x=1#y  ").as_deref(),
            Some("abc-def_ghi")
        );
        assert_eq!(parse_share_token("host/s/plain").as_deref(), Some("plain"));
        assert_eq!(parse_share_token("baretoken").as_deref(), Some("baretoken"));
        assert_eq!(parse_share_token("https://host/sessions/xyz"), None);
        assert_eq!(parse_share_token(""), None);
        assert_eq!(parse_share_token("   "), None);
    }
}
