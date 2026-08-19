//! Map collab host frames onto `cascade_core::SessionEvent`.

use cascade_core::{SessionEvent, UiMethod, UiRequest};
use serde_json::{json, Value};
use tokio::sync::{broadcast, mpsc};
use tracing::debug;

use crate::link::parse_collab_link;
use crate::socket::{CollabSocket, Outgoing, SocketEvent};

const BROADCAST_CAP: usize = 1024;

#[derive(Debug, Clone)]
pub enum GuestCommand {
    Prompt { text: String },
    Abort,
}

/// Native collab GUEST so GTK can treat a live room as a third backend (no PTY).
pub struct CollabAttach;

impl CollabAttach {
    pub async fn connect(
        link: &str,
    ) -> anyhow::Result<(broadcast::Receiver<SessionEvent>, mpsc::Sender<GuestCommand>)> {
        let parsed = parse_collab_link(link)?;
        let name = std::env::var("CASCADE_COLLAB_NAME")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| whoami::username());

        let mut sock = CollabSocket::connect_guest(parsed, name);
        let send_frames = sock.send.clone();
        let (ev_tx, ev_rx) = broadcast::channel(BROADCAST_CAP);
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<GuestCommand>(32);

        // Wait until the socket reports Open or a fatal close so callers fail fast on 4004/4009.
        loop {
            match sock.events.recv().await {
                Some(SocketEvent::Open) => break,
                Some(SocketEvent::Closed {
                    reason,
                    will_reconnect,
                }) => {
                    if !will_reconnect {
                        anyhow::bail!("collab connect failed: {reason}");
                    }
                }
                None => anyhow::bail!("collab socket ended before open"),
                _ => {}
            }
        }

        let mut mapper = FrameMapper::default();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    cmd = cmd_rx.recv() => {
                        let Some(cmd) = cmd else { break; };
                        let frame = match cmd {
                            GuestCommand::Prompt { text } => json!({"t":"prompt","text": text}),
                            GuestCommand::Abort => json!({"t":"abort"}),
                        };
                        let _ = send_frames.send(Outgoing { frame, target_peer: 0 });
                    }
                    ev = sock.events.recv() => {
                        let Some(ev) = ev else { break; };
                        match ev {
                            SocketEvent::Open => {
                                // reconnect: hello is sent by the socket; mapper stays.
                                let _ = ev_tx.send(SessionEvent::Notice {
                                    level: "info".into(),
                                    message: "collab reconnected".into(),
                                });
                            }
                            SocketEvent::Frame { json, .. } => {
                                for e in mapper.map_frame(&json) {
                                    let _ = ev_tx.send(e);
                                }
                            }
                            SocketEvent::Control(_) => {}
                            SocketEvent::Closed { reason, will_reconnect } => {
                                let _ = ev_tx.send(SessionEvent::Notice {
                                    level: if will_reconnect { "warning" } else { "error" }.into(),
                                    message: format!("collab: {reason}"),
                                });
                                if !will_reconnect {
                                    let _ = ev_tx.send(SessionEvent::ProcessExited { code: None });
                                    break;
                                }
                            }
                        }
                    }
                }
            }
        });

        Ok((ev_rx, cmd_tx))
    }
}

#[derive(Default)]
struct FrameMapper {
    last_text: String,
    last_thinking: String,
}

