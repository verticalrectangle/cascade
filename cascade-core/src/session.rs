use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::process::{Child, Command};
use tokio::sync::{broadcast, oneshot, Mutex};
use uuid::Uuid;

use crate::registry::{SessionMeta, SessionRegistry};
use crate::rpc::{self, RpcClient};

const BROADCAST_CAP: usize = 1024;

/// Spawn configuration for one omp session.
#[derive(Clone, Debug)]
pub struct SpawnOptions {
    pub cwd: PathBuf,
    pub omp_bin: PathBuf,
    pub model: Option<String>,
    pub resume: Option<String>,
    pub session_dir: Option<PathBuf>,
    pub no_session: bool,
    pub extra_env: Vec<(String, String)>,
}

impl Default for SpawnOptions {
    fn default() -> Self {
        Self {
            cwd: PathBuf::from("."),
            omp_bin: PathBuf::from("omp"),
            model: None,
            resume: None,
            session_dir: None,
            no_session: false,
            extra_env: Vec::new(),
        }
    }
}

/// Everything the UI needs, pushed over the broadcast channel AND mirrored
/// verbatim to remote clients over WSS by cascaded.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionEvent {
    Ready { protocol_versions: Vec<u32> },
    TurnStarted,
    TextDelta { content_index: u32, delta: String },
    ThinkingDelta { content_index: u32, delta: String },
    MessageStart { role: String },
    MessageEnd { message: Value },
    ToolStart {
        tool_call_id: String,
        tool_name: String,
        args: Value,
        intent: Option<String>,
    },
    ToolUpdate {
        tool_call_id: String,
        partial: Value,
    },
    ToolEnd {
        tool_call_id: String,
        tool_name: String,
        is_error: bool,
        result: Value,
    },
    AgentEnd,
    TodoChanged { phases: Vec<TodoPhase> },
    UiRequest(UiRequest),
    UiRequestCancelled { target_id: String },
    Notice { level: String, message: String },
    SessionInfo { title: String, session_id: String },
    StateChanged,
    ProcessExited { code: Option<i32> },
    /// Full transcript/plan snapshot, sent by cascaded on WS attach.
    Snapshot(SessionSnapshot),
    Raw(Value),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UiRequest {
    pub id: String,
    pub method: UiMethod,
    pub title: Option<String>,
    pub message: Option<String>,
    pub options: Vec<String>,
    pub placeholder: Option<String>,
    pub prefill: Option<String>,
    pub url: Option<String>,
    pub timeout_secs: Option<u64>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UiMethod {
    Select,
    Confirm,
    Input,
    Editor,
    OpenUrl,
    Notify,
    SetStatus,
    SetWidget,
    SetTitle,
    Other,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UiAnswer {
    Value(String),
    Confirmed(bool),
    Cancelled,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TodoPhase {
    pub name: String,
    pub tasks: Vec<TodoItem>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TodoItem {
    pub content: String,
    pub status: TodoStatus,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
    Abandoned,
    Blocked,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SessionSnapshot {
    pub messages: Vec<Value>,
    pub todos: Vec<TodoPhase>,
    pub streaming: bool,
    pub pending_ui: Vec<UiRequest>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RpcSessionState {
    pub session_id: String,
    pub session_file: Option<String>,
    pub session_name: Option<String>,
    pub is_streaming: bool,
    pub is_compacting: bool,
    pub message_count: u32,
    pub todo_phases: Vec<TodoPhase>,
    pub model: Option<Value>,
    pub thinking_level: Option<String>,
}

struct SessionInner {
    id: String,
    rpc: RpcClient,
    events: broadcast::Sender<SessionEvent>,
    snapshot: Mutex<SessionSnapshot>,
    omp_session_id: Mutex<Option<String>>,
    child: Mutex<Option<Child>>,
    exited: AtomicBool,
}

/// One live omp process driven over `--mode rpc-ui` (protocol v2 negotiated).
#[derive(Clone)]
pub struct OmpSession {
    inner: Arc<SessionInner>,
}


/// A model in the catalog, provider split for display ("provider / model").
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModelInfo {
    pub provider: String,
    pub id: String,
    pub name: String,
}
impl OmpSession {
    pub async fn spawn(opts: SpawnOptions) -> Result<OmpSession> {
        let mut cmd = Command::new(&opts.omp_bin);
        cmd.arg("--mode")
            .arg("rpc-ui")
            .current_dir(&opts.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .env("PI_RPC_EMIT_TITLE", "1");
        if let Some(model) = &opts.model {
            cmd.arg("--model").arg(model);
        }

        if let Some(resume) = &opts.resume {
            cmd.arg("--resume").arg(resume);
        }

        if let Some(dir) = &opts.session_dir {
            cmd.arg("--session-dir").arg(dir);
        }
        if opts.no_session {
            cmd.arg("--no-session");
        }
        for (k, v) in &opts.extra_env {
            cmd.env(k, v);
        }

        let mut child = cmd.spawn().with_context(|| {
            format!("spawn {} --mode rpc-ui", opts.omp_bin.display())
        })?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("omp stdin not piped"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("omp stdout not piped"))?;

        let rpc = RpcClient::new(stdin);
        let v2_flag = Arc::new(AtomicBool::new(false));
        let (events, _) = broadcast::channel(BROADCAST_CAP);
        let (ready_tx, ready_rx) = oneshot::channel();
        let ready_tx = Arc::new(Mutex::new(Some(ready_tx)));
        let (frame_tx, mut frame_rx) = tokio::sync::mpsc::unbounded_channel::<Value>();

        let v2_for_reader = v2_flag.clone();
        tokio::spawn(async move {
            rpc::read_frames(stdout, v2_for_reader, |frame| {
                let _ = frame_tx.send(frame);
            })
            .await;
        });

        let id = Uuid::new_v4().to_string();
        let inner = Arc::new(SessionInner {
            id: id.clone(),
            rpc: rpc.clone(),
            events: events.clone(),
            snapshot: Mutex::new(SessionSnapshot::default()),
            omp_session_id: Mutex::new(None),
            child: Mutex::new(Some(child)),
            exited: AtomicBool::new(false),
        });
        let session = OmpSession {
            inner: inner.clone(),
        };

        let ready_slot = ready_tx.clone();
        let dispatch_rpc = rpc.clone();
        let sess = session.clone();
        tokio::spawn(async move {
            while let Some(frame) = frame_rx.recv().await {
                if let Some(info) = rpc::parse_ready(&frame) {
                    if let Some(tx) = ready_slot.lock().await.take() {
                        let _ = tx.send(info);
                    }
                }
                if let Some(event_frame) = dispatch_rpc.dispatch_incoming(frame).await {
                    sess.handle_frame(event_frame).await;
                }
            }
            sess.emit_exit(None).await;
        });

        let wait_inner = inner.clone();
        tokio::spawn(async move {
            loop {
                let status = {
                    let mut guard = wait_inner.child.lock().await;
                    match guard.as_mut() {
                        None => return,
                        Some(child) => match child.try_wait() {
                            Ok(Some(st)) => {
                                *guard = None;
                                Some(st.code())
                            }
                            Ok(None) => None,
                            Err(_) => {
                                *guard = None;
                                Some(None)
                            }
                        },
                    }
                };
                if let Some(code) = status {
                    OmpSession {
                        inner: wait_inner.clone(),
                    }
                    .emit_exit(code)
                    .await;
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        });

        let ready = rpc::wait_ready(ready_rx).await?;
        rpc.enable_v2(ready.max_frame_bytes);
        v2_flag.store(true, Ordering::SeqCst);

        rpc.command(json!({
            "type": "negotiate_protocol",
            "protocolVersion": 2,
        }))
        .await
        .context("negotiate_protocol v2")?;

        Ok(session)
    }

    pub fn id(&self) -> &str {
        &self.inner.id
    }

    pub async fn omp_session_id(&self) -> Option<String> {
        self.inner.omp_session_id.lock().await.clone()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<SessionEvent> {
        self.inner.events.subscribe()
    }

    pub async fn snapshot(&self) -> SessionSnapshot {
        self.inner.snapshot.lock().await.clone()
    }

    pub async fn prompt(&self, message: String) -> Result<()> {
        self.inner
            .rpc
            .command(json!({ "type": "prompt", "message": message }))
            .await?;
        Ok(())
    }

    pub async fn steer(&self, message: String) -> Result<()> {
        self.inner
            .rpc
            .command(json!({ "type": "steer", "message": message }))
            .await?;
        Ok(())
    }

    pub async fn abort(&self) -> Result<()> {
        self.inner.rpc.command(json!({ "type": "abort" })).await?;
        Ok(())
    }

    pub async fn new_session(&self) -> Result<()> {
        self.inner
            .rpc
            .command(json!({ "type": "new_session" }))
            .await?;
        let mut snap = self.inner.snapshot.lock().await;
        *snap = SessionSnapshot::default();
        Ok(())
    }

    pub async fn answer_ui(&self, request_id: String, response: UiAnswer) -> Result<()> {
        {
            let mut snap = self.inner.snapshot.lock().await;
            snap.pending_ui.retain(|r| r.id != request_id);
        }
        let mut body = json!({
            "type": "extension_ui_response",
            "id": request_id,
        });
        match response {
            UiAnswer::Value(v) => body["value"] = json!(v),
            UiAnswer::Confirmed(c) => body["confirmed"] = json!(c),
            UiAnswer::Cancelled => body["cancelled"] = json!(true),
        }
        self.inner.rpc.send_raw(body).await
    }

    pub async fn set_model(&self, provider: String, model_id: String) -> Result<()> {
        self.inner
            .rpc
            .command(json!({
                "type": "set_model",
                "provider": provider,
                "modelId": model_id,
            }))
            .await?;
        Ok(())
    }

    pub async fn set_thinking_level(&self, level: String) -> Result<()> {
        self.inner
            .rpc
            .command(json!({
                "type": "set_thinking_level",
                "level": level,
            }))
            .await?;
        Ok(())
    }

    /// Model catalog for the model-switch menu (omp `get_available_models`).
    pub async fn available_models(&self) -> Result<Vec<ModelInfo>> {
        let resp = self
            .inner
            .rpc
            .command(json!({ "type": "get_available_models" }))
            .await?;
        let data = resp.data.unwrap_or(Value::Null);
        let models = data
            .get("models")
            .and_then(|m| m.as_array())
            .cloned()
            .unwrap_or_default();
        Ok(models
            .iter()
            .filter_map(|m| {
                let provider = m.get("provider")?.as_str()?.to_string();
                let id = m.get("id")?.as_str()?.to_string();
                let name = m
                    .get("name")
                    .and_then(|n| n.as_str())
                    .unwrap_or(&id)
                    .to_string();
                Some(ModelInfo { provider, id, name })
            })
            .collect())
    }

    pub async fn get_state(&self) -> Result<RpcSessionState> {
        let resp = self
            .inner
            .rpc
            .command(json!({ "type": "get_state" }))
            .await?;
        let data = resp.data.unwrap_or(Value::Null);
        let state: RpcSessionState = serde_json::from_value(data).unwrap_or_default();
        *self.inner.omp_session_id.lock().await = if state.session_id.is_empty() {
            None
        } else {
            Some(state.session_id.clone())
        };
        {
            let mut snap = self.inner.snapshot.lock().await;
            snap.streaming = state.is_streaming;
            if !state.todo_phases.is_empty() {
                snap.todos = state.todo_phases.clone();
            }
        }
        Ok(state)
    }

    pub async fn shutdown(&self) -> Result<()> {
        {
            let mut guard = self.inner.child.lock().await;
            if let Some(child) = guard.as_mut() {
                let _ = child.kill().await;
                let status = child.wait().await.ok();
                *guard = None;
                drop(guard);
                self.emit_exit(status.and_then(|s| s.code())).await;
            }
        }
        Ok(())
    }

    async fn emit_exit(&self, code: Option<i32>) {
        if self.inner.exited.swap(true, Ordering::SeqCst) {
            return;
        }
        let _ = self
            .inner
            .events
            .send(SessionEvent::ProcessExited { code });
    }

    async fn handle_frame(&self, frame: Value) {
        let events = map_frame(&frame);
        for ev in events {
            self.apply_event(&ev).await;
            let _ = self.inner.events.send(ev);
        }
    }

    async fn apply_event(&self, ev: &SessionEvent) {
        let mut snap = self.inner.snapshot.lock().await;
        match ev {
            SessionEvent::TurnStarted => snap.streaming = true,
            SessionEvent::AgentEnd => snap.streaming = false,
            SessionEvent::MessageEnd { message } => snap.messages.push(message.clone()),
            SessionEvent::TodoChanged { phases } => snap.todos = phases.clone(),
            SessionEvent::UiRequest(req) => {
                snap.pending_ui.retain(|r| r.id != req.id);
                snap.pending_ui.push(req.clone());
            }
            SessionEvent::UiRequestCancelled { target_id } => {
                snap.pending_ui.retain(|r| r.id != *target_id);
            }
            SessionEvent::SessionInfo { session_id, .. } => {
                drop(snap);
                *self.inner.omp_session_id.lock().await = Some(session_id.clone());
            }
            _ => {}
        }
    }
}

fn map_frame(frame: &Value) -> Vec<SessionEvent> {
    let ty = frame.get("type").and_then(|t| t.as_str()).unwrap_or("");
    match ty {
        "ready" => {
            let versions = frame
                .get("supportedProtocolVersions")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_u64().map(|n| n as u32))
                        .collect()
                })
                .unwrap_or_default();
            vec![SessionEvent::Ready {
                protocol_versions: versions,
            }]
        }
        "agent_start" => vec![SessionEvent::TurnStarted],
        "agent_end" => vec![SessionEvent::AgentEnd],
        "message_start" => {
            let role = frame
                .get("message")
                .and_then(|m| m.get("role"))
                .and_then(|r| r.as_str())
                .unwrap_or("")
                .to_string();
            vec![SessionEvent::MessageStart { role }]
        }
        "message_end" => {
            let message = frame.get("message").cloned().unwrap_or(Value::Null);
            vec![SessionEvent::MessageEnd { message }]
        }
        "message_update" => map_message_update(frame),
        "tool_execution_start" => vec![SessionEvent::ToolStart {
            tool_call_id: js_str(frame, "toolCallId"),
            tool_name: js_str(frame, "toolName"),
            args: frame.get("args").cloned().unwrap_or(Value::Null),
            intent: frame
                .get("intent")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
        }],
        "tool_execution_update" => vec![SessionEvent::ToolUpdate {
            tool_call_id: js_str(frame, "toolCallId"),
            partial: frame
                .get("partialResult")
                .cloned()
                .unwrap_or(Value::Null),
        }],
        "tool_execution_end" => map_tool_end(frame),
        "extension_ui_request" => map_ui_request(frame),
        "session_info_update" => vec![SessionEvent::SessionInfo {
            title: js_str(frame, "title"),
            session_id: js_str(frame, "sessionId"),
        }],
        "notice" => vec![SessionEvent::Notice {
            level: js_str(frame, "level"),
            message: js_str(frame, "message"),
        }],
        "model_changed" | "thinking_level_changed" | "config_update" => {
            vec![SessionEvent::StateChanged]
        }
        _ => vec![SessionEvent::Raw(frame.clone())],
    }
}

fn map_message_update(frame: &Value) -> Vec<SessionEvent> {
    let ev = match frame.get("assistantMessageEvent") {
        Some(v) => v,
        None => return vec![SessionEvent::Raw(frame.clone())],
    };
    let ev_ty = ev.get("type").and_then(|t| t.as_str()).unwrap_or("");
    let content_index = ev
        .get("contentIndex")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as u32;
    match ev_ty {
        "text_delta" => vec![SessionEvent::TextDelta {
            content_index,
            delta: js_str(ev, "delta"),
        }],
        "thinking_delta" => vec![SessionEvent::ThinkingDelta {
            content_index,
            delta: js_str(ev, "delta"),
        }],
        _ => vec![SessionEvent::Raw(frame.clone())],
    }
}

fn map_tool_end(frame: &Value) -> Vec<SessionEvent> {
    let tool_name = js_str(frame, "toolName");
    let result = frame.get("result").cloned().unwrap_or(Value::Null);
    let is_error = frame
        .get("isError")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let mut out = vec![SessionEvent::ToolEnd {
        tool_call_id: js_str(frame, "toolCallId"),
        tool_name: tool_name.clone(),
        is_error,
        result: result.clone(),
    }];
    if tool_name == "todo" {
        if let Some(phases) = extract_todo_phases(&result) {
            out.push(SessionEvent::TodoChanged { phases });
        }
    }
    out
}

fn extract_todo_phases(result: &Value) -> Option<Vec<TodoPhase>> {
    let phases = result
        .get("details")
        .and_then(|d| d.get("phases"))
        .or_else(|| result.get("phases"))?;
    serde_json::from_value(phases.clone()).ok()
}

fn map_ui_request(frame: &Value) -> Vec<SessionEvent> {
    let method = frame.get("method").and_then(|m| m.as_str()).unwrap_or("");
    if method == "cancel" {
        let target_id = frame
            .get("targetId")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        return vec![SessionEvent::UiRequestCancelled { target_id }];
    }
    let options = frame
        .get("options")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    let timeout_secs = frame.get("timeout").and_then(|v| v.as_u64());
    vec![SessionEvent::UiRequest(UiRequest {
        id: js_str(frame, "id"),
        method: parse_ui_method(method),
        title: opt_str(frame, "title"),
        message: opt_str(frame, "message"),
        options,
        placeholder: opt_str(frame, "placeholder"),
        prefill: opt_str(frame, "prefill"),
        url: opt_str(frame, "url"),
        timeout_secs,
    })]
}

fn parse_ui_method(m: &str) -> UiMethod {
    match m {
        "select" => UiMethod::Select,
        "confirm" => UiMethod::Confirm,
        "input" => UiMethod::Input,
        "editor" => UiMethod::Editor,
        "open_url" => UiMethod::OpenUrl,
        "notify" => UiMethod::Notify,
        "setStatus" | "set_status" => UiMethod::SetStatus,
        "setWidget" | "set_widget" => UiMethod::SetWidget,
        "setTitle" | "set_title" => UiMethod::SetTitle,
        _ => UiMethod::Other,
    }
}

fn js_str(v: &Value, k: &str) -> String {
    v.get(k)
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string()
}

fn opt_str(v: &Value, k: &str) -> Option<String> {
    v.get(k).and_then(|x| x.as_str()).map(|s| s.to_string())
}

/// Owns N OmpSessions. Used by cascaded (both roles) and cascade-gtk (local).
#[derive(Clone)]
pub struct SessionManager {
    registry: SessionRegistry,
    sessions: Arc<Mutex<HashMap<String, OmpSession>>>,
}

impl SessionManager {
    pub fn new(registry: SessionRegistry) -> Self {
        Self {
            registry,
            sessions: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn spawn(&self, opts: SpawnOptions) -> Result<String> {
        let cwd = opts.cwd.display().to_string();
        let session = OmpSession::spawn(opts).await?;
        let id = session.id().to_string();
        let now = Utc::now();
        let mut meta = SessionMeta {
            id: id.clone(),
            omp_session_id: None,
            name: None,
            cwd,
            session_file: None,
            machine: local_machine(),
            created_at: now,
            last_active: now,
            kind: "managed".into(),
            join_handle: None,
            view_handle: None,
            pid: None,
        };
        if let Ok(state) = session.get_state().await {
            meta.omp_session_id = if state.session_id.is_empty() {
                None
            } else {
                Some(state.session_id)
            };
            meta.session_file = state.session_file;
            meta.name = state.session_name;
        }
        let registry = self.registry.clone();
        let meta_for_db = meta.clone();
        tokio::task::spawn_blocking(move || registry.upsert(&meta_for_db))
            .await
            .context("join upsert")??;

        let mut rx = session.subscribe();
        let registry = self.registry.clone();
        let touch_id = id.clone();
        tokio::spawn(async move {
            while rx.recv().await.is_ok() {
                let r = registry.clone();
                let tid = touch_id.clone();
                let _ = tokio::task::spawn_blocking(move || r.touch(&tid)).await;
            }
        });

        self.sessions.lock().await.insert(id.clone(), session);
        Ok(id)
    }

    pub async fn get(&self, id: &str) -> Option<OmpSession> {
        self.sessions.lock().await.get(id).cloned()
    }

    pub async fn list(&self) -> Vec<SessionMeta> {
        let registry = self.registry.clone();
        tokio::task::spawn_blocking(move || registry.list().unwrap_or_default())
            .await
            .unwrap_or_default()
    }

    pub async fn shutdown(&self, id: &str) -> Result<()> {
        let session = self.sessions.lock().await.remove(id);
        if let Some(s) = session {
            s.shutdown().await?;
        }
        Ok(())
    }

    pub async fn shutdown_all(&self) {
        let mut map = self.sessions.lock().await;
        let sessions: Vec<_> = map.drain().map(|(_, s)| s).collect();
        drop(map);
        for s in sessions {
            let _ = s.shutdown().await;
        }
    }
}

fn local_machine() -> String {
    std::env::var("HOSTNAME").unwrap_or_else(|_| "local".into())
}