impl FrameMapper {
    fn map_frame(&mut self, frame: &Value) -> Vec<SessionEvent> {
        let t = frame.get("t").and_then(|v| v.as_str()).unwrap_or("");
        match t {
            "event" => frame
                .get("event")
                .map(|ev| self.map_agent_event(ev))
                .unwrap_or_default(),
            "entry" => frame
                .get("entry")
                .map(|e| vec![self.map_entry(e)])
                .unwrap_or_default(),
            "state" => vec![SessionEvent::StateChanged],
            "welcome" => {
                self.last_text.clear();
                self.last_thinking.clear();
                let mut out = Vec::new();
                if let Some(header) = frame.get("header") {
                    let title = header
                        .get("title")
                        .and_then(|v| v.as_str())
                        .unwrap_or("collab")
                        .to_string();
                    let session_id = header
                        .get("id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    out.push(SessionEvent::SessionInfo { title, session_id });
                }
                out.push(SessionEvent::Ready {
                    protocol_versions: vec![crate::protocol::COLLAB_PROTO],
                });
                out.push(SessionEvent::StateChanged);
                out
            }
            "snapshot-chunk" => {
                let mut out = Vec::new();
                if let Some(entries) = frame.get("entries").and_then(|v| v.as_array()) {
                    for e in entries {
                        out.push(self.map_entry(e));
                    }
                }
                out
            }
            "ui-request" => {
                if let Some(req) = frame.get("request") {
                    vec![SessionEvent::UiRequest(map_ui_request(req))]
                } else {
                    vec![SessionEvent::Raw(frame.clone())]
                }
            }
            "ui-request-end" => {
                let id = frame
                    .get("reqId")
                    .map(|v| v.to_string())
                    .unwrap_or_default();
                vec![SessionEvent::UiRequestCancelled { target_id: id }]
            }
            "error" => {
                let message = frame
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("collab error")
                    .to_string();
                vec![SessionEvent::Notice {
                    level: "error".into(),
                    message,
                }]
            }
            "bye" => {
                let message = frame
                    .get("reason")
                    .and_then(|v| v.as_str())
                    .unwrap_or("bye")
                    .to_string();
                vec![
                    SessionEvent::Notice {
                        level: "info".into(),
                        message,
                    },
                    SessionEvent::ProcessExited { code: None },
                ]
            }
            "agents" | "bus" | "transcript" => vec![SessionEvent::Raw(frame.clone())],
            _ => {
                debug!(t, "unmapped collab frame");
                vec![SessionEvent::Raw(frame.clone())]
            }
        }
    }

    fn map_agent_event(&mut self, ev: &Value) -> Vec<SessionEvent> {
        let ty = ev.get("type").and_then(|v| v.as_str()).unwrap_or("");
        match ty {
            "turn_start" => vec![SessionEvent::TurnStarted],
            "agent_end" => vec![SessionEvent::AgentEnd],
            "message_start" => {
                self.last_text.clear();
                self.last_thinking.clear();
                let role = ev
                    .get("message")
                    .and_then(|m| m.get("role"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("assistant")
                    .to_string();
                vec![SessionEvent::MessageStart { role }]
            }
            "message_update" => self.map_message_update(ev.get("message")),
            "message_end" => {
                self.last_text.clear();
                self.last_thinking.clear();
                let message = ev.get("message").cloned().unwrap_or(Value::Null);
                vec![SessionEvent::MessageEnd { message }]
            }
            "tool_execution_start" => vec![SessionEvent::ToolStart {
                tool_call_id: str_field(ev, "toolCallId"),
                tool_name: str_field(ev, "toolName"),
                args: ev.get("args").cloned().unwrap_or(Value::Null),
                intent: ev
                    .get("intent")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
            }],
            "tool_execution_update" => vec![SessionEvent::ToolUpdate {
                tool_call_id: str_field(ev, "toolCallId"),
                partial: ev.get("partialResult").cloned().unwrap_or(Value::Null),
            }],
            "tool_execution_end" => vec![SessionEvent::ToolEnd {
                tool_call_id: str_field(ev, "toolCallId"),
                tool_name: str_field(ev, "toolName"),
                is_error: ev.get("isError").and_then(|v| v.as_bool()).unwrap_or(false),
                result: ev.get("result").cloned().unwrap_or(Value::Null),
            }],
            "notice" => vec![SessionEvent::Notice {
                level: str_field(ev, "level"),
                message: str_field(ev, "message"),
            }],
            "agent_start" => vec![SessionEvent::TurnStarted],
            _ => vec![SessionEvent::Raw(json!({"t":"event","event": ev}))],
        }
    }

    fn map_message_update(&mut self, message: Option<&Value>) -> Vec<SessionEvent> {
        let Some(message) = message else {
            return Vec::new();
        };
        let mut out = Vec::new();
        if let Some(content) = message.get("content") {
            if let Some(s) = content.as_str() {
                if let Some(delta) = suffix_delta(&self.last_text, s) {
                    if !delta.is_empty() {
                        out.push(SessionEvent::TextDelta {
                            content_index: 0,
                            delta,
                        });
                    }
                }
                self.last_text = s.to_string();
            } else if let Some(arr) = content.as_array() {
                for (i, block) in arr.iter().enumerate() {
                    let ty = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
                    match ty {
                        "text" => {
                            let s = block.get("text").and_then(|v| v.as_str()).unwrap_or("");
                            if let Some(delta) = suffix_delta(&self.last_text, s) {
                                if !delta.is_empty() {
                                    out.push(SessionEvent::TextDelta {
                                        content_index: i as u32,
                                        delta,
                                    });
                                }
                            }
                            self.last_text = s.to_string();
                        }
                        "thinking" => {
                            let s = block.get("thinking").and_then(|v| v.as_str()).unwrap_or("");
                            if let Some(delta) = suffix_delta(&self.last_thinking, s) {
                                if !delta.is_empty() {
                                    out.push(SessionEvent::ThinkingDelta {
                                        content_index: i as u32,
                                        delta,
                                    });
                                }
                            }
                            self.last_thinking = s.to_string();
                        }
                        _ => {}
                    }
                }
            }
        }
        if out.is_empty() {
            out.push(SessionEvent::Raw(json!({"t":"event","event":{"type":"message_update","message": message}})));
        }
        out
    }

    fn map_entry(&self, entry: &Value) -> SessionEvent {
        match entry.get("type").and_then(|v| v.as_str()) {
            Some("message") => {
                if let Some(message) = entry.get("message") {
                    SessionEvent::MessageEnd {
                        message: message.clone(),
                    }
                } else {
                    SessionEvent::Raw(entry.clone())
                }
            }
            _ => SessionEvent::Raw(json!({"t":"entry","entry": entry})),
        }
    }
}

fn str_field(v: &Value, k: &str) -> String {
    v.get(k)
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string()
}

fn suffix_delta(prev: &str, full: &str) -> Option<String> {
    if let Some(rest) = full.strip_prefix(prev) {
        Some(rest.to_string())
    } else if full == prev {
        Some(String::new())
    } else {
        Some(full.to_string())
    }
}

fn map_ui_request(req: &Value) -> UiRequest {
    let id = req
        .get("reqId")
        .map(|v| v.to_string())
        .unwrap_or_else(|| "0".into());
    let kind = req.get("kind").and_then(|v| v.as_str()).unwrap_or("");
    let method = match kind {
        "select" => UiMethod::Select,
        "editor" => UiMethod::Editor,
        "confirm" => UiMethod::Confirm,
        "input" => UiMethod::Input,
        _ => UiMethod::Other,
    };
    let options = req
        .get("options")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|o| {
                    o.as_str()
                        .map(str::to_string)
                        .or_else(|| o.get("label").and_then(|l| l.as_str()).map(str::to_string))
                })
                .collect()
        })
        .unwrap_or_default();
    UiRequest {
        id,
        method,
        title: req
            .get("title")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        message: req
            .get("helpText")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        options,
        placeholder: None,
        prefill: req
            .get("prefill")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        url: None,
        timeout_secs: None,
    }
}
